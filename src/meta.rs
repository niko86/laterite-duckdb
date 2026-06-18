//! `ags_groups(path)` and `ags_headings(path)` — a file's own structure as
//! queryable tables: the group list, and per-heading units/types straight from
//! the file's UNIT/TYPE rows (AGS4 is self-describing), enriched from the
//! registry where the group is known (parent, KEY status). Filter to one group
//! with a plain `WHERE "group" = 'LOCA'`.

use laterite_ags4_core::registry::registry;
use laterite_types::sql_type;
use quack_rs::prelude::*;

use crate::rows::{Cell, register_rows};
use crate::source::read_parsed;

pub fn register(con: &Connection) -> ExtResult<()> {
    groups(con)?;
    headings(con)?;
    Ok(())
}

fn groups(con: &Connection) -> ExtResult<()> {
    register_rows(
        con,
        "ags_groups",
        1,
        &[],
        vec![
            ("group", TypeId::Varchar),
            ("n_rows", TypeId::BigInt),
            ("n_headings", TypeId::BigInt),
            ("parent", TypeId::Varchar),
        ],
        |bind| {
            let path = unsafe { bind.get_parameter_value(0) }.as_str()?;
            // SAFETY: live bind-info → the query's client context → the VFS.
            let ctx = unsafe { bind.get_client_context() };
            let parsed = read_parsed(&ctx, &path)?;
            let reg = registry();
            Ok(parsed
                .order
                .iter()
                .map(|code| {
                    let g = parsed.get(code).expect("group from order exists");
                    let parent = reg.get(code).and_then(|d| d.parent.clone());
                    vec![
                        Cell::Str(code.clone()),
                        Cell::Int(g.rows.len() as i64),
                        Cell::Int(g.headings.len() as i64),
                        parent.map_or(Cell::Null, Cell::Str),
                    ]
                })
                .collect())
        },
    )
}

fn headings(con: &Connection) -> ExtResult<()> {
    register_rows(
        con,
        "ags_headings",
        1,
        &[],
        vec![
            ("group", TypeId::Varchar),
            ("heading", TypeId::Varchar),
            ("unit", TypeId::Varchar),
            ("ags_type", TypeId::Varchar),
            ("sql_type", TypeId::Varchar),
            ("status", TypeId::Varchar),
            ("is_key", TypeId::Boolean),
            ("ordinal", TypeId::BigInt),
        ],
        |bind| {
            let path = unsafe { bind.get_parameter_value(0) }.as_str()?;
            // SAFETY: live bind-info → the query's client context → the VFS.
            let ctx = unsafe { bind.get_client_context() };
            let parsed = read_parsed(&ctx, &path)?;
            let reg = registry();
            let mut out = Vec::new();
            for code in &parsed.order {
                let g = parsed.get(code).expect("group exists");
                let desc = reg.get(code);
                for (i, heading) in g.headings.iter().enumerate() {
                    let ags_type = g.types.get(i).cloned().unwrap_or_default();
                    let unit = g.units.get(i).cloned().unwrap_or_default();
                    // The file doesn't carry KEY status — the registry does;
                    // unknown/custom groups + headings fall back to OTHER / not-key.
                    let (status, is_key) = desc
                        .and_then(|d| d.headings.iter().find(|h| h.name == *heading))
                        .map_or(("OTHER".to_string(), false), |h| {
                            (h.status.clone(), h.is_key())
                        });
                    out.push(vec![
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
            Ok(out)
        },
    )
}
