//! The generic raw-FFI table-function harness — the one place all `unsafe`
//! DuckDB table-function plumbing lives.
//!
//! Every function this extension exposes takes the same shape: **compute all
//! rows at bind, then stream them a vector-chunk at a time in func**. This
//! module wires that lifecycle once, in raw `libduckdb-sys`, so each function
//! only supplies a `producer` closure — `(&Bind) -> (columns, rows)`.
//!
//! ## Why raw FFI (and not duckdb-rs's safe `VTab`)
//!
//! The VFS-reading binds must call `duckdb_table_function_get_client_context`,
//! which takes the raw `duckdb_bind_info`. duckdb-rs's safe `BindInfo` keeps
//! that pointer private and exposes no accessor, so a `VTab::bind(&BindInfo)`
//! can never reach the client context. The escape hatch is a **hand-written
//! `unsafe extern "C" fn bind(info: duckdb_bind_info)`** that holds the raw
//! pointer. duckdb-rs also exposes no public way to register a hand-built
//! `TableFunction` (its `Connection::register_table_function` only takes a
//! `T: VTab`, and the pre-built-`TableFunction` registrar needs a private
//! field), so the table function is created and registered entirely through
//! raw `libduckdb-sys` (`duckdb_create_table_function` +
//! `duckdb_register_table_function`) on the raw `duckdb_connection` the entry
//! point hands us. All of that is contained here.

use std::error::Error;
use std::ffi::{CStr, CString, c_void};
use std::sync::atomic::{AtomicUsize, Ordering};

use libduckdb_sys as ffi;

/// One output cell. The declared column type and the emitted variant agree by
/// construction (the producer builds both). `Null` writes the validity mask
/// regardless of the column's physical type.
// `Double`/`Bool` land with the functions that need them (read_ags, headings);
// the harness carries the full vocabulary from Phase 0.
#[allow(dead_code)]
pub enum Cell {
    Null,
    Str(String),
    Int(i64),
    Double(f64),
    Bool(bool),
}

/// A physical output-column type. Maps to the DuckDB logical type declared for
/// the column at bind; the func writer picks the physical store from the `Cell`.
#[derive(Clone, Copy)]
#[allow(dead_code)] // `Double`/`Boolean` land with later functions (see `Cell`)
pub enum ColType {
    Varchar,
    BigInt,
    Double,
    Boolean,
}

impl ColType {
    /// A freshly-created `duckdb_logical_type` for this column. DuckDB copies it
    /// into the function/result definition, so the caller destroys the returned
    /// handle after adding it.
    unsafe fn logical_type(self) -> ffi::duckdb_logical_type {
        let id = match self {
            ColType::Varchar => ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR,
            ColType::BigInt => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT,
            ColType::Double => ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE,
            ColType::Boolean => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BOOLEAN,
        };
        unsafe { ffi::duckdb_create_logical_type(id as _) }
    }
}

/// The producer closure a function supplies: given the live bind info (wrapped
/// in [`Bind`]), it reads params, does any VFS reads, and returns the output
/// column schema + every row. Stored as the table function's extra-info, so it
/// is called on each bind and must be `Fn + Send + Sync + 'static`.
type Columns = Vec<(&'static str, ColType)>;
type Rows = Vec<Vec<Cell>>;
type Producer = Box<dyn Fn(&Bind) -> Result<(Columns, Rows), String> + Send + Sync + 'static>;

/// A borrowed view over the raw `duckdb_bind_info` for the current bind call.
/// Producers read positional/named params through it, and reach the client
/// context (the VFS) via [`Bind::raw_info`].
pub struct Bind {
    info: ffi::duckdb_bind_info,
}

impl Bind {
    /// The raw bind info — the seam `source::Vfs::from_bind` needs to reach the
    /// query's client context. Only valid for the duration of the bind call.
    pub fn raw_info(&self) -> ffi::duckdb_bind_info {
        self.info
    }

    /// Read a positional `VARCHAR` parameter as a `String`.
    pub fn param_str(&self, index: usize) -> Result<String, String> {
        unsafe {
            let mut val = ffi::duckdb_bind_get_parameter(self.info, index as u64);
            if val.is_null() {
                return Err(format!("missing positional parameter {index}"));
            }
            let s = value_to_string(val);
            ffi::duckdb_destroy_value(&mut val);
            s.ok_or_else(|| format!("positional parameter {index} is not a string"))
        }
    }

    /// Read an optional named `VARCHAR` parameter (`name := value`). Absent or
    /// blank → `None`. Unused by `ags_groups` (no named params) but part of the
    /// harness for the functions that take `edition` / `encoding`.
    #[allow(dead_code)]
    pub fn named_str(&self, name: &str) -> Option<String> {
        let cname = CString::new(name).ok()?;
        unsafe {
            let mut val = ffi::duckdb_bind_get_named_parameter(self.info, cname.as_ptr());
            if val.is_null() {
                return None;
            }
            let s = value_to_string(val);
            ffi::duckdb_destroy_value(&mut val);
            s.filter(|v| !v.trim().is_empty())
        }
    }
}

/// Convert a `duckdb_value` to an owned `String` (or `None` if not string-like).
/// `duckdb_get_varchar` returns a freshly-allocated C string the caller frees.
unsafe fn value_to_string(val: ffi::duckdb_value) -> Option<String> {
    unsafe {
        let raw = ffi::duckdb_get_varchar(val);
        if raw.is_null() {
            return None;
        }
        let s = CStr::from_ptr(raw).to_string_lossy().into_owned();
        ffi::duckdb_free(raw as *mut c_void);
        Some(s)
    }
}

/// The precomputed result, stored as bind data and streamed in func.
struct BindData {
    col_types: Vec<ColType>,
    rows: Rows,
}

/// Per-scan streaming cursor, stored as init data. DuckDB calls func serially
/// for a non-parallel table function, so a plain running offset suffices; the
/// atomic keeps it `Sync` without a lock.
struct InitData {
    cursor: AtomicUsize,
}

/// Free a `Box<T>` handed to DuckDB as bind/init/extra-info via `Box::into_raw`.
unsafe extern "C" fn drop_boxed<T>(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(ptr.cast::<T>()) });
    }
}

/// bind: run the producer, declare the columns, set cardinality, stash the rows.
unsafe extern "C" fn bind(info: ffi::duckdb_bind_info) {
    unsafe {
        // The producer was boxed as extra-info at registration.
        let producer = &*(ffi::duckdb_bind_get_extra_info(info) as *const Producer);
        let bind = Bind { info };
        match producer(&bind) {
            Ok((columns, rows)) => {
                for (name, col) in &columns {
                    let cname = match CString::new(*name) {
                        Ok(c) => c,
                        Err(_) => continue, // column names are static + NUL-free
                    };
                    let mut lt = col.logical_type();
                    ffi::duckdb_bind_add_result_column(info, cname.as_ptr(), lt);
                    ffi::duckdb_destroy_logical_type(&mut lt);
                }
                ffi::duckdb_bind_set_cardinality(info, rows.len() as u64, true);
                let data = Box::new(BindData {
                    col_types: columns.into_iter().map(|(_, c)| c).collect(),
                    rows,
                });
                ffi::duckdb_bind_set_bind_data(
                    info,
                    Box::into_raw(data) as *mut c_void,
                    Some(drop_boxed::<BindData>),
                );
            }
            Err(msg) => {
                let cmsg =
                    CString::new(msg).unwrap_or_else(|_| CString::new("bind error").unwrap());
                ffi::duckdb_bind_set_error(info, cmsg.as_ptr());
            }
        }
    }
}

/// init: allocate the streaming cursor.
unsafe extern "C" fn init(info: ffi::duckdb_init_info) {
    unsafe {
        let data = Box::new(InitData {
            cursor: AtomicUsize::new(0),
        });
        ffi::duckdb_init_set_init_data(
            info,
            Box::into_raw(data) as *mut c_void,
            Some(drop_boxed::<InitData>),
        );
    }
}

/// func: emit up to `duckdb_vector_size()` rows per call; size 0 ends the scan.
unsafe extern "C" fn func(info: ffi::duckdb_function_info, output: ffi::duckdb_data_chunk) {
    unsafe {
        let bind_data = &*(ffi::duckdb_function_get_bind_data(info) as *const BindData);
        let init_data = &*(ffi::duckdb_function_get_init_data(info) as *const InitData);

        let cap = ffi::duckdb_vector_size() as usize;
        let start = init_data.cursor.load(Ordering::Acquire);
        let n = bind_data.rows.len().saturating_sub(start).min(cap);
        if n == 0 {
            ffi::duckdb_data_chunk_set_size(output, 0);
            return;
        }
        for col in 0..bind_data.col_types.len() {
            let vector = ffi::duckdb_data_chunk_get_vector(output, col as u64);
            for i in 0..n {
                write_cell(vector, i, &bind_data.rows[start + i][col]);
            }
        }
        ffi::duckdb_data_chunk_set_size(output, n as u64);
        init_data.cursor.store(start + n, Ordering::Release);
    }
}

/// Write one cell into `vector` at `row`. The physical store follows the `Cell`
/// variant (which agrees with the column's declared type by construction);
/// `Null` writes the validity mask, which is type-agnostic.
unsafe fn write_cell(vector: ffi::duckdb_vector, row: usize, cell: &Cell) {
    unsafe {
        match cell {
            Cell::Null => set_null(vector, row),
            Cell::Str(s) => match CString::new(s.as_str()) {
                Ok(c) => ffi::duckdb_vector_assign_string_element(vector, row as u64, c.as_ptr()),
                // An interior NUL can't be a C string element; emit SQL NULL
                // rather than truncate silently (AGS text has no NULs anyway).
                Err(_) => set_null(vector, row),
            },
            Cell::Int(v) => {
                let data = ffi::duckdb_vector_get_data(vector) as *mut i64;
                *data.add(row) = *v;
            }
            Cell::Double(v) => {
                let data = ffi::duckdb_vector_get_data(vector) as *mut f64;
                *data.add(row) = *v;
            }
            Cell::Bool(v) => {
                // DuckDB BOOLEAN is one byte, laid out as a Rust `bool`.
                let data = ffi::duckdb_vector_get_data(vector) as *mut bool;
                *data.add(row) = *v;
            }
        }
    }
}

/// Mark `row` NULL via the vector's validity mask.
unsafe fn set_null(vector: ffi::duckdb_vector, row: usize) {
    unsafe {
        ffi::duckdb_vector_ensure_validity_writable(vector);
        let validity = ffi::duckdb_vector_get_validity(vector);
        ffi::duckdb_validity_set_row_invalid(validity, row as u64);
    }
}

/// Register a table function taking `n_positional` positional `VARCHAR` args
/// plus any optional `named` `VARCHAR` params (`name := value`), whose rows are
/// produced once at bind by `producer`.
///
/// Built and registered entirely through raw `libduckdb-sys` on the raw
/// connection (see the module note on why duckdb-rs's safe path can't be used).
pub fn register_table<F>(
    con: ffi::duckdb_connection,
    name: &str,
    n_positional: usize,
    named: &[&str],
    producer: F,
) -> Result<(), Box<dyn Error>>
where
    F: Fn(&Bind) -> Result<(Columns, Rows), String> + Send + Sync + 'static,
{
    unsafe {
        let tf = ffi::duckdb_create_table_function();
        if tf.is_null() {
            return Err("duckdb_create_table_function returned null".into());
        }
        let cname = CString::new(name)?;
        ffi::duckdb_table_function_set_name(tf, cname.as_ptr());

        // Positional VARCHAR params. DuckDB copies each logical type, so we
        // destroy our local handle right after adding it.
        for _ in 0..n_positional {
            let mut lt = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR as _);
            ffi::duckdb_table_function_add_parameter(tf, lt);
            ffi::duckdb_destroy_logical_type(&mut lt);
        }
        // Optional named VARCHAR params (the only way DuckDB does optional args).
        for pname in named {
            let cpname = CString::new(*pname)?;
            let mut lt = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR as _);
            ffi::duckdb_table_function_add_named_parameter(tf, cpname.as_ptr(), lt);
            ffi::duckdb_destroy_logical_type(&mut lt);
        }

        ffi::duckdb_table_function_set_bind(tf, Some(bind));
        ffi::duckdb_table_function_set_init(tf, Some(init));
        ffi::duckdb_table_function_set_function(tf, Some(func));

        // The producer, boxed as extra-info. Register transfers ownership of the
        // extra-info (with its destroy callback) into the catalog, so destroying
        // our local table-function handle afterwards does not free it.
        let producer: Producer = Box::new(producer);
        let extra = Box::into_raw(Box::new(producer)) as *mut c_void;
        ffi::duckdb_table_function_set_extra_info(tf, extra, Some(drop_boxed::<Producer>));

        let state = ffi::duckdb_register_table_function(con, tf);
        let mut tf = tf;
        ffi::duckdb_destroy_table_function(&mut tf);
        if state != ffi::DuckDBSuccess {
            return Err(format!("failed to register table function '{name}'").into());
        }
        Ok(())
    }
}
