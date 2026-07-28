//! Gate: `functions.json` is the canonical manifest of the SQL table functions
//! this extension registers, and it must match the actual `register_table(con,
//! …)` calls in `src/`. A function added or removed in the Rust source then
//! can't silently drift from the manifest that downstream consumers (the docs
//! site) pin instead of hand-listing the surface. `description.yml`'s prose
//! Functions table had already lost `ags_rules` exactly this way; a
//! machine-checked manifest makes that class of drift impossible.
//!
//! Source-level on purpose (parses the `register_table` call sites) — loading
//! the extension in-process is not viable under `cargo test`, see `e2e.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// What the source actually registers: name -> (positional arity, named params),
/// read straight off the `register_table(con, "name", <arity>, &[<named>], …)`
/// call sites — the same shape every module uses.
fn registered() -> BTreeMap<String, (usize, Vec<String>)> {
    const MARKER: &str = "register_table(con, \"";
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(&src).expect("read src/") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("read src file");
        let mut rest = text.as_str();
        while let Some(i) = rest.find(MARKER) {
            let after_name = &rest[i + MARKER.len()..];
            let end = after_name
                .find('"')
                .expect("register_table name closing quote");
            let name = after_name[..end].to_string();
            let tail = &after_name[end + 1..]; // e.g. `, 2, &["encoding"], |bind…`

            let arity: usize = tail
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .expect("register_table arity");

            let lb = tail.find("&[").expect("register_table named-param slice");
            let rb = lb + tail[lb..].find(']').expect("named-param slice close");
            let named: Vec<String> = tail[lb + 2..rb]
                .split(',')
                .map(|s| s.trim().trim_matches('"').to_string())
                .filter(|s| !s.is_empty())
                .collect();

            out.insert(name, (arity, named));
            rest = tail;
        }
    }
    out
}

/// The manifest's `version` must equal the crate's. Downstream consumers (the
/// docs repo) pin a copy of `functions.json`, so a release that bumped only
/// `Cargo.toml` + `description.yml` published a manifest still claiming the
/// previous version — silently, since nothing compared the two. The release
/// driver bumps all three (`scripts/release.sh`); this is what makes that
/// mandatory rather than remembered.
#[test]
fn manifest_version_matches_crate() {
    let raw = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("functions.json"))
        .expect("read functions.json");
    let manifest: serde_json::Value = serde_json::from_str(&raw).expect("parse functions.json");
    let declared = manifest["version"]
        .as_str()
        .expect("manifest version string");
    assert_eq!(
        declared,
        env!("CARGO_PKG_VERSION"),
        "functions.json version != Cargo.toml version — bump all three version \
         stamps together (scripts/release.sh does this)"
    );
}

/// Every registered function needs a row in the docs CSV. The community docs
/// page renders that file, so a function absent from it ships undocumented —
/// which is exactly what happened to `ags_rules` (registered, described in
/// `description.yml`, but never given a CSV row) and to `to_duckdb` (present
/// here, but never synced upstream, so the published table was a release
/// behind). The sync half is fixed in `scripts/release.sh`; this is the half a
/// test can hold.
#[test]
fn docs_csv_covers_every_registered_function() {
    let raw = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("functions.json"))
        .expect("read functions.json");
    let manifest: serde_json::Value = serde_json::from_str(&raw).expect("parse functions.json");

    let csv = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/function_descriptions.csv"),
    )
    .expect("read docs/function_descriptions.csv");
    // The first field of each row is the function name; quoting only ever starts
    // from the second field, so splitting on the first comma is sufficient here.
    let documented: Vec<&str> = csv
        .lines()
        .skip(1) // header
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split(',').next().unwrap_or("").trim())
        .collect();

    let mut missing: Vec<&str> = manifest["functions"]
        .as_array()
        .expect("functions array")
        .iter()
        .map(|f| f["name"].as_str().expect("name"))
        .filter(|n| !documented.contains(n))
        .collect();
    missing.sort_unstable();
    assert!(
        missing.is_empty(),
        "registered but absent from docs/function_descriptions.csv: {missing:?} \
         — add a row, or the community docs page ships without them"
    );
}

#[test]
fn manifest_matches_registered_functions() {
    let raw = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("functions.json"))
        .expect("read functions.json");
    let manifest: serde_json::Value = serde_json::from_str(&raw).expect("parse functions.json");
    let funcs = manifest["functions"].as_array().expect("functions array");

    let src = registered();

    let mut manifest_names: Vec<&str> = funcs
        .iter()
        .map(|f| f["name"].as_str().expect("name"))
        .collect();
    manifest_names.sort_unstable();
    let mut src_names: Vec<&str> = src.keys().map(String::as_str).collect();
    src_names.sort_unstable();
    assert_eq!(
        manifest_names, src_names,
        "functions.json names != the register_table() names in src/ — update functions.json"
    );

    for f in funcs {
        let name = f["name"].as_str().unwrap();
        let (arity, named) = src.get(name).expect("checked names match above");
        let params = f["params"].as_array().expect("params array").len();
        assert_eq!(
            params, *arity,
            "{name}: functions.json lists {params} params but register_table declares arity {arity}"
        );
        let manifest_named: Vec<&str> = f["named_params"]
            .as_array()
            .expect("named_params array")
            .iter()
            .map(|v| v.as_str().expect("named param str"))
            .collect();
        assert_eq!(
            &manifest_named, named,
            "{name}: functions.json named_params != register_table's named-param list"
        );
    }
}
