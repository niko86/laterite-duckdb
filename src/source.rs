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
    ///
    /// `pub(crate)` because the cert fast-path ([`super::cert`]) opens the source
    /// itself to `seek` + range-read one group's bytes.
    pub(crate) fn open_for_read(&self, path: &str) -> Result<(FileHandle, u64), String> {
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
///
/// `pub(crate)` (with `pub(crate)` methods) so the cert fast-path
/// ([`super::cert`]) can `seek` + range-read one group's slice.
pub(crate) struct FileHandle {
    handle: ffi::duckdb_file_handle,
}

impl FileHandle {
    /// Read up to `buf.len()` bytes, returning the count (0 at EOF).
    pub(crate) fn read(&self, buf: &mut [u8]) -> Result<usize, String> {
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

    /// Seek the read position to an absolute byte offset — the cert fast-path's
    /// ranged read starts a group's slice here (a remote ranged-GET on
    /// http/s3). Mirrors quack-rs's `FileHandle::seek`.
    pub(crate) fn seek(&self, position: u64) -> Result<(), String> {
        let state = unsafe { ffi::duckdb_file_handle_seek(self.handle, position as i64) };
        if state != ffi::DuckDBSuccess {
            return Err(format!("read_ags: seek to {position} failed"));
        }
        Ok(())
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

/// Read + parse the AGS4 file at `path` through the VFS as UTF-8, memoised by
/// `(path, size, "UTF-8")`. The default reader (`ags_groups`, `load_ags`); a
/// thin wrapper over [`read_parsed_with_encoding`].
pub fn read_parsed(vfs: &Vfs, path: &str) -> Result<Arc<ParsedAgs4>, String> {
    read_parsed_with_encoding(vfs, path, encoding_rs::UTF_8)
}

/// Read + parse the AGS4 file at `path` through the VFS, decoding the source
/// bytes with `encoding` before the UTF-8-only core codec. Memoised by
/// `(path, size, encoding.name())`, so the same file read as UTF-8 and as
/// windows-1252 memoise to distinct parses. Structural malformation surfaces
/// here with a message pointing at validation/repair — the "assume valid, else
/// say so" contract.
pub fn read_parsed_with_encoding(
    vfs: &Vfs,
    path: &str,
    encoding: &'static encoding_rs::Encoding,
) -> Result<Arc<ParsedAgs4>, String> {
    let (handle, size) = vfs.open_for_read(path)?;
    super::cache::get_or_try_insert(path, size, encoding.name(), || {
        let buf = slurp(&handle, path, size)?;
        // UTF-8 is the core codec's native form (no copy); any other WHATWG
        // label is decoded to UTF-8 first (the malformed-sequence fallback is
        // encoding_rs's replacement char — a decode never errors out here).
        let decoded = if encoding == encoding_rs::UTF_8 {
            std::borrow::Cow::Borrowed(&buf[..])
        } else {
            let (text, _, _) = encoding.decode(&buf);
            std::borrow::Cow::Owned(text.into_owned().into_bytes())
        };
        read_ags4_bytes(decoded.as_ref()).map_err(|e| {
            format!(
                "read_ags: '{path}' did not parse as AGS4 ({e}); the file may be invalid — validate/repair it first"
            )
        })
    })
}

/// Read a whole (small, un-cached) file through the VFS — used for the `.ags.idx`
/// sidecar. Unlike [`read_parsed`], this is not memoised (the sidecar is tiny and
/// read at most once per bind); a missing file surfaces as an error so the cert
/// path can `.ok()?` it into a clean fallback.
pub fn read_bytes(vfs: &Vfs, path: &str) -> Result<Vec<u8>, String> {
    let (handle, size) = vfs.open_for_read(path)?;
    slurp(&handle, path, size)
}

/// Resolve an optional WHATWG encoding label (`read_ags(encoding := …)`) to a
/// static [`encoding_rs::Encoding`]. Absent or blank → UTF-8 (the default); an
/// unrecognised label is a clean error, not a panic.
pub fn resolve_encoding(label: Option<&str>) -> Result<&'static encoding_rs::Encoding, String> {
    match label.map(str::trim).filter(|l| !l.is_empty()) {
        None => Ok(encoding_rs::UTF_8),
        Some(l) => encoding_rs::Encoding::for_label(l.as_bytes())
            .ok_or_else(|| format!("read_ags: unknown encoding label '{l}'")),
    }
}
