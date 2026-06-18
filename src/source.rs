//! Resolve a `path` argument to a parsed AGS4 file — the one seam every table
//! function's bind goes through.
//!
//! Every read goes through **DuckDB's virtual filesystem** ([`FileSystem`],
//! obtained from the bind's [`ClientContext`]), so a single code path serves
//! local paths, `http(s)://`, and `s3://` (with `LOAD httpfs`) and honours the
//! host's secrets/httpfs config — rather than reaching for `std::fs` and only
//! ever seeing local disk. Raw `.ags` has no index/footer, so a full read is
//! inherent to the format; remote *efficiency* comes from the optional
//! `load_ags_script` persist step, not the raw read.

use std::ffi::CString;

use laterite_ags4_core::ags4_codec::{ParsedAgs4, read_ags4_bytes};
use quack_rs::client_context::ClientContext;
use quack_rs::file_system::{FileOpenOptions, FileSystem};
use quack_rs::prelude::ExtensionError;

/// Read + parse the AGS4 file at `path` through the DuckDB VFS bound to `ctx`.
/// Structural malformation surfaces here (the parser must structurally parse to
/// produce rows at all) with a message pointing at validation/repair — the
/// "assume valid, else say so" contract.
pub fn read_parsed(ctx: &ClientContext, path: &str) -> Result<ParsedAgs4, ExtensionError> {
    let bytes = read_bytes(ctx, path)?;
    read_ags4_bytes(&bytes).map_err(|e| {
        ExtensionError::new(format!(
            "read_ags: '{path}' did not parse as AGS4 ({e}); the file may be invalid — validate/repair it first"
        ))
    })
}

/// Slurp the whole file through DuckDB's virtual filesystem.
fn read_bytes(ctx: &ClientContext, path: &str) -> Result<Vec<u8>, ExtensionError> {
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
    // `size()` is the file's total length (a HEAD for remote); a single `read`
    // may return short, so loop until the buffer is filled or we hit EOF.
    let size = handle.size().max(0) as usize;
    let mut buf = vec![0u8; size];
    let mut filled = 0;
    while filled < size {
        let n = handle.read(&mut buf[filled..]).map_err(|e| {
            ExtensionError::new(format!(
                "read_ags: read error on '{path}': {}",
                e.message().unwrap_or_else(|| "unknown error".into())
            ))
        })?;
        if n == 0 {
            break; // EOF before the reported size — truncate to what arrived.
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}
