//! End-to-end: load the built `laterite_ags4` extension into a real DuckDB and
//! exercise it through SQL.
//!
//! ## Phase 0 status — read this before "fixing" the skip
//!
//! The intended host here is an in-process **bundled** `duckdb::Connection`
//! (dev-dep `duckdb = { features = ["bundled"] }`). Under `cargo test`, Cargo
//! unifies features across the normal + dev dependency graph, so `libduckdb-sys`
//! is built with **`loadable-extension`** (required by the extension crate)
//! *and* `bundled`. With `loadable-extension` on, every `duckdb_*` call
//! dispatches through a C-API function-pointer struct that is only populated by
//! `duckdb_rs_extension_api_init` — which the *host* calls at extension load.
//! In a standalone test binary nothing calls it, so `Connection::open_in_memory`
//! hits `duckdb_open` with a null pointer and asserts ("DuckDB API not
//! initialized"). i.e. an in-process bundled host is **not viable** in the same
//! `cargo test` as a loadable-extension crate.
//!
//! (The old quack-rs setup dodged this because quack vendored its *own* sys
//! crate for its bundled test host — no unification with our loadable
//! `libduckdb-sys`. duckdb-rs shares one `libduckdb-sys`, so the conflict is
//! unavoidable here.)
//!
//! So the [`load_extension`] helper + [`ags_groups_bundled_host`] below are kept
//! (they compile and pin the intended shape) but **`#[ignore]`d**: they only run
//! under `cargo test -- --ignored`, where they will assert as described. The
//! functional Phase-0 gate runs `ags_groups` against an **external** DuckDB
//! (the sqllogictest/`make`-configured DuckDB, or a standalone `duckdb` host
//! loading `build/debug/laterite_ags4.duckdb_extension`). Resolving an
//! in-process host is a later-phase concern (e.g. a separate e2e package that
//! does not depend on the loadable-extension crate).

use duckdb::{Config, Connection};

fn fixture() -> String {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mini.ags")
        .display()
        .to_string()
}

/// LOAD the metadata-stamped extension (path in `LATERITE_AGS4_EXT`, e.g.
/// `build/debug/laterite_ags4.duckdb_extension`) into a fresh unsigned
/// in-memory DuckDB. See the module note on why this can't work under
/// `cargo test` feature unification.
fn load_extension() -> Option<Connection> {
    let ext = std::env::var("LATERITE_AGS4_EXT").ok()?;
    let config = Config::default().allow_unsigned_extensions().ok()?;
    let con = Connection::open_in_memory_with_flags(config).ok()?;
    // Locally-built artifact: tolerate platform/version-field mismatch.
    con.execute_batch("SET allow_extensions_metadata_mismatch=true")
        .ok()?;
    con.execute_batch(&format!("LOAD '{ext}'")).ok()?;
    Some(con)
}

/// Phase-0 flagship: `ags_groups` returns the file's 3 groups, LOCA has 2 rows,
/// and its registry parent is PROJ. Ignored — see the module note (asserts under
/// `cargo test` feature unification); the functional gate is external.
#[test]
#[ignore = "in-process bundled host is non-viable under loadable-extension feature unification; see module note"]
fn ags_groups_bundled_host() {
    let Some(con) = load_extension() else {
        eprintln!("skipping: set LATERITE_AGS4_EXT to a stamped .duckdb_extension");
        return;
    };
    let ags = fixture();

    let n: i64 = con
        .query_row(
            &format!("SELECT count(*) FROM ags_groups('{ags}')"),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 3, "mini.ags has 3 groups (PROJ, LOCA, SAMP)");

    let loca_rows: i64 = con
        .query_row(
            &format!("SELECT n_rows FROM ags_groups('{ags}') WHERE \"group\"='LOCA'"),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(loca_rows, 2);

    let loca_parent: String = con
        .query_row(
            &format!("SELECT parent FROM ags_groups('{ags}') WHERE \"group\"='LOCA'"),
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(loca_parent, "PROJ");
}
