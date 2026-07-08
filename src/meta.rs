//! `ags_groups(path)` and `ags_headings(path)` — a file's own structure as
//! queryable tables: the group list with per-group row/heading counts and the
//! registry parent, and per-heading units/types straight from the file's
//! UNIT/TYPE rows (AGS4 is self-describing), enriched from the registry where the
//! group is known (parent, KEY status). Filter to one group with a plain
//! `WHERE "group" = 'LOCA'`.
//!
//! Both ride the [`crate::ffi_table`] harness (compute-at-bind, stream-in-func).

use laterite_ags4_core::registry::registry;
use laterite_types::sql_type;
use libduckdb_sys as ffi;

use super::ffi_table::{Bind, Cell, ColType, register_table};
use super::source::{Vfs, read_parsed};

/// Register `ags_groups(path)` and `ags_headings(path)`.
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
    })?;

    register_table(con, "ags_headings", 1, &[], |bind: &Bind| {
        let path = bind.param_str(0)?;
        // SAFETY: the producer runs during bind, so the raw bind info is live
        // and its client context (the VFS) is valid for this call.
        let vfs = unsafe { Vfs::from_bind(bind.raw_info()) }?;
        let parsed = read_parsed(&vfs, &path)?;
        let reg = registry();

        let columns = vec![
            ("group", ColType::Varchar),
            ("heading", ColType::Varchar),
            ("unit", ColType::Varchar),
            ("ags_type", ColType::Varchar),
            ("sql_type", ColType::Varchar),
            ("status", ColType::Varchar),
            ("is_key", ColType::Boolean),
            ("ordinal", ColType::BigInt),
        ];
        let mut rows = Vec::new();
        for code in &parsed.order {
            let g = parsed.get(code).expect("group from order exists");
            let desc = reg.get(code);
            for (i, heading) in g.headings.iter().enumerate() {
                // AGS4 carries the type/unit per heading in its own TYPE/UNIT
                // rows; a shorter-than-headings row falls back to empty.
                let ags_type = g.types.get(i).cloned().unwrap_or_default();
                let unit = g.units.get(i).cloned().unwrap_or_default();
                // The file doesn't carry KEY status — the registry does;
                // unknown/custom groups + headings fall back to OTHER / not-key.
                let (status, is_key) = desc
                    .and_then(|d| d.headings.iter().find(|h| h.name == *heading))
                    .map_or(("OTHER".to_string(), false), |h| {
                        (h.status.clone(), h.is_key())
                    });
                rows.push(vec![
                    Cell::Str(code.clone()),
                    Cell::Str(heading.clone()),
                    if unit.is_empty() {
                        Cell::Null
                    } else {
                        Cell::Str(unit)
                    },
                    Cell::Str(ags_type.clone()),
                    Cell::Str(sql_type(&ags_type).to_string()),
                    Cell::Str(status),
                    Cell::Bool(is_key),
                    Cell::Int(i as i64),
                ]);
            }
        }

        Ok((columns, rows))
    })
}
