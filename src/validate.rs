//! `validate_ags(path[, edition])` — opt-in AGS4 validation as a queryable table.
//!
//! Wraps the clean-room `laterite-ags4-validator` (`check_file`). One row per
//! finding: `(rule, line, group, severity, desc)`. This is **never** a gate on
//! `read_ags` — reads assume the file is valid and only surface *structural*
//! malformation; call `validate_ags` explicitly when you want the full rule
//! check. There is no repair surface (mutation stays in `lat-check`/the library).
//!
//! The edition is auto-detected from `TRAN_AGS` by default; the optional
//! `edition` **named** parameter forces a bundled dictionary edition regardless
//! (e.g. to check a file's forward/backward compatibility against a specific
//! schema):
//!   - `validate_ags(path)`                 — auto-detect from `TRAN_AGS`.
//!   - `validate_ags(path, edition := '4.2')` — force '4.0.3'/'4.0.4'/'4.1'/
//!     '4.1.1'/'4.2'. (Named, not a 2nd positional, because DuckDB has no
//!     same-name table-function overloads — see `rows::register_rows`.)

use std::path::Path;

use laterite_ags4_validator::findings::Severity;
use laterite_ags4_validator::{CheckOptions, check_file};
use quack_rs::prelude::*;

use super::cert;
use super::rows::{Cell, register_rows};

pub fn register(con: &Connection) -> ExtResult<()> {
    register_rows(
        con,
        "validate_ags",
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
            // `edition` named param: absent → auto-detect from TRAN_AGS; an
            // explicit, non-blank value forces that edition. The forced string is
            // also what a cert must match to cover this request.
            let forced = match unsafe { bind.get_named_parameter_value("edition") }.as_str() {
                Ok(e) if !e.trim().is_empty() => Some(e.trim().to_string()),
                _ => None,
            };
            // Certificate fast-path: a fresh cert from THIS engine whose profile
            // covers the request already proves the file clean — return clean
            // (zero findings) without re-running the rule pass over (potentially)
            // hundreds of MB. `validate_ags` never runs Rule 20's on-disk half, so
            // the request's `check_files` is false.
            let ctx = unsafe { bind.get_client_context() };
            if cert::clean_verdict_certified(&ctx, &path, false, forced.as_deref()) {
                return Ok(Vec::new());
            }
            let opts = match &forced {
                Some(e) => CheckOptions {
                    dict_version: Some(cert::parse_edition(e)?),
                    ..CheckOptions::default()
                },
                None => CheckOptions::default(),
            };
            run(&path, &opts)
        },
    )
}

/// Run the validator and flatten its findings into output rows.
fn run(path: &str, opts: &CheckOptions) -> Result<Vec<Vec<Cell>>, ExtensionError> {
    let findings = check_file(Path::new(path), opts)
        .map_err(|e| ExtensionError::new(format!("validate_ags: '{path}': {e}")))?;
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
