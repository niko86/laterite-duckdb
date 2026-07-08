//! `ags_dictionary([edition])`, `ags_relationships()` and `ags_rules()` — the
//! embedded AGS registry + rule catalogue surfaced as queryable tables, so the
//! spec schema, the relationship graph that `_parent_id` follows, and the
//! numbered-rule catalogue are all inspectable in SQL with no sidecar file.
//! `ags_dictionary()` with no argument returns the *union* registry (the
//! single-source dictionary spanning editions); `ags_dictionary(edition := …)`
//! returns that one edition's bundled STANDARD dictionary, over
//! `Dictionary::bundled` (#294 F#6).
//!
//! These are registry-only — no VFS, no `path`. They ride the
//! [`crate::ffi_table`] harness (compute-at-bind, stream-in-func) like the rest.

use laterite_ags4_core::keychain::shared_keys;
use laterite_ags4_core::registry::registry;
use laterite_ags4_validator::{Dictionary, rule_metadata_json};
use laterite_types::sql_type;
use libduckdb_sys as ffi;

use super::ffi_table::{Bind, Cell, ColType, register_table};

/// Register `ags_dictionary`, `ags_relationships` and `ags_rules`.
pub fn register(con: ffi::duckdb_connection) -> Result<(), Box<dyn std::error::Error>> {
    dictionary(con)?;
    relationships(con)?;
    rules(con)?;
    Ok(())
}

fn dictionary(con: ffi::duckdb_connection) -> Result<(), Box<dyn std::error::Error>> {
    register_table(con, "ags_dictionary", 0, &["edition"], |bind: &Bind| {
        let columns = vec![
            ("group", ColType::Varchar),
            ("parent", ColType::Varchar),
            ("heading", ColType::Varchar),
            ("status", ColType::Varchar),
            ("ags_type", ColType::Varchar),
            ("sql_type", ColType::Varchar),
            ("unit", ColType::Varchar),
            ("description", ColType::Varchar),
            ("ordinal", ColType::BigInt),
        ];
        // `edition :=` → that one edition's bundled STANDARD dictionary, over
        // Dictionary::bundled (#294 F#6); absent → the union registry (default).
        let edition = bind.named_str("edition");
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        match edition {
            Some(e) => {
                let d = Dictionary::bundled(super::cert::parse_edition(&e)?);
                let mut codes: Vec<&'static str> = d.group_codes().collect();
                codes.sort_unstable();
                for code in codes {
                    let parent = d.group(code).map(|m| m.parent).filter(|p| !p.is_empty());
                    for (i, h) in d.group_headings(code).iter().enumerate() {
                        let entry = d.heading(code, h);
                        let ags_type = entry.map(|x| x.ags_type).unwrap_or("");
                        rows.push(vec![
                            Cell::Str(code.to_string()),
                            parent.map_or(Cell::Null, |p| Cell::Str(p.to_string())),
                            Cell::Str(h.to_string()),
                            Cell::Str(entry.map(|x| x.status).unwrap_or("").to_string()),
                            Cell::Str(ags_type.to_string()),
                            Cell::Str(sql_type(ags_type).to_string()),
                            entry
                                .map(|x| x.unit)
                                .filter(|u| !u.is_empty())
                                .map_or(Cell::Null, |u| Cell::Str(u.to_string())),
                            entry
                                .map(|x| x.desc)
                                .filter(|s| !s.is_empty())
                                .map_or(Cell::Null, |s| Cell::Str(s.to_string())),
                            Cell::Int(i as i64),
                        ]);
                    }
                }
            }
            None => {
                let reg = registry();
                for g in reg.iter() {
                    for (i, h) in g.headings.iter().enumerate() {
                        rows.push(vec![
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
            }
        }
        Ok((columns, rows))
    })
}

fn relationships(con: ffi::duckdb_connection) -> Result<(), Box<dyn std::error::Error>> {
    register_table(con, "ags_relationships", 0, &[], |_bind: &Bind| {
        let columns = vec![
            ("child", ColType::Varchar),
            ("parent", ColType::Varchar),
            // The KEY headings the child shares with its parent — the columns
            // `_parent_id` is derived from. Comma-joined; empty ⇒ the link is
            // unresolvable from data (key drift with no shared name).
            ("shared_keys", ColType::Varchar),
        ];
        let reg = registry();
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        for g in reg.iter() {
            if let Some(parent) = &g.parent {
                rows.push(vec![
                    Cell::Str(g.code.clone()),
                    Cell::Str(parent.clone()),
                    Cell::Str(shared_keys(reg, g).join(",")),
                ]);
            }
        }
        Ok((columns, rows))
    })
}

/// `ags_rules()` — the numbered AGS4 rule catalogue (the engine's gated
/// `rules_meta.json`) as queryable rows, so the AGS4 rules the `laterite`
/// validator enforces are inspectable in SQL (#294 F#11). One row per rule;
/// `observations` is the comma-joined O-N list, `fixable` is whether
/// `lat --fix` can repair it.
fn rules(con: ffi::duckdb_connection) -> Result<(), Box<dyn std::error::Error>> {
    register_table(con, "ags_rules", 0, &[], |_bind: &Bind| {
        let columns = vec![
            ("rule", ColType::Varchar),
            ("title", ColType::Varchar),
            ("checks", ColType::Varchar),
            ("severity", ColType::Varchar),
            ("fixable", ColType::Boolean),
            ("observations", ColType::Varchar),
        ];
        // The catalogue is compile-time-embedded and the validator asserts it
        // parses, so this never fails at runtime.
        let meta: serde_json::Value = serde_json::from_str(rule_metadata_json())
            .expect("embedded rules_meta.json always parses");
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        if let Some(catalogue) = meta.get("rules").and_then(|r| r.as_array()) {
            for r in catalogue {
                let s = |k: &str| {
                    r.get(k)
                        .and_then(|v| v.as_str())
                        .filter(|x| !x.is_empty())
                        .map_or(Cell::Null, |x| Cell::Str(x.to_string()))
                };
                let obs = r
                    .get("observations")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                rows.push(vec![
                    s("rule"),
                    s("title"),
                    s("checks"),
                    s("severity"),
                    r.get("fixable")
                        .and_then(|v| v.as_bool())
                        .map_or(Cell::Null, Cell::Bool),
                    if obs.is_empty() {
                        Cell::Null
                    } else {
                        Cell::Str(obs)
                    },
                ]);
            }
        }
        Ok((columns, rows))
    })
}
