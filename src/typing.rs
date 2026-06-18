//! AGS type code → DuckDB output column type + value writer (P1 subset).
//!
//! Every cell is routed through `laterite_types::parse_value` — the single
//! typing authority already shared by `laterite.read`, the wasm explorer, and
//! the `.ags5db` writer — so `read_ags` types a column identically to those
//! hosts *by construction*, not by re-implementation.
//!
//! P1 emits the numeric/bool families natively (the flagship: `2DP` → DOUBLE,
//! `0DP` → BIGINT, `YN` → BOOLEAN, `ID`/`X`/… → VARCHAR) and carries the
//! temporal families (`DT`/date/time) through as their canonical VARCHAR
//! string. Native TIMESTAMP/DATE/TIME typing is a deliberate follow-up — the
//! writers exist (`VectorWriter::write_timestamp`/`write_date`/`write_time`),
//! but the canonical-string → epoch-unit conversion is deferred so P1 stays
//! tight and the flagship (born-typed numerics) is unambiguous.

use laterite_types::{CanonicalType, canonical_type};
use quack_rs::prelude::{TypeId, VectorWriter};
use serde_json::Value;

/// The physical kind a column is emitted as in P1.
#[derive(Clone, Copy)]
pub enum Emit {
    Varchar,
    Double,
    BigInt,
    Bool,
}

impl Emit {
    /// The emit kind for an AGS type code. Temporal, string, enum, and unknown
    /// codes all land on VARCHAR (see the module note on temporal).
    pub fn of(ags_type: &str) -> Self {
        match canonical_type(ags_type) {
            Some(CanonicalType::Decimal) => Emit::Double,
            Some(CanonicalType::Integer) => Emit::BigInt,
            Some(CanonicalType::Bool) => Emit::Bool,
            _ => Emit::Varchar,
        }
    }

    pub fn type_id(self) -> TypeId {
        match self {
            Emit::Varchar => TypeId::Varchar,
            Emit::Double => TypeId::Double,
            Emit::BigInt => TypeId::BigInt,
            Emit::Bool => TypeId::Boolean,
        }
    }
}

/// Write a parsed value into `writer` at row `idx` as `kind`. A JSON-null
/// (whatever `parse_value` deemed empty/unparseable) becomes SQL NULL — the
/// born-typed behaviour `laterite.read` gives (a non-conforming numeric cell
/// is NULL, never an error).
///
/// # Safety
/// `writer` must target the column declared with `kind.type_id()`, and `idx`
/// must be within the output chunk's capacity.
pub unsafe fn write_value(writer: &mut VectorWriter, idx: usize, value: &Value, kind: Emit) {
    if value.is_null() {
        unsafe { writer.set_null(idx) };
        return;
    }
    match kind {
        Emit::Double => match value.as_f64() {
            Some(f) => unsafe { writer.write_f64(idx, f) },
            None => unsafe { writer.set_null(idx) },
        },
        Emit::BigInt => match value.as_i64() {
            Some(n) => unsafe { writer.write_i64(idx, n) },
            None => unsafe { writer.set_null(idx) },
        },
        Emit::Bool => match value.as_bool() {
            Some(b) => unsafe { writer.write_bool(idx, b) },
            None => unsafe { writer.set_null(idx) },
        },
        Emit::Varchar => match value.as_str() {
            Some(s) => unsafe { writer.write_varchar(idx, s) },
            None => unsafe { writer.set_null(idx) },
        },
    }
}
