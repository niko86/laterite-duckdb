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
//! once. That's what makes `load_ags_script` (one `read_ags` per group) and a
//! notebook's repeat queries pay the parse just once per file.

use std::ffi::CString;
use std::sync::Arc;

use laterite_ags4_core::ags4_codec::{ParsedAgs4, read_ags4_bytes};
use quack_rs::client_context::ClientContext;
use quack_rs::file_system::{FileHandle, FileOpenOptions, FileSystem};
use quack_rs::prelude::ExtensionError;

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
    // Cheap open + size first: the size is the cache key, so a hit skips the
    // slurp + parse in the closure below entirely.
    let (handle, size) = open_for_read(ctx, path)?;
    super::cache::get_or_try_insert(path, size, || {
        let buf = slurp(&handle, path, size)?;
        read_ags4_bytes(&buf).map_err(|e| {
            ExtensionError::new(format!(
                "read_ags: '{path}' did not parse as AGS4 ({e}); the file may be invalid — validate/repair it first"
            ))
        })
    })
}

/// Read the *raw bytes* of `path` through the VFS — the unparsed, **uncached**
/// slurp behind the certificate path. `certify_ags` needs the exact source bytes
/// to hash + index, and `validate_ags`'s cert fast-path needs them to confirm the
/// SHA; both want the bytes themselves, not the parsed form (and a cert mint /
/// one-shot verdict isn't a hot repeat-read, so it doesn't earn a cache slot).
pub fn read_bytes(ctx: &ClientContext, path: &str) -> Result<Vec<u8>, ExtensionError> {
    let (handle, size) = open_for_read(ctx, path)?;
    slurp(&handle, path, size)
}

/// Write `data` to `path` through the VFS (create/truncate). The `.ags.idx`
/// certificate is written this way so it lands wherever the source did — beside a
/// local file, or via a writable VFS — rather than only ever on local disk.
pub fn write_bytes(ctx: &ClientContext, path: &str, data: &[u8]) -> Result<(), ExtensionError> {
    let fs = FileSystem::from_client_context(ctx).ok_or_else(|| {
        ExtensionError::new(
            "certify_ags: DuckDB did not provide a filesystem (requires DuckDB 1.5+)",
        )
    })?;
    let c_path = CString::new(path).map_err(|_| {
        ExtensionError::new(format!(
            "certify_ags: path '{path}' contains an interior NUL byte"
        ))
    })?;
    let handle = fs
        .open(&c_path, &FileOpenOptions::write_create())
        .map_err(|e| {
            ExtensionError::new(format!(
                "certify_ags: cannot open '{path}' for writing: {}",
                e.message().unwrap_or_else(|| "unknown error".into())
            ))
        })?;
    // `write` may make partial progress; loop until every byte is durably handed
    // to the VFS, then sync so the sidecar is on disk before we report success.
    let mut off = 0usize;
    while off < data.len() {
        let n = handle.write(&data[off..]).map_err(|e| {
            ExtensionError::new(format!(
                "certify_ags: write error on '{path}': {}",
                e.message().unwrap_or_else(|| "unknown error".into())
            ))
        })?;
        if n == 0 {
            return Err(ExtensionError::new(format!(
                "certify_ags: write to '{path}' stalled at {off} of {} bytes",
                data.len()
            )));
        }
        off += n;
    }
    handle.sync().map_err(|e| {
        ExtensionError::new(format!(
            "certify_ags: sync failed on '{path}': {}",
            e.message().unwrap_or_else(|| "unknown error".into())
        ))
    })
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
