//! `ags_groups(path)` and `ags_headings(path)` — a file's own structure as
//! queryable tables: the group list with per-group row/heading counts and the
//! parent, and per-heading units/types straight from the file's UNIT/TYPE rows
//! (AGS4 is self-describing), enriched with parent and KEY status from the
//! Rule 18 effective dictionary — the registry where it answers, else the
//! file's own `DICT` declarations. Filter to one group with a plain
//! `WHERE "group" = 'LOCA'`.
//!
//! Both ride the [`crate::ffi_table`] harness (compute-at-bind, stream-in-func).

use laterite_ags4_core::effective_dict::{FileDict, file_dict_of};
use laterite_ags4_core::registry::registry;
use laterite_ags4_types::sql_type;
use libduckdb_sys as ffi;

use super::ffi_table::{Bind, Cell, ColType, register_table};
use super::source::{Vfs, read_parsed};

/// The file half's declared parent for `code`, normalised to `Option`:
/// the DICT convention `"-"` (explicitly parentless) and an empty cell both
/// mean "no parent" — the same normalisation `EffectiveDict::parent` applies.
fn file_parent(fd: &FileDict, code: &str) -> Option<String> {
    fd.parent(code)
        .filter(|p| !p.is_empty() && *p != "-")
        .map(str::to_string)
}

/// Register `ags_groups(path)` and `ags_headings(path)`.
pub fn register(con: ffi::duckdb_connection) -> Result<(), Box<dyn std::error::Error>> {
    register_table(con, "ags_groups", 1, &[], |bind: &Bind| {
        let path = bind.param_str(0)?;
        // SAFETY: the producer runs during bind, so the raw bind info is live
        // and its client context (the VFS) is valid for this call.
        let vfs = unsafe { Vfs::from_bind(bind.raw_info()) }?;
        let parsed = read_parsed(&vfs, &path)?;
        let reg = registry();
        let fd = file_dict_of(&parsed);

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
                // Parent from the effective dictionary: the registry where it
                // knows the group, else the file's own DICT_PGRP declaration
                // (Rule 18) — a group declared by neither has none (→ NULL).
                let parent = reg
                    .get(code)
                    .and_then(|d| d.parent.clone())
                    .or_else(|| file_parent(&fd, code));
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
        let fd = file_dict_of(&parsed);

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
                // Status from the effective dictionary: the registry where it
                // answers, else the file's own DICT_STAT declaration (Rule 18
                // — this is where a declared custom group's KEY headings come
                // from, and the validator keys Rule 10a off the same source).
                // Declared by neither → OTHER / not-key. `is_key` uses the
                // shared module's predicate (case-insensitive "KEY" within
                // DICT_STAT, so "KEY+REQUIRED" counts); the status string
                // stays as declared.
                let (status, is_key) = desc
                    .and_then(|d| d.headings.iter().find(|h| h.name == *heading))
                    .map(|h| (h.status.clone(), h.is_key()))
                    .or_else(|| {
                        fd.heading(code, heading).map(|h| {
                            let key = h.status.to_ascii_uppercase().contains("KEY");
                            (h.status.clone(), key)
                        })
                    })
                    .unwrap_or_else(|| ("OTHER".to_string(), false));
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
