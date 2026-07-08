//! `laterite_ags4` — a DuckDB loadable extension that reads AGS4 geotechnical
//! files as **typed, UUID-keyed tables**, straight from SQL.
//!
//! ```sql
//! LOAD laterite_ags4;
//! SELECT "group", n_rows, parent FROM ags_groups('site.ags') ORDER BY n_rows DESC;
//! ```
//!
//! Built on the **C Extension API** via the official `duckdb` crate (duckdb-rs,
//! DuckDB 1.5.3 C-API line; zero C++). It reuses the pure Rust engine wholesale:
//! `laterite-ags4-core`'s AGS4 codec + deterministic-key `keychain` and
//! `laterite-types`' single typing authority.
//!
//! A **read-only SQL surface** over AGS4 — validation and certification live in
//! the `lat` CLI / the `laterite` library, not here.
//!
//! ## Binding
//!
//! Every function runs on the raw-FFI table-function harness ([`ffi_table`]) over
//! the duckdb crate's C Extension API, reading files through DuckDB's VFS
//! ([`source`]): `read_ags` / `read_ags_text` (typed, UUID-keyed group tables via
//! the shared typing authority [`typing`], with the `.ags.idx` cert fast-path
//! [`cert`]), `ags_groups` / `ags_headings` ([`meta`]), `ags_dictionary` /
//! `ags_relationships` / `ags_rules` ([`dict_fns`]), and `load_ags` ([`load`]).

use std::error::Error;
use std::ffi::CString;

use libduckdb_sys as ffi;

// `#[path]` so the submodules resolve to `src/*.rs` in BOTH build shapes: the
// native cdylib (crate root = this file) and the wasm staticlib example
// (`src/wasm_lib.rs` pulls this file in as `mod lib`, which would otherwise look
// for its children under `src/lib/`). Cross-module refs use `super::` (not
// `crate::`) for the same reason — under `mod lib`, `crate::` is the example
// root, `super::` is this module in both shapes.
#[path = "cache.rs"]
mod cache;
#[path = "cert.rs"]
mod cert;
#[path = "dict_fns.rs"]
mod dict_fns;
#[path = "ffi_table.rs"]
mod ffi_table;
#[path = "load.rs"]
mod load;
#[path = "meta.rs"]
mod meta;
#[path = "read_ags.rs"]
mod read_ags;
#[path = "source.rs"]
mod source;
#[path = "typing.rs"]
mod typing;

/// Register every function this extension provides — all on the [`ffi_table`]
/// harness.
///
/// Takes the raw `duckdb_connection` (not a `duckdb::Connection`): the harness
/// registers through raw `libduckdb-sys`, and duckdb-rs exposes no way to reach
/// the raw connection from a `Connection` (nor to register a hand-built table
/// function) — see [`ffi_table`].
fn register(con: ffi::duckdb_connection) -> Result<(), Box<dyn Error>> {
    meta::register(con)?; // ags_groups(path), ags_headings(path)
    read_ags::register(con)?; // read_ags(path, group [, encoding := …])
    read_ags::register_text(con)?; // read_ags_text(content, group)
    dict_fns::register(con)?; // ags_dictionary([edition]), ags_relationships(), ags_rules()
    load::register(con)?; // load_ags(path)
    Ok(())
}

/// The symbol `LOAD laterite_ags4` calls.
///
/// Hand-written (rather than via duckdb-rs's `#[duckdb_entrypoint_c_api]`) so we
/// can obtain a raw `duckdb_connection` for registration: the macro hands back a
/// `duckdb::Connection` whose raw handle is inaccessible, and the harness needs
/// the raw connection. Mirrors the macro's shape otherwise (init the C API
/// struct, fetch the database, register, surface errors).
///
/// # Safety
/// Called by DuckDB at extension load with a valid `info` / `access` pair.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn laterite_ags4_init_c_api(
    info: ffi::duckdb_extension_info,
    access: *const ffi::duckdb_extension_access,
) -> bool {
    unsafe {
        match init_internal(info, access) {
            Ok(v) => v,
            Err(e) => {
                if let Some(set_error) = (*access).set_error {
                    if let Ok(msg) = CString::new(e.to_string()) {
                        set_error(info, msg.as_ptr());
                    }
                }
                false
            }
        }
    }
}

/// Internal entry point (error-returning); the `extern "C"` wrapper reports any
/// error to DuckDB.
///
/// # Safety
/// See [`laterite_ags4_init_c_api`].
unsafe fn init_internal(
    info: ffi::duckdb_extension_info,
    access: *const ffi::duckdb_extension_access,
) -> Result<bool, Box<dyn Error>> {
    unsafe {
        // Populate the C API function-pointer struct the libduckdb-sys wrappers
        // dispatch through. v1.5.1 is the floor at which the client-context /
        // filesystem functions are present (a too-low floor makes the VFS calls
        // hit an "API not initialized" assert).
        let have_api = ffi::duckdb_rs_extension_api_init(info, access, "v1.5.1")?;
        if !have_api {
            return Ok(false); // API version mismatch — DuckDB has the reason
        }

        let get_database = (*access)
            .get_database
            .ok_or("duckdb_extension_access.get_database is null")?;
        let db_ptr = get_database(info);
        if db_ptr.is_null() {
            return Ok(false); // DuckDB already recorded why
        }
        let db: ffi::duckdb_database = *db_ptr;

        // A raw connection just to register the table functions. Registration is
        // copied into the database catalog, so the connection can be released
        // immediately afterwards (the functions persist database-wide).
        let mut con: ffi::duckdb_connection = std::ptr::null_mut();
        if ffi::duckdb_connect(db, &mut con) != ffi::DuckDBSuccess {
            return Err("failed to open a connection to register functions".into());
        }
        let result = register(con);
        ffi::duckdb_disconnect(&mut con);
        result?;
        Ok(true)
    }
}
