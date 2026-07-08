//! `ags_groups(path)` — a file's own structure as a queryable table: the group
//! list with per-group row/heading counts and the registry parent. Filter to one
//! group with a plain `WHERE "group" = 'LOCA'`.
//!
//! Phase 0 ports only `ags_groups` onto the [`crate::ffi_table`] harness (the
//! sibling `ags_headings` and the other functions land in later phases).

use laterite_ags4_core::registry::registry;
use libduckdb_sys as ffi;

use super::ffi_table::{Bind, Cell, ColType, register_table};
use super::source::{Vfs, read_parsed};

/// Register `ags_groups(path)`.
pub fn register(con: ffi::duckdb_connection) -> Result<(), Box<dyn std::error::Error>> {
    register_table(con, "ags_groups", 1, &[], |bind: &Bind| {
        let path = bind.param_str(0)?;
        // SAFETY: the producer runs during bind, so the raw bind info is live
        // and its client context (the VFS) is valid for this call.
        let vfs = unsafe { Vfs::from_bind(bind.raw_info()) }?;
        let parsed = read_parsed(&vfs, &path)?;
        let reg = registry();

        let columns = vec![
            ("group", ColType::Varchar),
            ("n_rows", ColType::BigInt),
            ("n_headings", ColType::BigInt),
            ("parent", ColType::Varchar),
        ];
        let rows = parsed
            .order
            .iter()
            .map(|code| {
                let g = parsed.get(code).expect("group from order exists");
                // The file doesn't carry the parent — the registry does; an
                // unknown/custom group has no registry parent (→ NULL).
                let parent = reg.get(code).and_then(|d| d.parent.clone());
                vec![
                    Cell::Str(code.clone()),
                    Cell::Int(g.rows.len() as i64),
                    Cell::Int(g.headings.len() as i64),
                    parent.map_or(Cell::Null, Cell::Str),
                ]
            })
            .collect();

        Ok((columns, rows))
    })
}
