//! `ags_validate(path[, edition])` — opt-in AGS4 validation as a queryable table.
//!
//! Wraps the clean-room `laterite-ags4-validator` (`check_file`). One row per
//! finding: `(rule, line, group, severity, desc)`. This is **never** a gate on
//! `read_ags` — reads assume the file is valid and only surface *structural*
//! malformation; call `ags_validate` explicitly when you want the full rule
//! check. There is no repair surface (mutation stays in `lat-check`/the library).
//!
//! The edition is auto-detected from `TRAN_AGS` by default; the optional
//! `edition` **named** parameter forces a bundled dictionary edition regardless
//! (e.g. to check a file's forward/backward compatibility against a specific
//! schema):
//!   - `ags_validate(path)`                 — auto-detect from `TRAN_AGS`.
//!   - `ags_validate(path, edition := '4.2')` — force '4.0.3'/'4.0.4'/'4.1'/
//!     '4.1.1'/'4.2'. (Named, not a 2nd positional, because DuckDB has no
//!     same-name table-function overloads — see `rows::register_rows`.)

use std::path::Path;

use laterite_ags4_validator::findings::Severity;
use laterite_ags4_validator::{CheckOptions, DictVersion, check_file};
use quack_rs::prelude::*;

use crate::rows::{Cell, register_rows};

pub fn register(con: &Connection) -> ExtResult<()> {
    register_rows(
        con,
        "ags_validate",
        1,
        &[("edition", TypeId::Varchar)],
        vec![
            ("rule", TypeId::Varchar),
            ("line", TypeId::BigInt),
            ("group", TypeId::Varchar),
            ("severity", TypeId::Varchar),
            ("desc", TypeId::Varchar),
        ],
        |bind| {
            let path = unsafe { bind.get_parameter_value(0) }.as_str()?;
            // `edition` named param: absent → `as_str()` errors (null Value) →
            // auto-detect; an explicit, non-blank value forces that edition.
            let opts = match unsafe { bind.get_named_parameter_value("edition") }.as_str() {
                Ok(e) if !e.trim().is_empty() => CheckOptions {
                    dict_version: Some(parse_edition(&e)?),
                    ..CheckOptions::default()
                },
                _ => CheckOptions::default(),
            };
            run(&path, &opts)
        },
    )
}

/// Map a user edition string to a bundled `DictVersion`, or a clear error
/// listing the supported set. (The validator deliberately exposes no
/// `FromStr` — the bundled set is small and fixed, so we match it here.)
fn parse_edition(s: &str) -> Result<DictVersion, ExtensionError> {
    match s.trim() {
        "4.0.3" => Ok(DictVersion::V4_0_3),
        "4.0.4" => Ok(DictVersion::V4_0_4),
        "4.1" => Ok(DictVersion::V4_1),
        "4.1.1" => Ok(DictVersion::V4_1_1),
        "4.2" => Ok(DictVersion::V4_2),
        other => Err(ExtensionError::new(format!(
            "ags_validate: unknown edition '{other}'; expected one of 4.0.3, 4.0.4, 4.1, 4.1.1, 4.2"
        ))),
    }
}

/// Run the validator and flatten its findings into output rows.
fn run(path: &str, opts: &CheckOptions) -> Result<Vec<Vec<Cell>>, ExtensionError> {
    let findings = check_file(Path::new(path), opts)
        .map_err(|e| ExtensionError::new(format!("ags_validate: '{path}': {e}")))?;
    let mut out = Vec::new();
    // Findings is a BTreeMap<rule, Vec<Finding>> — already deterministic.
    for (rule, items) in &findings {
        for f in items {
            out.push(vec![
                Cell::Str(rule.clone()),
                f.line.map_or(Cell::Null, |l| Cell::Int(i64::from(l))),
                Cell::Str(f.group.clone()),
                Cell::Str(severity_label(f.severity).to_string()),
                Cell::Str(f.desc.clone()),
            ]);
        }
    }
    Ok(out)
}

const fn severity_label(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Fyi => "fyi",
    }
}
