//! `validate_ags(path[, dict_version][, warnings][, fyi])` — opt-in AGS4
//! validation as a queryable table.
//!
//! Wraps the clean-room `laterite-ags4-validator` (`check_file`). One row per
//! finding: `(rule, line, group, severity, desc)`. This is **never** a gate on
//! `read_ags` — reads assume the file is valid and only surface *structural*
//! malformation; call `validate_ags` explicitly when you want the full rule
//! check. There is no repair surface (mutation stays in `lat-check`/the library).
//!
//! The dictionary edition is auto-detected from `TRAN_AGS` by default; the
//! optional `dict_version` **named** parameter forces a bundled edition regardless
//! (e.g. to check a file's forward/backward compatibility against a specific
//! schema). Errors **and warnings** are returned by default — matching the
//! library's `validate()` default and `lat-check`; the FYI tier is opt-in, and
//! warnings can be switched off, via the boolean `warnings` / `fyi` knobs:
//!   - `validate_ags(path)`                        — errors + warnings, edition from `TRAN_AGS`.
//!   - `validate_ags(path, dict_version := '4.2')` — force '4.0.3'/'4.0.4'/'4.1'/
//!     '4.1.1'/'4.2'.
//!   - `validate_ags(path, warnings := false)`     — errors only.
//!   - `validate_ags(path, fyi := true)`           — also include the FYI tier.
//!
//! (Named, not extra positionals, because DuckDB has no same-name table-function
//! overloads — see `rows::register_rows`.)

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
        &[
            ("dict_version", TypeId::Varchar),
            ("warnings", TypeId::Boolean),
            ("fyi", TypeId::Boolean),
            ("encoding", TypeId::Varchar),
        ],
        vec![
            ("rule", TypeId::Varchar),
            ("line", TypeId::BigInt),
            ("group", TypeId::Varchar),
            ("severity", TypeId::Varchar),
            ("desc", TypeId::Varchar),
        ],
        |bind| {
            let path = unsafe { bind.get_parameter_value(0) }.as_str()?;
            // `dict_version` named param: absent → auto-detect from TRAN_AGS; an
            // explicit, non-blank value forces that edition. The forced string is
            // also what a cert must match to cover this request.
            let forced = match unsafe { bind.get_named_parameter_value("dict_version") }.as_str() {
                Ok(e) if !e.trim().is_empty() => Some(e.trim().to_string()),
                _ => None,
            };
            // Severity knobs: warnings are ON by default (joining the warnings-on
            // family — the library's `validate()` default and `lat-check`), FYI is
            // opt-in. Absent `warnings` → null → true; pass `warnings := false` for
            // an error-only check. Absent `fyi` → null → false (`as_bool_or`).
            let want_warnings =
                unsafe { bind.get_named_parameter_value("warnings") }.as_bool_or(true);
            let want_fyi = unsafe { bind.get_named_parameter_value("fyi") }.as_bool_or(false);
            // Optional `encoding` named param (#294 #12): default UTF-8; a WHATWG
            // label decodes the source before the byte→text step of the rule check.
            let encoding = super::source::resolve_encoding(bind)?;
            // Certificate fast-path: a fresh cert from THIS engine proves the file
            // ERROR-clean, so it covers an *error-only* request (`warnings := false`,
            // no `fyi`) — return clean (zero findings) without re-running the rule
            // pass over (potentially) hundreds of MB. The default (warnings-on) and
            // any `warnings`/`fyi` request ask for more than the cert vouches for, so
            // they always run the engine (mirrors the library's `.validate()`, which
            // never short-circuits a cert for the warning/FYI tiers). `validate_ags`
            // never runs Rule 20's on-disk half, so the request's `check_files` is false.
            // The cert fast-path only serves the default UTF-8 read: a cert records
            // its check profile but NOT the encoding it was minted under, so a
            // non-UTF-8 request re-runs the engine rather than trust a possibly
            // differently-decoded clean verdict.
            let ctx = unsafe { bind.get_client_context() };
            if !want_warnings
                && !want_fyi
                && encoding == encoding_rs::UTF_8
                && cert::clean_verdict_certified(&ctx, &path, false, forced.as_deref())
            {
                return Ok(Vec::new());
            }
            let opts = CheckOptions {
                dict_version: match &forced {
                    Some(e) => Some(cert::parse_edition(e)?),
                    None => None,
                },
                include_warnings: want_warnings,
                include_fyi: want_fyi,
                encoding,
                ..CheckOptions::default()
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
