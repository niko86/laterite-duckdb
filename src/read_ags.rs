//! `read_ags(path, group)` + `read_ags_text(content, group)` — one AGS group as
//! a typed, UUID-keyed table on the [`crate::ffi_table`] harness.
//!
//! The producer runs once at bind: it reads (or slices) + parses the file,
//! admits the group under the Rule 18 effective dictionary (standard ∪ the
//! file's own `DICT` group), and materialises the typed schema — `_id`,
//! `_parent_id`, then one column per heading typed from the file's own TYPE
//! row (AGS4 is self-describing) — with each row's deterministic `keychain`
//! ids. The harness then streams those rows a vector-chunk at a time.
//!
//! - `read_ags(path, group [, encoding := …])` reads through DuckDB's virtual
//!   filesystem (see [`super::source`]), so `path` may be local, `http(s)://`,
//!   or `s3://` (with `LOAD httpfs`). The optional `encoding` named param
//!   decodes non-UTF-8 source bytes before the UTF-8-only core codec.
//! - `read_ags_text(content, group)` takes the AGS4 text inline as a VARCHAR
//!   (already-decoded UTF-8) — no VFS, no `encoding` param.
//!
//! A group only the file's own `DICT` declares binds like any other — columns
//! typed from its TYPE row — but **unkeyed**: the engine mints content-addressed
//! ids from spec KEY headings only, so `_id`/`_parent_id` are NULL (the same
//! unkeyed batch the wheel builds for that group). A group declared by neither
//! side of the effective dictionary is refused at bind.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use laterite_ags4_core::ags4_codec::{AgsGroup, ParsedAgs4, read_ags4_bytes};
use laterite_ags4_core::effective_dict::file_dict_of;
use laterite_ags4_core::keychain;
use laterite_ags4_core::registry::registry;
use libduckdb_sys as ffi;

use super::ffi_table::{Bind, Cell, ColType, register_table};
use super::source::{Vfs, read_parsed_with_encoding};
use super::typing::{Emit, cell_for};

/// How an admitted group binds. Decided by whichever side of the Rule 18
/// effective dictionary admitted it — the caller ([`binding_for`] on the
/// whole-file paths, [`super::cert::sliced_group`] on the cert fast-path —
/// both answer the file half through core's `effective_dict`, so they agree
/// by construction).
pub enum Binding {
    /// A standard-dictionary group: `_id`/`_parent_id` minted from its spec
    /// KEY headings via the shared `keychain`.
    Keyed,
    /// A group only the file's own `DICT` declares: no spec keys exist, so
    /// `_id`/`_parent_id` are NULL — the engine's documented unkeyed batch.
    Unkeyed,
}

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
            if let Some((ags, binding)) = super::cert::sliced_group(&vfs, &path, &group) {
                return build_table(&ags, &group, &binding);
            }
        }
        let parsed = read_parsed_with_encoding(&vfs, &path, encoding)?;
        let ags = resolve_group(&parsed, &group)?;
        let binding = binding_for(&parsed, &group)?;
        build_table(ags, &group, &binding)
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
        let binding = binding_for(&parsed, &group)?;
        build_table(ags, &group, &binding)
    })
}

/// Admit `group` under the Rule 18 effective dictionary and say how it binds.
/// The file half is read through core's `effective_dict` — the same module the
/// certificate's `defines` field is measured with, so the whole-file and cert
/// paths agree on what "declared" means by construction. A group declared by
/// neither side is refused: binding it anyway would silently paper over
/// exactly what Rule 18 exists to catch.
fn binding_for(parsed: &ParsedAgs4, group: &str) -> Result<Binding, String> {
    if registry().get(group).is_some() {
        return Ok(Binding::Keyed);
    }
    if file_dict_of(parsed).groups().contains(group) {
        return Ok(Binding::Unkeyed);
    }
    Err(format!(
        "group '{group}' is not in the AGS dictionary and the file's own DICT group does not \
         declare it (Rule 18); a custom group is readable once the file declares it"
    ))
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

/// Build the typed `(columns, rows)` for one admitted group: `_id`,
/// `_parent_id`, then one column per heading typed from the file's own TYPE row.
/// A [`Binding::Keyed`] group gets each row's deterministic `keychain` ids
/// precomputed (one SHA-256 each); a [`Binding::Unkeyed`] one keeps the id
/// columns — the schema is the same for every group — as NULLs.
#[allow(clippy::type_complexity)]
fn build_table(
    ags: &AgsGroup,
    group: &str,
    binding: &Binding,
) -> Result<(Vec<(&'static str, ColType)>, Vec<Vec<Cell>>), String> {
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

    // `_id`/`_parent_id` for every row up front, via the positional batch keychain
    // (KEY columns resolved once per group) — the same path laterite's own reader
    // uses. `ags.rows` is now keyed by shared `Arc<str>` heading names, so a column
    // `c` is read by resolving it to its heading name. An unkeyed (file-declared)
    // group skips the minting: `group_row_ids` has no spec KEY headings to
    // resolve for it, and its documented answer — an empty vec — must become
    // per-row NULLs here, never zero rows.
    let ids: Option<Vec<(String, Option<String>)>> = match binding {
        Binding::Keyed => Some(keychain::group_row_ids(
            registry(),
            group,
            &ags.headings,
            ags.rows.len(),
            |c, r| {
                ags.rows[r]
                    .get(ags.headings[c].as_str())
                    .map(String::as_str)
            },
        )),
        Binding::Unkeyed => None,
    };

    let rows: Vec<Vec<Cell>> = ags
        .rows
        .iter()
        .enumerate()
        .map(|(r, row)| {
            let mut cells: Vec<Cell> = Vec::with_capacity(plan.len() + 3);
            match ids.as_ref().map(|v| &v[r]) {
                Some((id, parent)) => {
                    cells.push(Cell::Str(id.clone()));
                    cells.push(parent.as_ref().map_or(Cell::Null, |s| Cell::Str(s.clone())));
                }
                None => {
                    cells.push(Cell::Null);
                    cells.push(Cell::Null);
                }
            }
            for (heading, ags_type, kind) in &plan {
                let raw = row.get(heading.as_str()).map(String::as_str);
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
                        row.get(h.as_str()).map(String::as_str).unwrap_or(""),
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
