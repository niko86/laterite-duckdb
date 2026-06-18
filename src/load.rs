//! `load_ags_script(path)` — generate the SQL to materialise an AGS4 file into
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
use quack_rs::prelude::*;

use super::rows::{Cell, register_rows};
use super::source::read_parsed;

pub fn register(con: &Connection) -> ExtResult<()> {
    register_rows(
        con,
        "load_ags_script",
        1,
        &[],
        vec![("seq", TypeId::BigInt), ("stmt", TypeId::Varchar)],
        |bind| {
            let path = unsafe { bind.get_parameter_value(0) }.as_str()?;
            // SAFETY: live bind-info → the query's client context → the VFS.
            let ctx = unsafe { bind.get_client_context() };
            let parsed = read_parsed(&ctx, &path)?;
            let reg = registry();
            let lit = path.replace('\'', "''"); // SQL string-literal escape

            let mut out: Vec<Vec<Cell>> = Vec::new();
            for code in &parsed.order {
                let tbl = format!("ags_{}", code.to_lowercase());
                let mut stmts = vec![
                    format!("CREATE TABLE {tbl} AS SELECT * FROM read_ags('{lit}', '{code}');"),
                    format!("CREATE INDEX {tbl}_id_idx ON {tbl}(_id);"),
                ];
                if reg.get(code).and_then(|d| d.parent.as_ref()).is_some() {
                    stmts.push(format!(
                        "CREATE INDEX {tbl}_parent_idx ON {tbl}(_parent_id);"
                    ));
                }
                for s in stmts {
                    let seq = out.len() as i64;
                    out.push(vec![Cell::Int(seq), Cell::Str(s)]);
                }
            }
            Ok(out)
        },
    )
}
