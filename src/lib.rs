//! `laterite_ags4` — a DuckDB loadable extension that reads AGS4 geotechnical
//! files as **typed, UUID-keyed tables**, straight from SQL.
//!
//! ```sql
//! LOAD laterite_ags4;
//! SELECT loca_id, loca_gl FROM read_ags('site.ags', 'LOCA') WHERE loca_gl > 50.0;
//! -- join across groups via the deterministic keys, no shared state:
//! SELECT s.samp_ref, l.loca_gl
//! FROM read_ags('site.ags','SAMP') s
//! JOIN read_ags('site.ags','LOCA') l ON s._parent_id = l._id;
//! ```
//!
//! Built on the **C Extension API** via the quack-rs SDK (zero C++). It reuses the
//! pure Rust engine wholesale: `laterite-ags4-core`'s AGS4 codec + deterministic-key
//! [`keychain`](laterite_ags4_core::keychain) and `laterite-types`' single typing
//! authority.
//!
//! **Native-only.** The path/remote readers use DuckDB's filesystem (the VFS), which
//! is the version-exact C API line — so the extension is rebuilt against each DuckDB
//! release (community-extensions' build matrix does this). It is NOT built for
//! DuckDB-WASM (which lags this ABI); browser SQL-over-AGS is served by the dedicated
//! `laterite-ags4-wasm` package.
//!
//! Surface: `read_ags(path, group)` / `read_ags_text(content, group)`; metadata
//! (`ags_groups`/`ags_headings`/`ags_dictionary`/`ags_relationships`);
//! `validate_ags` / `validate_ags_text`; `certify_ags` (mint a `.ags.idx`
//! certificate; `read_ags`/`validate_ags` then take a sliced / skip-revalidation
//! fast-path when a fresh one exists) / `certify_ags_text` (return the cert JSON
//! in a column); `load_ags_script`; local + `http(s)://` + `s3://` (with
//! `LOAD httpfs`). The path verbs take an `encoding` named param for non-UTF-8
//! sources; the `_text` variants are UTF-8 (their input is already a VARCHAR).

use quack_rs::prelude::*;

mod cache;
mod cert;
mod certify;
mod dict_fns;
mod load;
mod meta;
mod read_ags;
mod rows;
mod source;
mod typing;
mod validate;

/// Register every function this extension provides.
fn register(con: &Connection) -> ExtResult<()> {
    read_ags::register(con)?; // read_ags(path, group)
    read_ags::register_text(con)?; // read_ags_text(content, group)
    meta::register(con)?; // ags_groups, ags_headings
    validate::register(con)?; // validate_ags(path)
    validate::register_text(con)?; // validate_ags_text(content)
    certify::register(con)?; // certify_ags(path) → mint <path>.idx
    certify::register_text(con)?; // certify_ags_text(content) → cert JSON in a column
    load::register(con)?; // load_ags_script(path)
    dict_fns::register(con)?; // ags_dictionary, ags_relationships
    Ok(())
}

// The symbol `LOAD laterite_ags4` calls. The built cdylib (+ metadata footer)
// is published as the `laterite_ags4` community extension.
quack_rs::entry_point_v2!(laterite_ags4_init_c_api, register);
