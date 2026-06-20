//! The `.ags.idx` **certificate** seam — minting (`certify_ags`) and the two
//! fast-paths that consume one: a sliced read (`read_ags`) and a skipped
//! re-validation (`validate_ags`).
//!
//! A `.ags.idx` is a sibling sidecar (`<file>.idx`) carrying a byte-offset index
//! over each group's section **plus** a validity certificate — who validated the
//! file, against which engine + edition, and that it came back clean. It exists
//! only for a file that validated clean, so an index that *exists* is a positive
//! assertion, never a reference into a possibly-corrupt file. Core
//! ([`laterite_ags4_core::index`]) owns the format; this module is the extension's
//! validator-aware layer that mints and consumes it.
//!
//! **The freshness split is deliberate** — a *read* trusts the cheap signal, a
//! *verdict* trusts the strong one:
//! - [`sliced_group`] (read) gates on **size** only: re-hashing a remote object to
//!   read one group would mean downloading it, defeating the ranged-read win. A
//!   same-size in-place edit is the documented blind spot (identical to the
//!   `(path, size)` parse cache), and `parse_group_slice` re-runs the real parser
//!   so a *shifted* file errors out (→ whole-file fallback) rather than returns
//!   garbage.
//! - [`clean_verdict_certified`] (verdict) gates on the strong **SHA-256**:
//!   `validate_ags` reads the file regardless, so it can afford to hash, and a
//!   verdict must not ride on a cheap signal.
//!
//! This extension can read remote objects (the VFS) but cannot read their HTTP
//! headers (quack-rs exposes `seek`/`read`/`size`, no ETag), so its freshness is
//! size-based; the format's `etag`/`last_modified` fields serve a future
//! header-capable consumer.

use std::time::{SystemTime, UNIX_EPOCH};

use laterite_ags4_core::ags4_codec::AgsGroup;
use laterite_ags4_core::index::{Sidecar, parse_group_slice};
use laterite_ags4_validator::DictVersion;
use laterite_ags4_validator::findings::{Findings, Severity};
use quack_rs::client_context::ClientContext;
use quack_rs::prelude::ExtensionError;

/// The checker identity this extension stamps into a cert — the SAME string the
/// Python `laterite` wheel stamps, so a cert minted by either surface is trusted
/// by the other (both run the bundled `laterite_ags4_validator` engine; what
/// makes them comparable is the engine VERSION, recorded alongside this).
pub const VALIDATOR_NAME: &str = "laterite_ags4";

/// The sibling certificate path for a source `path` (`site.ags` → `site.ags.idx`).
pub fn idx_path_for(path: &str) -> String {
    format!("{path}.idx")
}

/// Map a user edition string to a bundled `DictVersion`, or a clear error listing
/// the supported set. Shared by `validate_ags` and `certify_ags` so a forced
/// edition means the same thing whether you're checking or certifying. (The
/// validator deliberately exposes no `FromStr` — the bundled set is small and
/// fixed.)
pub fn parse_edition(s: &str) -> Result<DictVersion, ExtensionError> {
    match s.trim() {
        "4.0.3" => Ok(DictVersion::V4_0_3),
        "4.0.4" => Ok(DictVersion::V4_0_4),
        "4.1" => Ok(DictVersion::V4_1),
        "4.1.1" => Ok(DictVersion::V4_1_1),
        "4.2" => Ok(DictVersion::V4_2),
        other => Err(ExtensionError::new(format!(
            "unknown edition '{other}'; expected one of 4.0.3, 4.0.4, 4.1, 4.1.1, 4.2"
        ))),
    }
}

/// Tally findings by severity → `(errors, warnings, fyi)`. A cert is minted only
/// when `errors == 0`; the other two are recorded as the cert's advisory counts.
pub fn severity_counts(findings: &Findings) -> (u64, u64, u64) {
    let (mut errors, mut warnings, mut fyi) = (0u64, 0u64, 0u64);
    for items in findings.values() {
        for f in items {
            match f.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
                Severity::Fyi => fyi += 1,
            }
        }
    }
    (errors, warnings, fyi)
}

/// An RFC-3339 UTC timestamp for the cert's `checked_at`, dependency-free
/// (`SystemTime` → civil date via the standard days-from-epoch algorithm) so the
/// extension takes no date crate. Falls back to the epoch if the clock is somehow
/// before 1970 — `checked_at` is provenance, never load-bearing for trust.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339_from_secs(secs)
}

/// Format seconds-since-epoch as an RFC-3339 UTC string. Split out (pure) so the
/// civil-date arithmetic is unit-testable against known epochs.
fn rfc3339_from_secs(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hh, mm, ss) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    // civil_from_days (Howard Hinnant): days since 1970-01-01 → (y, m, d).
    let z = days as i64 + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Load + parse the certificate for `path`, or `None` if there isn't a usable one
/// (absent, unreadable, or a corrupt/unknown-version `.idx`). Every "no usable
/// cert" reason collapses to `None` so the caller cleanly falls back to the
/// validating whole-file path — a broken sidecar must never be a hard error.
fn load_sidecar(ctx: &ClientContext, path: &str) -> Option<Sidecar> {
    let json = super::source::read_bytes(ctx, &idx_path_for(path)).ok()?;
    Sidecar::from_json(&json).ok()
}

/// **Read fast-path.** If a *size-fresh* cert indexes `group`, range-read just that
/// group's bytes (one `seek` + `read`, local or remote) and parse the slice —
/// O(group) instead of O(file). Returns `None` (→ whole-file parse) on any miss:
/// no cert, a size change, a group the cert doesn't index (passthrough/absent), or
/// a slice that won't parse (a same-size *shifted* file — the whole-file path then
/// parses the changed bytes correctly).
pub fn sliced_group(ctx: &ClientContext, path: &str, group: &str) -> Option<AgsGroup> {
    let sidecar = load_sidecar(ctx, path)?;
    let (handle, size) = super::source::open_for_read(ctx, path).ok()?;
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

/// **Verdict fast-path.** Is there a cert proving this exact file already validated
/// clean under *this* checker, covering the request's profile? True ⇒ `validate_ags`
/// returns clean without re-running the rule pass. Requires all of:
/// 1. the cert was minted by this engine identity ([`Sidecar::checker_matches`]),
/// 2. its check profile is at least as strict as the request
///    ([`Sidecar::profile_covers`] — on-disk-file check, edition forcing), and
/// 3. the bytes are byte-identical (strong SHA — a verdict won't ride on size).
///
/// A cheap `open`+`size` gate rejects an obvious size change before the full read.
pub fn clean_verdict_certified(
    ctx: &ClientContext,
    path: &str,
    want_check_files: bool,
    want_forced_edition: Option<&str>,
) -> bool {
    let Some(sidecar) = load_sidecar(ctx, path) else {
        return false;
    };
    if !sidecar.checker_matches(VALIDATOR_NAME, laterite_ags4_validator::VERSION, None) {
        return false; // a different/older engine (or a compat cert): re-validate
    }
    if !sidecar.profile_covers(want_check_files, want_forced_edition) {
        return false; // the cert checked less strictly than asked: re-validate
    }
    // Cheap size gate: avoid a full read when the size already disagrees.
    let Ok((_, size)) = super::source::open_for_read(ctx, path) else {
        return false;
    };
    if !sidecar.size_matches(size) {
        return false;
    }
    // Strong confirmation: read + hash. Cheaper than re-running every rule, and a
    // clean verdict must be tied to the exact bytes.
    match super::source::read_bytes(ctx, path) {
        Ok(bytes) => sidecar.is_fresh_for(&bytes),
        Err(_) => false,
    }
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

    #[test]
    fn now_rfc3339_has_the_iso_shape() {
        let s = now_rfc3339();
        // YYYY-MM-DDTHH:MM:SSZ
        assert_eq!(s.len(), 20, "{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
        assert!(s.ends_with('Z'));
    }

    #[test]
    fn rfc3339_matches_known_epochs() {
        assert_eq!(rfc3339_from_secs(0), "1970-01-01T00:00:00Z");
        // 2026-06-20T01:39:00Z (day 20624 from epoch, ×86400 + 5940)
        assert_eq!(rfc3339_from_secs(1_781_919_540), "2026-06-20T01:39:00Z");
        // a leap-day instant — proves the civil-date arithmetic
        assert_eq!(rfc3339_from_secs(1_582_934_400), "2020-02-29T00:00:00Z");
    }
}
