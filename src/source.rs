//! Resolve a `path` argument to a parsed AGS4 file — the one seam every table
//! function's bind goes through.
//!
//! Every read goes through **DuckDB's virtual filesystem** ([`FileSystem`],
//! obtained from the bind's [`ClientContext`]), so a single code path serves
//! local paths, `http(s)://`, and `s3://` (with `LOAD httpfs`) and honours the
//! host's secrets/httpfs config — rather than reaching for `std::fs` and only
//! ever seeing local disk. Raw `.ags` has no index/footer, so a full read is
//! inherent to the format.
//!
//! The parse is memoised by [`super::cache`]: this resolver does the cheap
//! `open` + `size()` (a `stat` locally, a `HEAD` remotely) on every call, then
//! hands the `(path, size)` to the cache. A hit returns the shared
//! `Arc<ParsedAgs4>` *without reading or parsing the file*; a miss slurps + parses
//! once. That's what makes `load_ags` (one `read_ags` per group) and a
//! notebook's repeat queries pay the parse just once per file.

use std::ffi::CString;
use std::sync::Arc;

use laterite_ags4_core::ags4_codec::{ParsedAgs4, read_ags4_bytes};
use quack_rs::client_context::ClientContext;
use quack_rs::file_system::{FileHandle, FileOpenOptions, FileSystem};
use quack_rs::prelude::{BindInfo, ExtensionError};

/// Default ceiling on one file's reported/read size — generous for any real AGS
/// delivery (these run MB to low-GB), low enough to bound a hostile remote
/// `Content-Length` to a controlled, finite read. Override via
/// `LATERITE_AGS_MAX_FILE_BYTES`.
const DEFAULT_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB
/// Cap on the *up-front* buffer reserve so an over-reported size can't force a
/// huge allocation before a single byte arrives; the buffer grows from here as
/// bytes actually come in.
const INITIAL_RESERVE_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB
/// Read granularity for the incremental slurp.
const READ_CHUNK_BYTES: usize = 1024 * 1024; // 1 MiB

/// The per-file size ceiling, env-overridable.
fn max_file_bytes() -> u64 {
    std::env::var("LATERITE_AGS_MAX_FILE_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_FILE_BYTES)
}

/// Read + parse the AGS4 file at `path` through the DuckDB VFS bound to `ctx`,
/// memoised by `(path, size)`. Structural malformation surfaces here (the parser
/// must structurally parse to produce rows at all) with a message pointing at
/// validation/repair — the "assume valid, else say so" contract.
pub fn read_parsed(ctx: &ClientContext, path: &str) -> Result<Arc<ParsedAgs4>, ExtensionError> {
    read_parsed_with_encoding(ctx, path, encoding_rs::UTF_8)
}

/// As [`read_parsed`], but decoding the source bytes with `encoding` (the
/// `read_ags(..., encoding := 'windows-1252')` path, #294 #12). The core codec is
/// UTF-8-only, so a non-UTF-8 file is decoded to UTF-8 first (`encoding_rs` lossy —
/// undefined bytes → U+FFFD); UTF-8 keeps the exact original bytes (byte-identical
/// to the pre-#12 path). `encoding.name()` joins the cache key so a UTF-8 read and
/// a windows-1252 read of the same file memoise separately.
pub fn read_parsed_with_encoding(
    ctx: &ClientContext,
    path: &str,
    encoding: &'static encoding_rs::Encoding,
) -> Result<Arc<ParsedAgs4>, ExtensionError> {
    // Cheap open + size first: the size is part of the cache key, so a hit skips
    // the slurp + parse in the closure below entirely.
    let (handle, size) = open_for_read(ctx, path)?;
    super::cache::get_or_try_insert(path, size, encoding.name(), || {
        let buf = slurp(&handle, path, size)?;
        let parsed = if encoding == encoding_rs::UTF_8 {
            read_ags4_bytes(&buf)
        } else {
            let (text, _, _) = encoding.decode(&buf);
            read_ags4_bytes(text.as_bytes())
        };
        parsed.map_err(|e| {
            ExtensionError::new(format!(
                "read_ags: '{path}' did not parse as AGS4 ({e}); the file may be invalid — validate/repair it first"
            ))
        })
    })
}

/// Resolve the optional `encoding` named param — a WHATWG label like `'utf-8'` /
/// `'windows-1252'` — to its `&'static Encoding`; absent or blank → UTF-8, an
/// unrecognised label is a clear bind error. Used by `read_ags` (#294 #12).
pub fn resolve_encoding(info: &BindInfo) -> Result<&'static encoding_rs::Encoding, ExtensionError> {
    match unsafe { info.get_named_parameter_value("encoding") }.as_str() {
        Ok(label) if !label.trim().is_empty() => {
            let label = label.trim();
            encoding_rs::Encoding::for_label(label.as_bytes()).ok_or_else(|| {
                ExtensionError::new(format!(
                    "unknown encoding '{label}'; expected a WHATWG label like 'utf-8' or 'windows-1252'"
                ))
            })
        }
        _ => Ok(encoding_rs::UTF_8),
    }
}

/// Read the *raw bytes* of `path` through the VFS — the unparsed, **uncached**
/// slurp behind the certificate path. Reading a sibling `.ags.idx` certificate
/// wants the bytes themselves, not the parsed form (and a one-shot sidecar read
/// isn't a hot repeat-read, so it doesn't earn a cache slot).
pub fn read_bytes(ctx: &ClientContext, path: &str) -> Result<Vec<u8>, ExtensionError> {
    let (handle, size) = open_for_read(ctx, path)?;
    slurp(&handle, path, size)
}

/// Open `path` read-only through the VFS and resolve its size — the cheap
/// `open` + `size()` (a `stat` locally, a `HEAD` remotely) every read does before
/// committing to a slurp. A negative size is a VFS error/unknown, treated as a
/// failure (not clamped to 0, which would mask it); an absurd reported size is
/// rejected up front so it can neither key the cache nor drive an allocation.
/// `pub(crate)` so the certificate slice path can reuse the same open + size +
/// ceiling guard, then `seek` the returned handle to one group's byte range.
pub(crate) fn open_for_read(
    ctx: &ClientContext,
    path: &str,
) -> Result<(FileHandle, u64), ExtensionError> {
    let fs = FileSystem::from_client_context(ctx).ok_or_else(|| {
        ExtensionError::new("read_ags: DuckDB did not provide a filesystem (requires DuckDB 1.5+)")
    })?;
    let c_path = CString::new(path).map_err(|_| {
        ExtensionError::new(format!(
            "read_ags: path '{path}' contains an interior NUL byte"
        ))
    })?;
    let handle = fs
        .open(&c_path, &FileOpenOptions::read_only())
        .map_err(|e| {
            ExtensionError::new(format!(
                "read_ags: cannot open '{path}': {}",
                e.message().unwrap_or_else(|| "unknown error".into())
            ))
        })?;
    let raw_size = handle.size();
    if raw_size < 0 {
        return Err(ExtensionError::new(format!(
            "read_ags: cannot determine the size of '{path}' (filesystem returned {raw_size})"
        )));
    }
    let size = raw_size as u64;
    let max = max_file_bytes();
    if size > max {
        return Err(ExtensionError::new(format!(
            "read_ags: '{path}' reports {size} bytes, over the {max}-byte limit (raise LATERITE_AGS_MAX_FILE_BYTES if this is a genuine file)"
        )));
    }
    Ok((handle, size))
}

/// Slurp the whole file from `handle` into a buffer, with the same adversarial
/// guards the cached parse relies on: grow from a bounded reserve (never trust the
/// reported size as an up-front allocation), re-check the running total against the
/// ceiling (a stream can deliver more than it claimed), and reject a short read
/// (fewer bytes than claimed → the file changed underfoot) rather than return a
/// silently-truncated buffer.
fn slurp(handle: &FileHandle, path: &str, size: u64) -> Result<Vec<u8>, ExtensionError> {
    let max = max_file_bytes();
    let mut buf: Vec<u8> = Vec::with_capacity(size.min(INITIAL_RESERVE_BYTES) as usize);
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        let got = handle.read(&mut chunk).map_err(|e| {
            ExtensionError::new(format!(
                "read_ags: read error on '{path}': {}",
                e.message().unwrap_or_else(|| "unknown error".into())
            ))
        })?;
        if got == 0 {
            break; // EOF
        }
        if buf.len() as u64 + got as u64 > max {
            return Err(ExtensionError::new(format!(
                "read_ags: '{path}' exceeds the {max}-byte limit while reading (raise LATERITE_AGS_MAX_FILE_BYTES if this is a genuine file)"
            )));
        }
        buf.extend_from_slice(&chunk[..got]);
    }
    if (buf.len() as u64) < size {
        return Err(ExtensionError::new(format!(
            "read_ags: short read on '{path}' (got {} of {size} bytes); the file may have changed underfoot — retry",
            buf.len()
        )));
    }
    Ok(buf)
}
