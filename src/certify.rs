//! `certify_ags(path[, edition := '4.2'])` — validate an AGS4 file and, when it
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
//! SELECT * FROM certify_ags('site.ags', edition := '4.2');
//! ```
//!
//! Validation reads the **local path** (the validator uses `std::fs`, exactly like
//! `validate_ags`), so certify is local-oriented; the source *bytes* hashed +
//! indexed into the cert are read back through the VFS so the SHA covers precisely
//! what a later reader will see. The stamp's checker identity (`laterite_ags4` +
//! the engine VERSION) is the SAME one the Python wheel writes, so a cert minted
//! here is trusted by `read_ags`/`validate_ags` and by Python, and vice versa.

use std::path::Path;

use laterite_ags4_core::index::{Sidecar, ValidationStamp};
use laterite_ags4_validator::{CheckOptions, DictVersion, check_file_with_dict};
use quack_rs::client_context::ClientContext;
use quack_rs::prelude::*;

use super::cert::{self, VALIDATOR_NAME};
use super::rows::{Cell, register_rows};

pub fn register(con: &Connection) -> ExtResult<()> {
    register_rows(
        con,
        "certify_ags",
        1,
        &[("edition", TypeId::Varchar)],
        vec![
            ("path", TypeId::Varchar),
            ("index_path", TypeId::Varchar),
            ("certified", TypeId::Boolean),
            ("groups", TypeId::BigInt),
            ("errors", TypeId::BigInt),
            ("warnings", TypeId::BigInt),
            ("fyi", TypeId::BigInt),
            ("edition", TypeId::Varchar),
            ("message", TypeId::Varchar),
        ],
        |bind| {
            let path = unsafe { bind.get_parameter_value(0) }.as_str()?;
            // `edition` named param: absent → auto-detect from TRAN_AGS; an
            // explicit, non-blank value forces (and stamps) that edition.
            let forced = match unsafe { bind.get_named_parameter_value("edition") }.as_str() {
                Ok(e) if !e.trim().is_empty() => Some(cert::parse_edition(&e)?),
                _ => None,
            };
            let ctx = unsafe { bind.get_client_context() };
            run(&ctx, &path, forced)
        },
    )
}

fn run(
    ctx: &ClientContext,
    path: &str,
    forced: Option<DictVersion>,
) -> Result<Vec<Vec<Cell>>, ExtensionError> {
    let edition_forced = forced.is_some();
    let opts = CheckOptions {
        dict_version: forced,
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
