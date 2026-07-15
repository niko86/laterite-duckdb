//! `read_ags(path, group)` + `read_ags_text(content, group)` — one AGS group as
//! a typed, UUID-keyed table on the [`crate::ffi_table`] harness.
//!
//! The producer runs once at bind: it reads (or slices) + parses the file,
//! resolves the group's registry descriptor, and materialises the typed schema
//! — `_id`, `_parent_id`, then one column per heading typed from the file's own
//! TYPE row (AGS4 is self-describing) — with each row's deterministic
//! `keychain` ids. The harness then streams those rows a vector-chunk at a time.
//!
//! - `read_ags(path, group [, encoding := …])` reads through DuckDB's virtual
//!   filesystem (see [`super::source`]), so `path` may be local, `http(s)://`,
//!   or `s3://` (with `LOAD httpfs`). The optional `encoding` named param
//!   decodes non-UTF-8 source bytes before the UTF-8-only core codec.
//! - `read_ags_text(content, group)` takes the AGS4 text inline as a VARCHAR
//!   (already-decoded UTF-8) — no VFS, no `encoding` param.
//!
//! A group outside the AGS dictionary (passthrough/custom) returns a clear bind
//! error for now.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use laterite_ags4_core::ags4_codec::{AgsGroup, ParsedAgs4, read_ags4_bytes};
use laterite_ags4_core::keychain;
use laterite_ags4_core::registry::registry;
use libduckdb_sys as ffi;

use super::ffi_table::{Bind, Cell, ColType, register_table};
use super::source::{Vfs, read_parsed_with_encoding};
use super::typing::{Emit, cell_for};

/// The harness declares column names as `&'static str`, but AGS heading names
/// are dynamic (read from the file's HEADING row). Distinct heading names are
/// bounded (the dictionary plus any custom columns a file carries), so
/// interning — leaking each distinct name exactly once and reusing it on every
/// later bind — satisfies that requirement without re-leaking per query.
fn intern(name: &str) -> &'static str {
    static POOL: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let pool = POOL.get_or_init(|| Mutex::new(HashSet::new()));
    let mut set = pool.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(interned) = set.get(name) {
        return interned;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    set.insert(leaked);
    leaked
}

/// Register `read_ags(path, group [, encoding := …])` — the VFS path reader
/// (local / `http(s)://` / `s3://`).
pub fn register(con: ffi::duckdb_connection) -> Result<(), Box<dyn std::error::Error>> {
    register_table(con, "read_ags", 2, &["encoding"], |bind: &Bind| {
        let path = bind.param_str(0)?;
        let group = bind.param_str(1)?.trim().to_uppercase();
        // Optional `encoding` named param: default UTF-8; a WHATWG label decodes
        // non-UTF-8 source bytes before the UTF-8-only core codec.
        let encoding = super::source::resolve_encoding(bind.named_str("encoding").as_deref())?;

        // SAFETY: the producer runs during bind, so the raw bind info is live and
        // its client context (the VFS) is valid for this call.
        let vfs = unsafe { Vfs::from_bind(bind.raw_info()) }?;

        // Certificate fast-path: a size-fresh `<path>.ags.idx` that indexes this
        // group lets us range-read just that group's bytes (parsed as UTF-8), so
        // it serves the default encoding only; a non-UTF-8 read takes the
        // whole-file decode path (the cert is a same-file optimisation, not a
        // correctness requirement).
        if encoding == encoding_rs::UTF_8 {
            if let Some(ags) = super::cert::sliced_group(&vfs, &path, &group) {
                return build_table(&ags, &group);
            }
        }
        let parsed = read_parsed_with_encoding(&vfs, &path, encoding)?;
        let ags = resolve_group(&parsed, &group)?;
        build_table(ags, &group)
    })
}

/// Register `read_ags_text(content, group)` — the inline-text variant (no VFS,
/// no encoding: `content` is already a UTF-8 String).
pub fn register_text(con: ffi::duckdb_connection) -> Result<(), Box<dyn std::error::Error>> {
    register_table(con, "read_ags_text", 2, &[], |bind: &Bind| {
        let content = bind.param_str(0)?;
        let group = bind.param_str(1)?.trim().to_uppercase();
        let parsed = read_ags4_bytes(content.as_bytes())
            .map_err(|e| format!("read_ags_text: input did not parse as AGS4 ({e})"))?;
        let ags = resolve_group(&parsed, &group)?;
        build_table(ags, &group)
    })
}

/// Resolve one group out of a parsed file, with a helpful error listing what's
/// present when it's absent.
fn resolve_group<'a>(parsed: &'a ParsedAgs4, group: &str) -> Result<&'a AgsGroup, String> {
    parsed.get(group).ok_or_else(|| {
        format!(
            "group '{group}' not found (groups present: {})",
            parsed.order.join(", ")
        )
    })
}

/// Build the typed, keyed `(columns, rows)` for one resolved group: `_id`,
/// `_parent_id`, then one column per heading typed from the file's own TYPE row,
/// with each row's deterministic `keychain` ids precomputed (one SHA-256 each).
#[allow(clippy::type_complexity)]
fn build_table(
    ags: &AgsGroup,
    group: &str,
) -> Result<(Vec<(&'static str, ColType)>, Vec<Vec<Cell>>), String> {
    let reg = registry();
    let descriptor = reg.get(group).cloned().ok_or_else(|| {
        format!(
            "group '{group}' is not in the AGS dictionary; passthrough (custom-group) support is pending"
        )
    })?;

    // Schema: the deterministic identity keys first, then one column per heading
    // typed from the file's own TYPE row, then a trailing `_content_hash`.
    let mut columns: Vec<(&'static str, ColType)> = Vec::with_capacity(ags.headings.len() + 3);
    columns.push(("_id", ColType::Varchar));
    columns.push(("_parent_id", ColType::Varchar));

    // Per-heading (name, ags_type, emit-kind), aligned with the TYPE row. A
    // heading past the end of the TYPE row (a short TYPE line) defaults to `X`
    // (free text → VARCHAR), matching the whole-file reader.
    let mut plan: Vec<(String, String, Emit)> = Vec::with_capacity(ags.headings.len());
    for (i, heading) in ags.headings.iter().enumerate() {
        let ags_type = ags.types.get(i).cloned().unwrap_or_else(|| "X".to_string());
        let kind = Emit::of(&ags_type);
        columns.push((intern(heading), kind.col_type()));
        plan.push((heading.clone(), ags_type, kind));
    }

    // `_content_hash` (trailing): the typed, blank- and unit-aware VALUE
    // fingerprint of the whole row — the value twin of `_id`'s IDENTITY, minted
    // from the SAME `keychain` leaf as the wheel / Node / browser, so a row
    // hashes byte-identically across every surface. Trailing keeps heading
    // positions stable; it enables `SELECT DISTINCT ON (_content_hash)`
    // value-dedup (a power user EXCLUDEs it, or the id columns, at will).
    columns.push(("_content_hash", ColType::Varchar));

    let rows: Vec<Vec<Cell>> = ags
        .rows
        .iter()
        .map(|row| {
            let (id, parent) = keychain::row_ids(reg, &descriptor, row);
            let mut cells: Vec<Cell> = Vec::with_capacity(plan.len() + 3);
            cells.push(Cell::Str(id.to_string()));
            cells.push(parent.map_or(Cell::Null, |u| Cell::Str(u.to_string())));
            for (heading, ags_type, kind) in &plan {
                let raw = row.get(heading).map(String::as_str);
                cells.push(cell_for(raw, ags_type, *kind));
            }
            // Trailing `_content_hash` — see the column note above. Built from the
            // file's own UNIT + TYPE rows (per-file canonicalisation), exactly the
            // (heading, unit, type, value) tuples `keychain::group_content_hashes`
            // feeds on the other surfaces, so the digest is byte-identical.
            let hash_cells: Vec<(&str, &str, &str, &str)> = ags
                .headings
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    (
                        h.as_str(),
                        ags.units.get(i).map(String::as_str).unwrap_or(""),
                        ags.types.get(i).map(String::as_str).unwrap_or(""),
                        row.get(h).map(String::as_str).unwrap_or(""),
                    )
                })
                .collect();
            cells.push(Cell::Str(
                keychain::content_hash(group, &hash_cells).to_string(),
            ));
            cells
        })
        .collect();

    Ok((columns, rows))
}
