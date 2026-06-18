//! A helper for "compute every row at bind, then stream it" table functions —
//! the shape every `ags_*` metadata function takes. A function supplies its
//! column schema + a row producer; this wires the quack-rs bind/scan lifecycle
//! and the typed cell writes once. (`read_ags` itself is bespoke — it streams
//! per-file-typed columns + deterministic keys — so it doesn't use this.)

use quack_rs::prelude::*;
use quack_rs::vector::vector_size;

/// One output cell. Metadata columns are VARCHAR / BIGINT / BOOLEAN (+ NULL);
/// the column's declared `TypeId` must match the variant the producer emits.
pub enum Cell {
    Null,
    Str(String),
    Int(i64),
    Bool(bool),
}

/// Scan state: the precomputed rows + a streaming cursor.
pub struct RowsState {
    columns: usize,
    rows: Vec<Vec<Cell>>,
    cursor: usize,
}

/// Register a table function taking `n_params` positional `VARCHAR` args (plus
/// any optional `named` params — `name := value`, the only way DuckDB does
/// *optional* args, since `duckdb_register_table_function` rejects same-name
/// overloads) whose rows are produced once at bind. `columns` are `(name, type)`
/// in output order; `producer` turns the bind info into the full row set (and
/// reads any named params itself via `BindInfo::get_named_parameter_value`).
pub fn register_rows<F>(
    con: &Connection,
    name: &str,
    n_params: usize,
    named: &[(&'static str, TypeId)],
    columns: Vec<(&'static str, TypeId)>,
    producer: F,
) -> ExtResult<()>
where
    F: Fn(&BindInfo) -> Result<Vec<Vec<Cell>>, ExtensionError> + Send + Sync + 'static,
{
    let mut builder = TableFunctionBuilder::new(name);
    for _ in 0..n_params {
        builder = builder.param(TypeId::Varchar);
    }
    for (param_name, param_type) in named {
        builder = builder.named_param(param_name, *param_type);
    }
    let builder = builder
        .with_state::<RowsState, _>(move |bind| {
            for (col_name, col_type) in &columns {
                bind.add_result_column(col_name, *col_type);
            }
            let rows = producer(bind)?;
            bind.set_cardinality(rows.len() as u64, true);
            Ok(RowsState {
                columns: columns.len(),
                rows,
                cursor: 0,
            })
        })
        .scan(scan)
        .build()?;
    // SAFETY: freshly built; DuckDB owns the closures for the function lifetime.
    unsafe { con.register_table(builder) }
}

fn scan(state: &mut RowsState, chunk: &DataChunk) -> Result<(), ExtensionError> {
    let cap = vector_size() as usize;
    let start = state.cursor;
    let n = (state.rows.len() - start).min(cap);
    if n == 0 {
        // SAFETY: end-of-stream on the valid output chunk.
        unsafe { chunk.set_size(0) };
        return Ok(());
    }
    for col in 0..state.columns {
        let mut w = unsafe { chunk.writer(col) };
        for i in 0..n {
            // SAFETY: col < column_count; i < n <= capacity; the cell variant
            // matches the column's declared TypeId by construction.
            unsafe { write_cell(&mut w, i, &state.rows[start + i][col]) };
        }
    }
    // SAFETY: every column written for rows 0..n, n <= capacity.
    unsafe { chunk.set_size(n) };
    state.cursor += n;
    Ok(())
}

unsafe fn write_cell(w: &mut VectorWriter, idx: usize, cell: &Cell) {
    match cell {
        Cell::Null => unsafe { w.set_null(idx) },
        Cell::Str(s) => unsafe { w.write_varchar(idx, s) },
        Cell::Int(n) => unsafe { w.write_i64(idx, *n) },
        Cell::Bool(b) => unsafe { w.write_bool(idx, *b) },
    }
}
