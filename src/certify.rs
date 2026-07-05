//! `certify_ags(path[, dict_version := '4.2'])` — validate an AGS4 file and, when it
//! comes back clean, mint its `.ags.idx` **certificate** (a byte-offset index +
//! the validation provenance) beside it. One-shot validate-then-mint, because SQL
//! is stateless: there's no handle to validate first and certify after (the way
//! the Python `Ags4File.validate().certify()` chain does), so this does both.
//!
//! It returns a one-row status report rather than erroring on an invalid file: a
//! file with error findings is **not** certified (a cert asserts clean), and the
//! row says so with the error count, pointing you at `validate_ags`. Querying it:
//!
//! ```sql
//! SELECT certified, groups, message FROM certify_ags('site.ags');
//! -- force the edition the cert is stamped + checked against:
//! SELECT * FROM certify_ags('site.ags', dict_version := '4.2');
//! ```
//!
//! Validation reads the **local path** (the validator uses `std::fs`, exactly like
//! `validate_ags`), so certify is local-oriented; the source *bytes* hashed +
//! indexed into the cert are read back through the VFS so the SHA covers precisely
//! what a later reader will see. The stamp's checker identity (`laterite_ags4` +
//! the engine VERSION) is the SAME one the Python wheel writes, so a cert minted
//! here is trusted by `read_ags`/`validate_ags` and by Python, and vice versa.
//!
//! A non-UTF-8 source is decoded via the optional `encoding` named param before
//! validation (`certify_ags(path, encoding := 'windows-1252')`), the same knob
//! `read_ags`/`validate_ags` take — the cert still hashes the RAW source bytes, so
//! a later reader verifies against exactly what's on disk.
//!
//! `certify_ags_text(content[, dict_version])` is the content variant — it
//! validates AGS4 passed as a VARCHAR and, when clean, returns the certificate
//! **JSON in a `cert` column** rather than writing an `.ags.idx` (there is no path
//! to write beside). The in-memory analog of Python's `Ags4File.certify_bytes()`.
//! Like the other `_text` verbs it has no `encoding` param (the content is already
//! UTF-8) and does not apply the path verb's private 4.0.3→4.0.4 guard.

use std::path::Path;

use laterite_ags4_core::index::{Sidecar, ValidationStamp};
use laterite_ags4_validator::{
    CheckOptions, DictVersion, Dictionary, Findings, check_file_with_dict, parse,
    resolve_dict_version, rules, tran_ags_of,
};
use quack_rs::client_context::ClientContext;
use quack_rs::prelude::*;

use super::cert::{self, VALIDATOR_NAME};
use super::rows::{Cell, register_rows};

pub fn register(con: &Connection) -> ExtResult<()> {
    register_rows(
        con,
        "certify_ags",
        1,
        &[
            ("dict_version", TypeId::Varchar),
            ("encoding", TypeId::Varchar),
        ],
        vec![
            ("path", TypeId::Varchar),
            ("index_path", TypeId::Varchar),
            ("certified", TypeId::Boolean),
            ("groups", TypeId::BigInt),
            ("errors", TypeId::BigInt),
            ("warnings", TypeId::BigInt),
            ("fyi", TypeId::BigInt),
            ("dict_version", TypeId::Varchar),
            ("message", TypeId::Varchar),
        ],
        |bind| {
            let path = unsafe { bind.get_parameter_value(0) }.as_str()?;
            // `dict_version` named param: absent → auto-detect from TRAN_AGS; an
            // explicit, non-blank value forces (and stamps) that edition.
            let forced = match unsafe { bind.get_named_parameter_value("dict_version") }.as_str() {
                Ok(e) if !e.trim().is_empty() => Some(cert::parse_edition(&e)?),
                _ => None,
            };
            // Optional `encoding` named param: default UTF-8; a WHATWG label decodes
            // the source before validation (the cert still hashes the RAW bytes).
            let encoding = super::source::resolve_encoding(bind)?;
            let ctx = unsafe { bind.get_client_context() };
            run(&ctx, &path, forced, encoding)
        },
    )
}

fn run(
    ctx: &ClientContext,
    path: &str,
    forced: Option<DictVersion>,
    encoding: &'static encoding_rs::Encoding,
) -> Result<Vec<Vec<Cell>>, ExtensionError> {
    let edition_forced = forced.is_some();
    let opts = CheckOptions {
        dict_version: forced,
        encoding,
        ..CheckOptions::default()
    };
    // Validate the local path; `check_file_with_dict` also reports the resolved
    // edition (auto from TRAN_AGS, or the forced one) we stamp into the cert.
    let (findings, dv, _res) = check_file_with_dict(Path::new(path), &opts)
        .map_err(|e| ExtensionError::new(format!("certify_ags: '{path}': {e}")))?;
    let (errors, warnings, fyi) = cert::severity_counts(&findings);
    let edition = dv.as_str().to_string();
    let idx_path = cert::idx_path_for(path);

    if errors > 0 {
        // A certificate asserts a clean file — refuse to mint, report instead.
        return Ok(vec![status(
            path,
            &idx_path,
            false,
            0,
            errors,
            warnings,
            fyi,
            &edition,
            format!(
                "not certified: {errors} error finding(s) — run validate_ags('{path}') and fix them first"
            ),
        )]);
    }

    // Clean → assemble the cert from the source BYTES (read back through the VFS so
    // the SHA + byte offsets cover exactly what a reader will see), stamping this
    // engine's identity, then write `<path>.idx`.
    let bytes = super::source::read_bytes(ctx, path)?;
    let stamp = ValidationStamp {
        validator: VALIDATOR_NAME.to_string(),
        validator_version: laterite_ags4_validator::VERSION.to_string(),
        compat: None,       // native validator, never the python-ags4 compat shim
        check_files: false, // validate_ags doesn't run Rule 20's on-disk half
        edition_forced,
        checked_at: cert::now_rfc3339(),
        warnings: warnings as u32,
        fyi: fyi as u32,
    };
    let sidecar = Sidecar::assemble(&bytes, edition.clone(), stamp)
        .map_err(|e| ExtensionError::new(format!("certify_ags: indexing '{path}': {e}")))?;
    let json = sidecar.to_json().map_err(|e| {
        ExtensionError::new(format!("certify_ags: serializing cert for '{path}': {e}"))
    })?;
    super::source::write_bytes(ctx, &idx_path, &json)?;

    let groups = sidecar.order.len() as i64;
    Ok(vec![status(
        path,
        &idx_path,
        true,
        groups,
        0,
        warnings,
        fyi,
        &edition,
        format!("certified {groups} group(s) → {idx_path}"),
    )])
}

/// Build the single status row in declared column order.
#[allow(clippy::too_many_arguments)] // flat status row; positional by construction
fn status(
    path: &str,
    idx_path: &str,
    certified: bool,
    groups: i64,
    errors: u64,
    warnings: u64,
    fyi: u64,
    edition: &str,
    message: String,
) -> Vec<Cell> {
    vec![
        Cell::Str(path.to_string()),
        Cell::Str(idx_path.to_string()),
        Cell::Bool(certified),
        Cell::Int(groups),
        Cell::Int(errors as i64),
        Cell::Int(warnings as i64),
        Cell::Int(fyi as i64),
        Cell::Str(edition.to_string()),
        Cell::Str(message),
    ]
}

/// Build `certify_ags_text(content[, dict_version])` — the content variant. There
/// is no path to write an `.ags.idx` beside, so a clean file's certificate comes
/// back as JSON in the `cert` column (the in-memory analog of Python's
/// `Ags4File.certify_bytes()`). No `encoding` param (the content is UTF-8).
pub fn register_text(con: &Connection) -> ExtResult<()> {
    register_rows(
        con,
        "certify_ags_text",
        1,
        &[("dict_version", TypeId::Varchar)],
        vec![
            ("certified", TypeId::Boolean),
            ("groups", TypeId::BigInt),
            ("errors", TypeId::BigInt),
            ("warnings", TypeId::BigInt),
            ("fyi", TypeId::BigInt),
            ("dict_version", TypeId::Varchar),
            ("cert", TypeId::Varchar),
            ("message", TypeId::Varchar),
        ],
        |bind| {
            let content = unsafe { bind.get_parameter_value(0) }.as_str()?;
            let forced = match unsafe { bind.get_named_parameter_value("dict_version") }.as_str() {
                Ok(e) if !e.trim().is_empty() => Some(cert::parse_edition(&e)?),
                _ => None,
            };
            run_text(&content, forced)
        },
    )
}

/// Validate `content` in memory and, when clean, assemble + return its certificate
/// JSON (assembled over the content bytes — exactly what was validated). Mirrors
/// [`run`] minus the file read/write and the private 4.0.3→4.0.4 guard.
fn run_text(content: &str, forced: Option<DictVersion>) -> Result<Vec<Vec<Cell>>, ExtensionError> {
    let edition_forced = forced.is_some();
    let parsed = parse::parse_str(content).map_err(|e| {
        ExtensionError::new(format!(
            "certify_ags_text: input did not parse as AGS4 ({e})"
        ))
    })?;
    let (dv, _res) = resolve_dict_version(forced, tran_ags_of(&parsed).as_deref())
        .map_err(|e| ExtensionError::new(format!("certify_ags_text: {e}")))?;
    let dict = Dictionary::bundled(dv);
    let opts = CheckOptions {
        dict_version: forced,
        ..CheckOptions::default()
    };
    let mut found = Findings::new();
    rules::run_all(&parsed, &dict, &opts, None, &mut found);
    let (errors, warnings, fyi) = cert::severity_counts(&found);
    let edition = dv.as_str().to_string();

    if errors > 0 {
        // A certificate asserts a clean file — refuse to mint, report instead.
        return Ok(vec![status_text(
            false,
            0,
            errors,
            warnings,
            fyi,
            &edition,
            None,
            format!(
                "not certified: {errors} error finding(s) — run validate_ags_text(...) and fix them first"
            ),
        )]);
    }

    // Clean → assemble the cert from the CONTENT bytes (exactly what was validated,
    // and what a reader passing the same text to read_ags_text would see), stamp
    // this engine's identity, and return the JSON — nothing is written to disk.
    let stamp = ValidationStamp {
        validator: VALIDATOR_NAME.to_string(),
        validator_version: laterite_ags4_validator::VERSION.to_string(),
        compat: None,
        check_files: false,
        edition_forced,
        checked_at: cert::now_rfc3339(),
        warnings: warnings as u32,
        fyi: fyi as u32,
    };
    let sidecar = Sidecar::assemble(content.as_bytes(), edition.clone(), stamp)
        .map_err(|e| ExtensionError::new(format!("certify_ags_text: indexing input: {e}")))?;
    let json = sidecar
        .to_json()
        .map_err(|e| ExtensionError::new(format!("certify_ags_text: serializing cert: {e}")))?;
    let json_str = String::from_utf8(json)
        .map_err(|e| ExtensionError::new(format!("certify_ags_text: cert JSON not UTF-8: {e}")))?;
    let groups = sidecar.order.len() as i64;
    Ok(vec![status_text(
        true,
        groups,
        0,
        warnings,
        fyi,
        &edition,
        Some(json_str),
        format!("certified {groups} group(s) (cert JSON in the `cert` column)"),
    )])
}

/// The `certify_ags_text` status row: no path/index_path (nothing written); a
/// `cert` column carries the JSON on success, `NULL` otherwise.
#[allow(clippy::too_many_arguments)] // flat status row; positional by construction
fn status_text(
    certified: bool,
    groups: i64,
    errors: u64,
    warnings: u64,
    fyi: u64,
    edition: &str,
    cert_json: Option<String>,
    message: String,
) -> Vec<Cell> {
    vec![
        Cell::Bool(certified),
        Cell::Int(groups),
        Cell::Int(errors as i64),
        Cell::Int(warnings as i64),
        Cell::Int(fyi as i64),
        Cell::Str(edition.to_string()),
        cert_json.map_or(Cell::Null, Cell::Str),
        Cell::Str(message),
    ]
}
