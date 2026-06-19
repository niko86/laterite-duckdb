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
use quack_rs::file_system::{FileOpenOptions, FileSystem};
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
    // `size()` is the file's total length (a HEAD for remote); it returns i64
    // with a negative value signalling error/unknown. Treat that as a failure
    // rather than clamping to 0 — clamping would parse + CACHE the file as empty
    // and mask the real VFS error under the (path, 0) key.
    let raw_size = handle.size();
    if raw_size < 0 {
        return Err(ExtensionError::new(format!(
            "read_ags: cannot determine the size of '{path}' (filesystem returned {raw_size})"
        )));
    }
    let size = raw_size as u64;
    // The reported size is a HINT (a remote HEAD's `Content-Length` is the
    // server's word). Reject an absurd one up front so it can neither key the
    // cache nor drive an allocation.
    let max = max_file_bytes();
    if size > max {
        return Err(ExtensionError::new(format!(
            "read_ags: '{path}' reports {size} bytes, over the {max}-byte limit (raise LATERITE_AGS_MAX_FILE_BYTES if this is a genuine file)"
        )));
    }

    super::cache::get_or_try_insert(path, size, || {
        // Miss only: read the file incrementally into a growable buffer. We do
        // NOT pre-allocate the reported size — a hostile/over-reported size would
        // force a huge up-front allocation — and we re-check the running total
        // against the ceiling so a stream that delivers MORE than it claimed
        // can't OOM us either.
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
        // A short read (fewer bytes arrived than the file claimed — a remote
        // stream cut off, or the file shrank under us) must NOT be cached as the
        // whole file: the parser would accept the truncated bytes as fewer groups,
        // and that partial parse would be pinned under the full-size key with no
        // way to bust it. Erroring leaves nothing cached, so the next call retries.
        if (buf.len() as u64) < size {
            return Err(ExtensionError::new(format!(
                "read_ags: short read on '{path}' (got {} of {size} bytes); the file may have changed underfoot — retry",
                buf.len()
            )));
        }
        read_ags4_bytes(&buf).map_err(|e| {
            ExtensionError::new(format!(
                "read_ags: '{path}' did not parse as AGS4 ({e}); the file may be invalid — validate/repair it first"
            ))
        })
    })
}
