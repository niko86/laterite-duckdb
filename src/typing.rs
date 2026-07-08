//! AGS type code → DuckDB output column type + typed cell — the one typing
//! authority the read functions share.
//!
//! Every cell is routed through `laterite_types::parse_value` — the single
//! typing authority already shared by `laterite.read`, the wasm explorer, and
//! the `.ags5db` writer — so `read_ags` types a column identically to those
//! hosts *by construction*, not by re-implementation.
//!
//! The numeric/bool families are emitted natively (the flagship: `2DP` →
//! DOUBLE, `0DP` → BIGINT, `YN` → BOOLEAN, `ID`/`X`/… → VARCHAR); the temporal
//! families (`DT`/date/time) are carried through as their canonical VARCHAR
//! string. Native TIMESTAMP/DATE/TIME typing is a deliberate follow-up — the
//! canonical-string → epoch-unit conversion is deferred so the flagship
//! (born-typed numerics) stays unambiguous.

use laterite_types::{CanonicalType, canonical_type, parse_value};

use super::ffi_table::{Cell, ColType};

/// The physical kind a heading column is emitted as.
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

    /// The harness column type this emit kind declares.
    pub fn col_type(self) -> ColType {
        match self {
            Emit::Varchar => ColType::Varchar,
            Emit::Double => ColType::Double,
            Emit::BigInt => ColType::BigInt,
            Emit::Bool => ColType::Boolean,
        }
    }
}

/// Type one raw AGS cell as a [`Cell`] under `kind`. The raw string is routed
/// through `parse_value` (the shared authority), then mapped to the physical
/// variant `kind` declared. A JSON-null (whatever `parse_value` deemed
/// empty/unparseable) becomes `Cell::Null` — the born-typed behaviour
/// `laterite.read` gives (a non-conforming numeric cell is NULL, never an
/// error).
pub fn cell_for(raw: Option<&str>, ags_type: &str, kind: Emit) -> Cell {
    let value = parse_value(raw, ags_type);
    if value.is_null() {
        return Cell::Null;
    }
    match kind {
        Emit::Double => value.as_f64().map_or(Cell::Null, Cell::Double),
        Emit::BigInt => value.as_i64().map_or(Cell::Null, Cell::Int),
        Emit::Bool => value.as_bool().map_or(Cell::Null, Cell::Bool),
        Emit::Varchar => value
            .as_str()
            .map_or(Cell::Null, |s| Cell::Str(s.to_string())),
    }
}
