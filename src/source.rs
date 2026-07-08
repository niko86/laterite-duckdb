//! Resolve a `path` argument to a parsed AGS4 file — the one seam a VFS-reading
//! bind goes through.
//!
//! Every read goes through **DuckDB's virtual filesystem**, obtained from the
//! bind's client context, so a single code path serves local paths,
//! `http(s)://`, and `s3://` (with `LOAD httpfs`) and honours the host's
//! secrets/httpfs config. Raw `.ags` has no index/footer, so a full read is
//! inherent to the format.
//!
//! Under quack-rs this was `ClientContext`/`FileSystem`/`FileHandle`; those
//! wrappers keep the raw pointers private, so here the VFS is driven directly
//! through `libduckdb-sys` (`duckdb_table_function_get_client_context` →
//! `duckdb_client_context_get_file_system` → `duckdb_file_system_open` /
//! `duckdb_file_handle_{size,seek,read}`). The RAII drops mirror quack-rs's
//! ownership exactly: the client context, file system, and file handle are each
//! caller-owned and destroyed on drop.
//!
//! The parse is memoised by [`super::cache`]: this resolver does the cheap
//! `open` + `size()` (a `stat` locally, a `HEAD` remotely) on every call, then
//! hands `(path, size)` to the cache. A hit returns the shared
//! `Arc<ParsedAgs4>` without reading or parsing.

use std::ffi::CString;
use std::os::raw::c_void;
use std::ptr;
use std::sync::Arc;

use laterite_ags4_core::ags4_codec::{ParsedAgs4, read_ags4_bytes};
use libduckdb_sys as ffi;

/// Default ceiling on one file's reported/read size — generous for any real AGS
/// delivery (MB to low-GB), low enough to bound a hostile remote
/// `Content-Length` to a finite read. Override via `LATERITE_AGS_MAX_FILE_BYTES`.
const DEFAULT_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB
/// Cap on the up-front buffer reserve so an over-reported size can't force a
/// huge allocation before a byte arrives; the buffer grows from here.
const INITIAL_RESERVE_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB
/// Read granularity for the incremental slurp.
const READ_CHUNK_BYTES: usize = 1024 * 1024; // 1 MiB

fn max_file_bytes() -> u64 {
    std::env::var("LATERITE_AGS_MAX_FILE_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_FILE_BYTES)
}

/// DuckDB's client-context filesystem for the current bind — the VFS the readers
/// go through. Owns the client context + file system handles; both are destroyed
/// on drop (matching quack-rs's `ClientContext`/`FileSystem` RAII).
pub struct Vfs {
    ctx: ffi::duckdb_client_context,
    fs: ffi::duckdb_file_system,
}

impl Vfs {
    /// Obtain the VFS from a table function's raw bind info.
    ///
    /// # Safety
    /// `info` must be the live `duckdb_bind_info` for the current bind call.
    pub unsafe fn from_bind(info: ffi::duckdb_bind_info) -> Result<Self, String> {
        unsafe {
            let mut ctx: ffi::duckdb_client_context = ptr::null_mut();
            ffi::duckdb_table_function_get_client_context(info, &mut ctx);
            if ctx.is_null() {
                return Err(
                    "read_ags: DuckDB did not provide a client context (requires DuckDB 1.5+)"
                        .to_string(),
                );
            }
            let fs = ffi::duckdb_client_context_get_file_system(ctx);
            if fs.is_null() {
                ffi::duckdb_destroy_client_context(&mut ctx);
                return Err("read_ags: DuckDB did not provide a filesystem".to_string());
            }
            Ok(Vfs { ctx, fs })
        }
    }

    /// Open `path` read-only and resolve its size — the cheap `open` + `size()`
    /// every read does before committing to a slurp. A negative size is a VFS
    /// error (not clamped to 0, which would mask it); an absurd reported size is
    /// rejected up front so it can neither key the cache nor drive an allocation.
    fn open_for_read(&self, path: &str) -> Result<(FileHandle, u64), String> {
        let c_path = CString::new(path)
            .map_err(|_| format!("read_ags: path '{path}' contains an interior NUL byte"))?;
        unsafe {
            let mut opts = ffi::duckdb_create_file_open_options();
            ffi::duckdb_file_open_options_set_flag(
                opts,
                ffi::duckdb_file_flag_DUCKDB_FILE_FLAG_READ,
                true,
            );
            let mut raw: ffi::duckdb_file_handle = ptr::null_mut();
            let state = ffi::duckdb_file_system_open(self.fs, c_path.as_ptr(), opts, &mut raw);
            ffi::duckdb_destroy_file_open_options(&mut opts);
            if state != ffi::DuckDBSuccess || raw.is_null() {
                return Err(format!("read_ags: cannot open '{path}'"));
            }
            let handle = FileHandle { handle: raw };
            let raw_size = ffi::duckdb_file_handle_size(handle.handle);
            if raw_size < 0 {
                return Err(format!(
                    "read_ags: cannot determine the size of '{path}' (filesystem returned {raw_size})"
                ));
            }
            let size = raw_size as u64;
            let max = max_file_bytes();
            if size > max {
                return Err(format!(
                    "read_ags: '{path}' reports {size} bytes, over the {max}-byte limit (raise LATERITE_AGS_MAX_FILE_BYTES if this is a genuine file)"
                ));
            }
            Ok((handle, size))
        }
    }
}

impl Drop for Vfs {
    fn drop(&mut self) {
        unsafe {
            if !self.fs.is_null() {
                ffi::duckdb_destroy_file_system(&mut self.fs);
            }
            if !self.ctx.is_null() {
                ffi::duckdb_destroy_client_context(&mut self.ctx);
            }
        }
    }
}

/// An open VFS file handle. Destroyed (which closes it) on drop.
struct FileHandle {
    handle: ffi::duckdb_file_handle,
}

impl FileHandle {
    /// Read up to `buf.len()` bytes, returning the count (0 at EOF).
    fn read(&self, buf: &mut [u8]) -> Result<usize, String> {
        let n = unsafe {
            ffi::duckdb_file_handle_read(
                self.handle,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as i64,
            )
        };
        if n < 0 {
            Err("read_ags: read error".to_string())
        } else {
            Ok(n as usize)
        }
    }
}

impl Drop for FileHandle {
    fn drop(&mut self) {
        unsafe {
            if !self.handle.is_null() {
                ffi::duckdb_destroy_file_handle(&mut self.handle);
            }
        }
    }
}

/// Slurp the whole file into a buffer, with the same adversarial guards the
/// cached parse relies on: grow from a bounded reserve (never trust the reported
/// size as an up-front allocation), re-check the running total against the
/// ceiling (a stream can deliver more than it claimed), and reject a short read.
fn slurp(handle: &FileHandle, path: &str, size: u64) -> Result<Vec<u8>, String> {
    let max = max_file_bytes();
    let mut buf: Vec<u8> = Vec::with_capacity(size.min(INITIAL_RESERVE_BYTES) as usize);
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        let got = handle.read(&mut chunk)?;
        if got == 0 {
            break; // EOF
        }
        if buf.len() as u64 + got as u64 > max {
            return Err(format!(
                "read_ags: '{path}' exceeds the {max}-byte limit while reading (raise LATERITE_AGS_MAX_FILE_BYTES if this is a genuine file)"
            ));
        }
        buf.extend_from_slice(&chunk[..got]);
    }
    if (buf.len() as u64) < size {
        return Err(format!(
            "read_ags: short read on '{path}' (got {} of {size} bytes); the file may have changed underfoot — retry",
            buf.len()
        ));
    }
    Ok(buf)
}

/// Read + parse the AGS4 file at `path` through the VFS, memoised by
/// `(path, size)`. Structural malformation surfaces here with a message pointing
/// at validation/repair — the "assume valid, else say so" contract.
///
/// Phase 0 reads UTF-8 only; the `encoding` named-param path (non-UTF-8 decode)
/// rides `read_ags` and lands with that function in a later phase.
pub fn read_parsed(vfs: &Vfs, path: &str) -> Result<Arc<ParsedAgs4>, String> {
    let (handle, size) = vfs.open_for_read(path)?;
    super::cache::get_or_try_insert(path, size, "UTF-8", || {
        let buf = slurp(&handle, path, size)?;
        read_ags4_bytes(&buf).map_err(|e| {
            format!(
                "read_ags: '{path}' did not parse as AGS4 ({e}); the file may be invalid — validate/repair it first"
            )
        })
    })
}
