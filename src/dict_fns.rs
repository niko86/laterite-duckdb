//! `ags_dictionary()` and `ags_relationships()` — the embedded AGS registry
//! (the single-source dictionary) surfaced as queryable tables, so the spec
//! schema and the relationship graph that `_parent_id` follows are inspectable
//! in SQL with no sidecar file. The dictionary is single-edition today, so
//! neither takes an `edition` argument yet (a deliberate P2 boundary — the
//! per-edition standard dicts live in the validator, not this registry).

use laterite_ags4_core::keychain::shared_keys;
use laterite_ags4_core::registry::registry;
use laterite_types::sql_type;
use quack_rs::prelude::*;

use super::rows::{Cell, register_rows};

pub fn register(con: &Connection) -> ExtResult<()> {
    dictionary(con)?;
    relationships(con)?;
    Ok(())
}

fn dictionary(con: &Connection) -> ExtResult<()> {
    register_rows(
        con,
        "ags_dictionary",
        0,
        &[],
        vec![
            ("group", TypeId::Varchar),
            ("parent", TypeId::Varchar),
            ("heading", TypeId::Varchar),
            ("status", TypeId::Varchar),
            ("ags_type", TypeId::Varchar),
            ("sql_type", TypeId::Varchar),
            ("unit", TypeId::Varchar),
            ("description", TypeId::Varchar),
            ("ordinal", TypeId::BigInt),
        ],
        |_bind| {
            let reg = registry();
            let mut out = Vec::new();
            for g in reg.iter() {
                for (i, h) in g.headings.iter().enumerate() {
                    out.push(vec![
                        Cell::Str(g.code.clone()),
                        g.parent.clone().map_or(Cell::Null, Cell::Str),
                        Cell::Str(h.name.clone()),
                        Cell::Str(h.status.clone()),
                        Cell::Str(h.ags_type.clone()),
                        Cell::Str(sql_type(&h.ags_type).to_string()),
                        h.unit
                            .clone()
                            .filter(|u| !u.is_empty())
                            .map_or(Cell::Null, Cell::Str),
                        if h.description.is_empty() {
                            Cell::Null
                        } else {
                            Cell::Str(h.description.clone())
                        },
                        Cell::Int(i as i64),
                    ]);
                }
            }
            Ok(out)
        },
    )
}

fn relationships(con: &Connection) -> ExtResult<()> {
    register_rows(
        con,
        "ags_relationships",
        0,
        &[],
        vec![
            ("child", TypeId::Varchar),
            ("parent", TypeId::Varchar),
            // The KEY headings the child shares with its parent — the columns
            // `_parent_id` is derived from. Comma-joined; empty ⇒ the link is
            // unresolvable from data (key drift with no shared name).
            ("shared_keys", TypeId::Varchar),
        ],
        |_bind| {
            let reg = registry();
            let mut out = Vec::new();
            for g in reg.iter() {
                if let Some(parent) = &g.parent {
                    out.push(vec![
                        Cell::Str(g.code.clone()),
                        Cell::Str(parent.clone()),
                        Cell::Str(shared_keys(reg, g).join(",")),
                    ]);
                }
            }
            Ok(out)
        },
    )
}
