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
//! Built on the **C Extension API** via the quack-rs SDK (forward-compatible
//! ABI, zero C++). It reuses the pure Rust engine wholesale: `laterite-ags4-core`'s
//! AGS4 codec + deterministic-key [`keychain`](laterite_ags4_core::keychain) and
//! `laterite-types`' single typing authority.
//!
//! P1 surface: `read_ags(path, group)` over local files. Metadata functions
//! (`ags_groups`/`ags_headings`/`ags_dictionary`/`ags_relationships`),
//! editions, validation, remote/httpfs, and persistence land in later phases.

use quack_rs::prelude::*;

// `#[path]` so submodules resolve from `src/` whether lib.rs is the native crate
// root OR re-included as `mod lib;` by the wasm shim (src/wasm_lib.rs). Without
// it the wasm build looks for `src/lib/*.rs`. Paired with the submodules' `super::`
// imports, this makes lib.rs compile in both contexts.
#[path = "dict_fns.rs"]
mod dict_fns;
#[path = "read_ags.rs"]
mod read_ags;
#[path = "rows.rs"]
mod rows;
#[path = "typing.rs"]
mod typing;

// VFS / path-based readers — compiled only with the `vfs` feature (default).
// They use DuckDB's virtual filesystem (the unstable C API), so they're absent
// from a stable `--no-default-features` build.
#[cfg(feature = "vfs")]
#[path = "load.rs"]
mod load;
#[cfg(feature = "vfs")]
#[path = "meta.rs"]
mod meta;
#[cfg(feature = "vfs")]
#[path = "source.rs"]
mod source;
#[cfg(feature = "vfs")]
#[path = "validate.rs"]
mod validate;

/// Register every function this extension provides.
fn register(con: &Connection) -> ExtResult<()> {
    // Stable-API functions — present in every build, including the stable wasm one.
    read_ags::register_text(con)?; // read_ags_text(content, group)
    dict_fns::register(con)?; // ags_dictionary, ags_relationships

    // Path-based readers via the VFS (unstable C API) — `vfs` feature only.
    #[cfg(feature = "vfs")]
    {
        read_ags::register(con)?; // read_ags(path, group)
        meta::register(con)?; // ags_groups, ags_headings
        validate::register(con)?; // ags_validate(path)
        load::register(con)?; // load_ags_script(path)
    }
    Ok(())
}

// The symbol `LOAD laterite_ags4` calls. The built cdylib (+ metadata footer)
// is published as the `laterite_ags4` community extension.
quack_rs::entry_point_v2!(laterite_ags4_init_c_api, register);
