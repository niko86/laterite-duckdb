//! The `read_ags(path, group)` table function — lazy, typed, UUID-keyed.
//!
//! - **bind** runs once: read the params, slurp + parse the file (raw `.ags`
//!   has no index/footer, so a full read is inherent to the format), resolve
//!   the group's registry descriptor, declare the output schema (`_id`,
//!   `_parent_id`, then one column per heading typed from the file's own TYPE
//!   row — AGS4 is self-describing), and precompute each row's deterministic
//!   `keychain` ids.
//! - **scan** streams the rows back a vector-size (≈2048) chunk at a time.
//!
//! Reads go through DuckDB's virtual filesystem (see [`super::source`]), so
//! `path` may be local, `http(s)://`, or `s3://` (with `LOAD httpfs`). A group
//! outside the AGS dictionary (passthrough/custom) returns a clear bind error
//! for now.
//!
//! `read_ags_text(content, group)` is the content variant: it takes the AGS4
//! file's text as a VARCHAR argument (e.g. AGS already in a column, or from
//! DuckDB's built-in `read_text`) instead of a path — handy when the data isn't a
//! file you can hand the VFS. Table-fn args are constant-folded at bind (where the
//! schema is decided), so the content must be constant there.

use std::collections::HashMap;

use laterite_ags4_core::ags4_codec::{ParsedAgs4, read_ags4_bytes};
use laterite_ags4_core::keychain;
use laterite_ags4_core::registry::registry;
use laterite_types::parse_value;
use quack_rs::prelude::*;
use quack_rs::vector::vector_size;

use super::typing::{Emit, write_value};

/// One output data column (after the `_id` / `_parent_id` pair).
struct Column {
    /// AGS heading name (also the row-map key).
    heading: String,
    /// AGS type code from the file's TYPE row (self-describing).
    ags_type: String,
    kind: Emit,
}

/// Per-query scan state: the parsed rows, their precomputed ids, the output
/// column plan, and a streaming cursor. `Send + 'static` (Strings + Vecs).
pub struct ReadAgsState {
    columns: Vec<Column>,
    rows: Vec<HashMap<String, String>>,
    /// `(id, parent_id)` per row as UUID strings; `parent_id` is `None` for a
    /// root group (PROJ).
    ids: Vec<(String, Option<String>)>,
    cursor: usize,
}

/// Build `read_ags(path, group)` — the VFS path reader (local / http(s):// / s3://).
pub fn register(con: &Connection) -> ExtResult<()> {
    let builder = TableFunctionBuilder::new("read_ags")
        .param(TypeId::Varchar) // path
        .param(TypeId::Varchar) // group
        .with_state::<ReadAgsState, _>(bind)
        .scan(scan)
        .build()?;
    // SAFETY: a freshly-built table function; DuckDB takes ownership of the
    // closures (via extra_info) for the registered function's lifetime.
    unsafe { con.register_table(builder) }
}

/// Build `read_ags_text(content, group)` — STABLE-only variant (no VFS).
pub fn register_text(con: &Connection) -> ExtResult<()> {
    let builder = TableFunctionBuilder::new("read_ags_text")
        .param(TypeId::Varchar) // AGS4 file content
        .param(TypeId::Varchar) // group
        .with_state::<ReadAgsState, _>(bind_text)
        .scan(scan)
        .build()?;
    unsafe { con.register_table(builder) }
}

/// bind (path): read params, slurp + parse the file via the VFS, then plan.
fn bind(info: &BindInfo) -> Result<ReadAgsState, ExtensionError> {
    // SAFETY: both are declared `Varchar` positional params, so DuckDB
    // guarantees they're present and string-valued during bind.
    let path = unsafe { info.get_parameter_value(0) }.as_str()?;
    let group = unsafe { info.get_parameter_value(1) }
        .as_str()?
        .trim()
        .to_uppercase();

    // SAFETY: `info` is a live bind-info; `get_client_context` yields the
    // query's client context, from which `source` obtains the VFS.
    let ctx = unsafe { info.get_client_context() };
    let parsed = super::source::read_parsed(&ctx, &path)?;
    plan(info, &parsed, &group)
}

/// bind (text): the AGS4 content arrives as a VARCHAR — parse it directly, no
/// VFS needed. The content must be constant-foldable at bind (where the schema
/// is decided).
fn bind_text(info: &BindInfo) -> Result<ReadAgsState, ExtensionError> {
    let content = unsafe { info.get_parameter_value(0) }.as_str()?;
    let group = unsafe { info.get_parameter_value(1) }
        .as_str()?
        .trim()
        .to_uppercase();
    let parsed = read_ags4_bytes(content.as_bytes()).map_err(|e| {
        ExtensionError::new(format!("read_ags_text: input did not parse as AGS4 ({e})"))
    })?;
    plan(info, &parsed, &group)
}

/// Shared planning: resolve the group, declare the typed output schema, and
/// precompute the deterministic ids. Used by both the path and text binds.
fn plan(info: &BindInfo, parsed: &ParsedAgs4, group: &str) -> Result<ReadAgsState, ExtensionError> {
    let ags = parsed.get(group).ok_or_else(|| {
        ExtensionError::new(format!(
            "group '{group}' not found (groups present: {})",
            parsed.order.join(", ")
        ))
    })?;

    let reg = registry();
    let descriptor = reg.get(group).cloned().ok_or_else(|| {
        ExtensionError::new(format!(
            "group '{group}' is not in the AGS dictionary; passthrough (custom-group) support is pending"
        ))
    })?;

    // Schema: the deterministic keys first, then one column per heading typed
    // from the file's own TYPE row.
    info.add_result_column("_id", TypeId::Varchar);
    info.add_result_column("_parent_id", TypeId::Varchar);
    let mut columns = Vec::with_capacity(ags.headings.len());
    for (i, heading) in ags.headings.iter().enumerate() {
        let ags_type = ags.types.get(i).cloned().unwrap_or_else(|| "X".to_string());
        let kind = Emit::of(&ags_type);
        info.add_result_column(heading, kind.type_id());
        columns.push(Column {
            heading: heading.clone(),
            ags_type,
            kind,
        });
    }

    // Precompute the deterministic ids per row (one SHA-256 each — cheap).
    let ids = ags
        .rows
        .iter()
        .map(|row| {
            let (id, parent) = keychain::row_ids(reg, &descriptor, row);
            (id.to_string(), parent.map(|u| u.to_string()))
        })
        .collect();

    info.set_cardinality(ags.rows.len() as u64, true);
    Ok(ReadAgsState {
        columns,
        rows: ags.rows.clone(),
        ids,
        cursor: 0,
    })
}

/// scan: emit up to one vector-size worth of rows per call; size 0 ends it.
fn scan(state: &mut ReadAgsState, chunk: &DataChunk) -> Result<(), ExtensionError> {
    let cap = vector_size() as usize;
    let start = state.cursor;
    let n = (state.rows.len() - start).min(cap);
    if n == 0 {
        // SAFETY: signalling end-of-stream on the valid output chunk.
        unsafe { chunk.set_size(0) };
        return Ok(());
    }

    // _id (column 0)
    {
        let mut w = unsafe { chunk.writer(0) };
        for i in 0..n {
            unsafe { w.write_varchar(i, &state.ids[start + i].0) };
        }
    }
    // _parent_id (column 1)
    {
        let mut w = unsafe { chunk.writer(1) };
        for i in 0..n {
            match &state.ids[start + i].1 {
                Some(s) => unsafe { w.write_varchar(i, s) },
                None => unsafe { w.set_null(i) },
            }
        }
    }
    // data columns (offset by the two key columns)
    for (c, col) in state.columns.iter().enumerate() {
        let mut w = unsafe { chunk.writer(c + 2) };
        for i in 0..n {
            let raw = state.rows[start + i].get(&col.heading).map(String::as_str);
            let value = parse_value(raw, &col.ags_type);
            // SAFETY: column c+2 was declared with `col.kind.type_id()`; i < n <= cap.
            unsafe { write_value(&mut w, i, &value, col.kind) };
        }
    }

    // SAFETY: n <= capacity and every column was written for rows 0..n.
    unsafe { chunk.set_size(n) };
    state.cursor += n;
    Ok(())
}
