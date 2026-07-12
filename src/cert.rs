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
pub fn sliced_group(vfs: &Vfs, path: &str, group: &str) -> Option<AgsGroup> {
    let sidecar = load_sidecar(vfs, path)?;
    let (handle, size) = vfs.open_for_read(path).ok()?;
    // read = READ: a size match is the (cheap) freshness gate — no re-hash.
    if !sidecar.size_matches(size) {
        return None;
    }
    let (start, end) = sidecar.groups.get(group).copied()?;
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
}
