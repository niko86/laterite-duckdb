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
    meta::register(con)?; // ags_groups, ags_headings
    dict_fns::register(con)?; // ags_dictionary, ags_relationships
    validate::register(con)?; // ags_validate(path)
    load::register(con)?; // load_ags_script(path)
    Ok(())
}

// The symbol `LOAD laterite_ags4` calls. The built cdylib (+ metadata footer)
// is published as the `laterite_ags4` community extension.
quack_rs::entry_point_v2!(laterite_ags4_init_c_api, register);
