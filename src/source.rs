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
    // `size()` is the file's total length (a HEAD for remote).
    let size = handle.size().max(0) as u64;

    super::cache::get_or_try_insert(path, size, || {
        // Miss only: slurp the whole file. A single `read` may return short, so
        // loop until the buffer is filled or we hit EOF.
        let n = size as usize;
        let mut buf = vec![0u8; n];
        let mut filled = 0;
        while filled < n {
            let got = handle.read(&mut buf[filled..]).map_err(|e| {
                ExtensionError::new(format!(
                    "read_ags: read error on '{path}': {}",
                    e.message().unwrap_or_else(|| "unknown error".into())
                ))
            })?;
            if got == 0 {
                break; // EOF before the reported size — truncate to what arrived.
            }
            filled += got;
        }
        buf.truncate(filled);
        read_ags4_bytes(&buf).map_err(|e| {
            ExtensionError::new(format!(
                "read_ags: '{path}' did not parse as AGS4 ({e}); the file may be invalid — validate/repair it first"
            ))
        })
    })
}
