//! The `.ags.idx` **certificate** consume seam — the sliced-read fast-path a
//! certificate enables. Minting lives *outside* this read-only extension
//! (`lat certify` / the `laterite` library); this module only reads a cert.
//!
//! A `.ags.idx` is a sibling sidecar (`<file>.idx`) carrying a byte-offset index
//! over each group's section **plus** a validity certificate — who validated the
//! file, against which engine + edition, and that it came back clean. It exists
//! only for a file that validated clean, so an index that *exists* is a positive
//! assertion, never a reference into a possibly-corrupt file. Core
//! ([`laterite_ags4_core::index`]) owns the format; this module consumes it.
//!
//! [`sliced_group`] gates on **size** only: re-hashing a remote object to read
//! one group would mean downloading it, defeating the ranged-read win. A
//! same-size in-place edit is the documented blind spot (identical to the
//! `(path, size)` parse cache), and `parse_group_slice` re-runs the real parser
//! so a *shifted* file errors out (→ whole-file fallback) rather than returns
//! garbage.
//!
//! This extension can read remote objects (the VFS) but cannot read their HTTP
//! headers (the VFS exposes `seek`/`read`/`size`, no ETag), so its freshness is
//! size-based; the format's `etag`/`last_modified` fields serve a future
//! header-capable consumer.

use laterite_ags4_core::ags4_codec::AgsGroup;
use laterite_ags4_core::index::{Sidecar, parse_group_slice};
use laterite_ags4_reference::dict::DictVersion;

use super::source::Vfs;

/// The sibling certificate path for a source `path` (`site.ags` → `site.ags.idx`).
pub fn idx_path_for(path: &str) -> String {
    format!("{path}.idx")
}

/// Map a user edition string to a bundled `DictVersion`, or a clear error listing
/// the supported set. Consumed by `ags_dictionary(edition := …)` to pick an
/// edition's bundled standard dictionary. (The validator deliberately exposes no
/// `FromStr` — the bundled set is small and fixed.)
pub fn parse_edition(s: &str) -> Result<DictVersion, String> {
    match s.trim() {
        "4.0.3" => Ok(DictVersion::V4_0_3),
        "4.0.4" => Ok(DictVersion::V4_0_4),
        "4.1" => Ok(DictVersion::V4_1),
        "4.1.1" => Ok(DictVersion::V4_1_1),
        "4.2" => Ok(DictVersion::V4_2),
        other => Err(format!(
            "unknown edition '{other}'; expected one of 4.0.3, 4.0.4, 4.1, 4.1.1, 4.2"
        )),
    }
}

/// Load + parse the certificate for `path`, or `None` if there isn't a usable one
/// (absent, unreadable, or a corrupt/unknown-version `.idx`). Every "no usable
/// cert" reason collapses to `None` so the caller cleanly falls back to the
/// whole-file read path — a broken sidecar must never be a hard error.
fn load_sidecar(vfs: &Vfs, path: &str) -> Option<Sidecar> {
    let json = super::source::read_bytes(vfs, &idx_path_for(path)).ok()?;
    Sidecar::from_json(&json).ok()
}

/// **Read fast-path.** If a *size-fresh* cert indexes `group`, range-read just that
/// group's bytes (one `seek` + `read`, local or remote) and parse the slice —
/// O(group) instead of O(file). Returns `None` (→ whole-file parse) on any miss:
/// no cert, a size change, a group the cert doesn't index (passthrough/absent), or
/// a slice that won't parse (a same-size *shifted* file — the whole-file path then
/// parses the changed bytes correctly).
/// The one span we may slice for `group`, or `None` if a sliced read cannot be trusted.
///
/// A GROUP may legally appear more than once in an AGS4 file, and the certificate records
/// **every** span it occupies. Only a group that appears exactly ONCE can be answered by a
/// range-read: for a redeclared group the section is not contiguous, and reading the first
/// span returns some of the rows while looking exactly like a complete, correct read.
///
/// That is what this extension did before the `.ags.idx` v2 format — not because it chose
/// the first span, but because v1's index had nowhere to record the others: `groups` mapped
/// a code to ONE `Range`, first-seen-wins, so the truncation was already baked into the
/// certificate and no consumer could see past it. v2 maps a code to `Vec<Range>`, which is
/// what makes this check possible at all.
///
/// `None` here is not a failure — it is the caller taking the whole-file parse, which sees
/// every span. The fast path is an optimisation, and an optimisation that returns fewer
/// rows than the slow path is a wrong answer, not a fast one.
fn sliceable_span(sidecar: &Sidecar, group: &str) -> Option<(u64, u64)> {
    match sidecar.groups.get(group).map(Vec::as_slice) {
        // Exactly one span: the section is contiguous, so a range-read is complete.
        Some([only]) => Some(*only),
        // Absent (a passthrough/unknown group), or REDECLARED. Both fall back.
        _ => None,
    }
}

pub fn sliced_group(vfs: &Vfs, path: &str, group: &str) -> Option<AgsGroup> {
    let sidecar = load_sidecar(vfs, path)?;
    let (handle, size) = vfs.open_for_read(path).ok()?;
    // read = READ: a size match is the (cheap) freshness gate — no re-hash.
    if !sidecar.size_matches(size) {
        return None;
    }
    let (start, end) = sliceable_span(&sidecar, group)?;
    if end < start || end > size {
        return None; // a cert inconsistent with the live size → fall back
    }
    handle.seek(start).ok()?;
    let len = (end - start) as usize;
    let mut buf = vec![0u8; len];
    let mut got = 0usize;
    while got < len {
        let n = handle.read(&mut buf[got..]).ok()?;
        if n == 0 {
            return None; // short read → whole-file fallback re-reports any IO error
        }
        got += n;
    }
    // The slice begins at the group's `"GROUP",…` record, so within `buf` its own
    // range is the whole buffer.
    parse_group_slice(&buf, (0, buf.len() as u64), group).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idx_path_is_a_sibling() {
        assert_eq!(idx_path_for("site.ags"), "site.ags.idx");
        assert_eq!(idx_path_for("s3://b/d.ags"), "s3://b/d.ags.idx");
    }

    #[test]
    fn parse_edition_accepts_only_bundled() {
        assert!(parse_edition("4.2").is_ok());
        assert!(parse_edition(" 4.1.1 ").is_ok()); // trimmed
        assert!(parse_edition("4.9").is_err());
    }

    /// An AGS4 file whose LOCA section is REDECLARED — two `"GROUP","LOCA"` records, one
    /// row each. A whole-file parse returns both rows. A sliced read of the first span
    /// returns one, and looks exactly like a complete answer.
    const REDECLARED: &str = concat!(
        "\"GROUP\",\"LOCA\"\r\n",
        "\"HEADING\",\"LOCA_ID\"\r\n",
        "\"UNIT\",\"\"\r\n\"TYPE\",\"ID\"\r\n",
        "\"DATA\",\"BH01\"\r\n\r\n",
        "\"GROUP\",\"PROJ\"\r\n",
        "\"HEADING\",\"PROJ_ID\"\r\n",
        "\"UNIT\",\"\"\r\n\"TYPE\",\"ID\"\r\n",
        "\"DATA\",\"P1\"\r\n\r\n",
        "\"GROUP\",\"LOCA\"\r\n",
        "\"HEADING\",\"LOCA_ID\"\r\n",
        "\"UNIT\",\"\"\r\n\"TYPE\",\"ID\"\r\n",
        "\"DATA\",\"BH02\"\r\n",
    );

    fn stamp() -> laterite_ags4_core::index::ValidationStamp {
        laterite_ags4_core::index::ValidationStamp {
            validator: "test".into(),
            engine: "0000000000000000".into(),
            compat: None,
            checked_at: "2026-07-14T00:00:00Z".into(),
            edition: laterite_ags4_core::index::EditionInput::Auto {
                resolved: "4.1.1".into(),
                resolution: laterite_ags4_reference::dict::DictResolution::Fallback,
            },
            encoding: "UTF-8".into(),
            errors: laterite_ags4_core::index::TierCoverage::Measured { count: 0 },
            warnings: laterite_ags4_core::index::TierCoverage::Measured { count: 0 },
            fyi: laterite_ags4_core::index::TierCoverage::Measured { count: 0 },
        }
    }

    /// **The bug this fixes.** A redeclared group has no single contiguous section, so it
    /// cannot be answered by a range-read — and the honest reply is to decline the fast
    /// path, not to return the first span. Returning the first span is a *wrong* answer
    /// that is indistinguishable from a right one: the rows are well-formed, the query
    /// succeeds, and the second section is simply missing.
    #[test]
    fn a_redeclared_group_refuses_the_sliced_read() {
        let sidecar = Sidecar::assemble(REDECLARED.as_bytes(), stamp()).expect("assembles");
        assert_eq!(
            sidecar.groups.get("LOCA").map(Vec::len),
            Some(2),
            "the v2 cert records BOTH spans — v1 could not, which is why this bug was invisible"
        );
        assert_eq!(
            sliceable_span(&sidecar, "LOCA"),
            None,
            "a redeclared group must fall back to the whole-file parse, which sees both rows"
        );
    }

    /// The fast path is not disabled — a group that appears once is still sliced.
    #[test]
    fn a_single_span_group_is_still_sliced() {
        let sidecar = Sidecar::assemble(REDECLARED.as_bytes(), stamp()).expect("assembles");
        let (start, end) = sliceable_span(&sidecar, "PROJ").expect("PROJ appears exactly once");
        assert!(start < end);
        let slice = &REDECLARED.as_bytes()[start as usize..end as usize];
        assert!(
            slice.starts_with(b"\"GROUP\",\"PROJ\""),
            "the span begins at PROJ's own GROUP record"
        );
    }

    /// A group the cert does not index at all (passthrough / absent) also declines.
    #[test]
    fn an_absent_group_refuses_the_sliced_read() {
        let sidecar = Sidecar::assemble(REDECLARED.as_bytes(), stamp()).expect("assembles");
        assert_eq!(sliceable_span(&sidecar, "SAMP"), None);
    }
}
