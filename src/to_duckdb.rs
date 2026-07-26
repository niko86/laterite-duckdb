//! `to_duckdb(ags_path, out_path)` — generate the SQL to PERSIST an AGS4 file as
//! a standalone DuckDB database at `out_path`: one indexed `ags_<group>` table
//! per group, keyed by the deterministic `_id`/`_parent_id`.
//!
//! Like [`super::load`] it RETURNS the `(seq, stmt)` script rather than running
//! the DDL itself — the C extension API can't cleanly issue `CREATE TABLE` /
//! `ATTACH` from inside a table function (see `load.rs`). The difference is the
//! wrap: `load_ags` materialises into the CURRENT database, whereas `to_duckdb`
//! `ATTACH`es `out_path` as a named database, creates each table INTO it, then
//! `DETACH`es — so you persist to any file from any session. Run it e.g. with
//! `duckdb -c "$(SELECT string_agg(stmt, e'\n' ORDER BY seq) FROM
//! to_duckdb('site.ags','site.duckdb'))"`, or `string_agg` + execute from a host
//! driver.
//!
//! The persisted tables carry `_id`/`_parent_id` byte-identical to `read_ags`,
//! so the file equals the libraries' `to_duckdb()` output — one store the
//! Python / Node / browser handles and this extension all agree on.

use laterite_ags4_core::registry::registry;
use libduckdb_sys as ffi;

use super::ffi_table::{Bind, Cell, ColType, register_table};
use super::source::{Vfs, read_parsed};

/// Register `to_duckdb(ags_path, out_path)`.
pub fn register(con: ffi::duckdb_connection) -> Result<(), Box<dyn std::error::Error>> {
    register_table(con, "to_duckdb", 2, &[], |bind: &Bind| {
        let path = bind.param_str(0)?;
        let out = bind.param_str(1)?;
        // SAFETY: the producer runs during bind, so the raw bind info is live
        // and its client context (the VFS) is valid for this call.
        let vfs = unsafe { Vfs::from_bind(bind.raw_info()) }?;
        let parsed = read_parsed(&vfs, &path)?;
        let reg = registry();
        let lit = path.replace('\'', "''"); // SQL string-literal escape (source)
        let out_lit = out.replace('\'', "''"); // …and the destination path

        let columns = vec![("seq", ColType::BigInt), ("stmt", ColType::Varchar)];

        let mut rows: Vec<Vec<Cell>> = Vec::new();
        // ATTACH the destination FIRST; every table is created into it, and it's
        // DETACHed last (flushing a complete, standalone .duckdb).
        rows.push(vec![
            Cell::Int(0),
            Cell::Str(format!("ATTACH '{out_lit}' AS _lat_out;")),
        ]);
        for code in &parsed.order {
            let lc = code.to_lowercase();
            let tbl = format!("_lat_out.ags_{lc}"); // schema-qualified into the attached db
            let mut stmts = vec![
                format!("CREATE TABLE {tbl} AS SELECT * FROM read_ags('{lit}', '{code}');"),
                // Index name is unqualified — DuckDB places it in the table's own
                // (attached) database. Same _id / _parent_id indexing as load_ags.
                format!("CREATE INDEX ags_{lc}_id_idx ON {tbl}(_id);"),
            ];
            // Root groups have no registry parent → no `_parent_id` index.
            if reg.get(code).and_then(|d| d.parent.as_ref()).is_some() {
                stmts.push(format!(
                    "CREATE INDEX ags_{lc}_parent_idx ON {tbl}(_parent_id);"
                ));
            }
            for s in stmts {
                let seq = rows.len() as i64;
                rows.push(vec![Cell::Int(seq), Cell::Str(s)]);
            }
        }
        let seq = rows.len() as i64;
        rows.push(vec![
            Cell::Int(seq),
            Cell::Str("DETACH _lat_out;".to_string()),
        ]);

        Ok((columns, rows))
    })
}
