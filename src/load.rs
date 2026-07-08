//! `load_ags(path)` — generate the SQL to materialise an AGS4 file into
//! persistent, indexed DuckDB tables (one `ags_<group>` per group), for the
//! repeat-/remote-query case.
//!
//! It returns `(seq, stmt)` rows you execute in order (e.g. `string_agg` them
//! and feed to `execute`/the CLI's `.read`) rather than running DDL from inside
//! a table function — the C extension API doesn't cleanly allow a function to
//! issue `CREATE TABLE`, so generating the script keeps the behaviour explicit
//! and transparent.
//!
//! Each group → `CREATE TABLE ags_<g> AS SELECT * FROM read_ags(path,'G')` plus
//! an index on `_id` (and `_parent_id` for non-root groups). The deterministic
//! keys mean the parent/child relationship holds in the data; explicit PK/FK
//! *constraints* are omitted (DuckDB CTAS can't carry them) — the indexes give
//! the join performance.

use laterite_ags4_core::registry::registry;
use libduckdb_sys as ffi;

use super::ffi_table::{Bind, Cell, ColType, register_table};
use super::source::{Vfs, read_parsed};

/// Register `load_ags(path)`.
pub fn register(con: ffi::duckdb_connection) -> Result<(), Box<dyn std::error::Error>> {
    register_table(con, "load_ags", 1, &[], |bind: &Bind| {
        let path = bind.param_str(0)?;
        // SAFETY: the producer runs during bind, so the raw bind info is live
        // and its client context (the VFS) is valid for this call.
        let vfs = unsafe { Vfs::from_bind(bind.raw_info()) }?;
        let parsed = read_parsed(&vfs, &path)?;
        let reg = registry();
        let lit = path.replace('\'', "''"); // SQL string-literal escape

        let columns = vec![("seq", ColType::BigInt), ("stmt", ColType::Varchar)];

        let mut rows: Vec<Vec<Cell>> = Vec::new();
        for code in &parsed.order {
            let tbl = format!("ags_{}", code.to_lowercase());
            let mut stmts = vec![
                format!("CREATE TABLE {tbl} AS SELECT * FROM read_ags('{lit}', '{code}');"),
                format!("CREATE INDEX {tbl}_id_idx ON {tbl}(_id);"),
            ];
            // Root groups have no registry parent → no `_parent_id` index.
            if reg.get(code).and_then(|d| d.parent.as_ref()).is_some() {
                stmts.push(format!(
                    "CREATE INDEX {tbl}_parent_idx ON {tbl}(_parent_id);"
                ));
            }
            for s in stmts {
                let seq = rows.len() as i64;
                rows.push(vec![Cell::Int(seq), Cell::Str(s)]);
            }
        }

        Ok((columns, rows))
    })
}
