//! End-to-end: load the built `laterite_ags4` extension into a real
//! in-process DuckDB and exercise `read_ags` through SQL.
//!
//! Loadable-extension testing is inherently two-step (the cdylib is built on
//! the `loadable-extension` path; the host DuckDB here is `bundled-test`), so
//! this test is **gated on `LATERITE_AGS4_DYLIB`** — the path to a freshly
//! built `liblaterite_duckdb.dylib`. Without it, the test self-skips, so a
//! plain `cargo test` stays green. Drive the full flow with
//! `tools/test-duckdb-ext.sh`, which builds the loadable cdylib, freezes a
//! copy, and re-runs this test with the env var set.

use quack_rs::testing::InMemoryDb;

/// Smoke: the bundled in-process DuckDB works at all (also keeps `cargo test`
/// meaningful when the gated E2E self-skips).
#[test]
fn in_process_duckdb_works() {
    let db = InMemoryDb::open().unwrap();
    let answer: i64 = db.query_one("SELECT 6 * 7").unwrap();
    assert_eq!(answer, 42);
}

/// The P1 flagship: born-typed columns + deterministic-key joins, verified by
/// loading the real extension and running SQL.
#[test]
fn read_ags_typed_and_keyed() {
    let Some(db) = load_extension() else {
        eprintln!(
            "skipping read_ags E2E: set LATERITE_AGS4_DYLIB to a built liblaterite_duckdb.dylib"
        );
        return;
    };
    let ags = fixture().display().to_string();

    // Born-typed: a 2DP heading is DOUBLE, an ID is VARCHAR.
    let ty: String = db
        .query_one(&format!(
            "SELECT typeof(loca_gl) FROM read_ags('{ags}','LOCA') LIMIT 1"
        ))
        .unwrap();
    assert_eq!(ty, "DOUBLE", "2DP heading LOCA_GL should be DOUBLE");
    let ty: String = db
        .query_one(&format!(
            "SELECT typeof(loca_id) FROM read_ags('{ags}','LOCA') LIMIT 1"
        ))
        .unwrap();
    assert_eq!(ty, "VARCHAR", "ID heading LOCA_ID should be VARCHAR");

    // Born-typed value (string "100.50" arrives as the double 100.5).
    let gl: f64 = db
        .query_one(&format!(
            "SELECT loca_gl FROM read_ags('{ags}','LOCA') WHERE loca_id='BH01'"
        ))
        .unwrap();
    assert!(
        (gl - 100.5).abs() < 1e-9,
        "LOCA_GL for BH01 should be 100.5, got {gl}"
    );

    // Every row carries a non-null deterministic _id.
    let with_id: i64 = db
        .query_one(&format!(
            "SELECT count(*) FROM read_ags('{ags}','LOCA') WHERE _id IS NOT NULL"
        ))
        .unwrap();
    assert_eq!(with_id, 2);

    // The crux: every SAMP row joins to its LOCA via `_parent_id = _id`, across
    // two independent read_ags calls, with zero orphans — the by-construction
    // FK with no shared state.
    let matched: i64 = db
        .query_one(&format!(
            "SELECT count(*) FROM read_ags('{ags}','SAMP') s \
             JOIN read_ags('{ags}','LOCA') l ON s._parent_id = l._id"
        ))
        .unwrap();
    assert_eq!(matched, 3, "all 3 SAMP rows should join to a LOCA parent");

    let orphans: i64 = db
        .query_one(&format!(
            "SELECT count(*) FROM read_ags('{ags}','SAMP') s \
             LEFT JOIN read_ags('{ags}','LOCA') l ON s._parent_id = l._id \
             WHERE l._id IS NULL"
        ))
        .unwrap();
    assert_eq!(
        orphans, 0,
        "no SAMP row should have an unresolved _parent_id"
    );

    // --- P2 metadata functions ---

    // ags_groups: 3 groups; LOCA has 2 rows and parent PROJ.
    let n_groups: i64 = db
        .query_one(&format!("SELECT count(*) FROM ags_groups('{ags}')"))
        .unwrap();
    assert_eq!(n_groups, 3);
    let loca_rows: i64 = db
        .query_one(&format!(
            "SELECT n_rows FROM ags_groups('{ags}') WHERE \"group\"='LOCA'"
        ))
        .unwrap();
    assert_eq!(loca_rows, 2);
    let loca_parent: String = db
        .query_one(&format!(
            "SELECT parent FROM ags_groups('{ags}') WHERE \"group\"='LOCA'"
        ))
        .unwrap();
    assert_eq!(loca_parent, "PROJ");

    // ags_headings: the file's own units/types, enriched with KEY status —
    // SAMP_TOP is a 2DP KEY → DOUBLE + is_key.
    let samp_top_sql: String = db
        .query_one(&format!(
            "SELECT sql_type FROM ags_headings('{ags}') WHERE \"group\"='SAMP' AND heading='SAMP_TOP'"
        ))
        .unwrap();
    assert_eq!(samp_top_sql, "DOUBLE");
    let samp_top_key: bool = db
        .query_one(&format!(
            "SELECT is_key FROM ags_headings('{ags}') WHERE \"group\"='SAMP' AND heading='SAMP_TOP'"
        ))
        .unwrap();
    assert!(samp_top_key, "SAMP_TOP should be flagged is_key");

    // ags_dictionary: the embedded registry is queryable (LOCA has headings).
    let loca_dict: i64 = db
        .query_one("SELECT count(*) FROM ags_dictionary() WHERE \"group\"='LOCA'")
        .unwrap();
    assert!(loca_dict > 0, "ags_dictionary should expose LOCA headings");

    // ags_relationships: SAMP's parent is LOCA, sharing LOCA_ID.
    let samp_rel: String = db
        .query_one(
            "SELECT parent || ':' || shared_keys FROM ags_relationships() WHERE child='SAMP'",
        )
        .unwrap();
    assert!(
        samp_rel.starts_with("LOCA:"),
        "SAMP parent should be LOCA, got {samp_rel}"
    );
    assert!(
        samp_rel.contains("LOCA_ID"),
        "shared_keys should contain LOCA_ID, got {samp_rel}"
    );

    // load_ags: the generated SQL materialises queryable, keyed tables.
    let script: String = db
        .query_one(&format!(
            "SELECT string_agg(stmt, chr(10) ORDER BY seq) FROM load_ags('{ags}')"
        ))
        .unwrap();
    assert!(
        script.contains("CREATE TABLE ags_loca"),
        "script should create ags_loca, got: {script}"
    );
    db.execute_batch(&script).unwrap();
    let loca_count: i64 = db.query_one("SELECT count(*) FROM ags_loca").unwrap();
    assert_eq!(loca_count, 2, "materialised ags_loca should have 2 rows");
    // The persisted tables join on the same deterministic keys.
    let joined: i64 = db
        .query_one("SELECT count(*) FROM ags_samp s JOIN ags_loca l ON s._parent_id = l._id")
        .unwrap();
    assert_eq!(joined, 3, "persisted SAMP-LOCA join should match all rows");

    // Virtual-filesystem read path: every read above now goes through DuckDB's
    // VFS (the same path that serves `http(s)://`/`s3://` with `LOAD httpfs`),
    // so a clean local read here exercises that plumbing. A nonexistent file
    // must surface a clean bind error rather than panic — the contract a remote
    // 404 also relies on.
    let missing: Result<i64, _> =
        db.query_one("SELECT count(*) FROM read_ags('/nonexistent/laterite_no_such.ags','LOCA')");
    assert!(
        missing.is_err(),
        "reading a nonexistent file through the VFS should error, not succeed"
    );
}

/// #294 #12: the `encoding` named param on `read_ags`. A
/// windows-1252 file (a non-UTF-8 byte in a DATA value) decodes correctly ONLY
/// when the label is supplied. The fixture is written at runtime — a committed
/// non-UTF-8 `.ags` would be mangled by the repo's `*.ags text=crlf` attribute.
#[test]
fn encoding_named_param() {
    let Some(db) = load_extension() else {
        eprintln!(
            "skipping encoding E2E: set LATERITE_AGS4_DYLIB to a built liblaterite_duckdb.dylib"
        );
        return;
    };
    let dir = std::env::temp_dir().join(format!("laterite_ags4_enc_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp enc dir");
    let path = dir.join("cp1252.ags");
    // PROJ_NAME = "Café", with 'é' as the single windows-1252 byte 0xE9 (which is
    // NOT valid UTF-8), so the two decodes diverge observably.
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(
        b"\"GROUP\",\"PROJ\"\r\n\"HEADING\",\"PROJ_ID\",\"PROJ_NAME\"\r\n\"UNIT\",\"\",\"\"\r\n\"TYPE\",\"ID\",\"X\"\r\n\"DATA\",\"P1\",\"Caf",
    );
    bytes.push(0xE9); // 'é' in windows-1252
    bytes.extend_from_slice(b"\"\r\n");
    std::fs::write(&path, &bytes).expect("write cp1252 fixture");
    let p = path.display().to_string();

    // read_ags(..., encoding := 'windows-1252') decodes 0xE9 -> 'é'.
    let name: String = db
        .query_one(&format!(
            "SELECT proj_name FROM read_ags('{p}', 'PROJ', encoding := 'windows-1252')"
        ))
        .expect("windows-1252 read should succeed");
    assert_eq!(name, "Café", "windows-1252 read must decode 0xE9 as é");

    // The default UTF-8 read must NOT yield "Café" (0xE9 is invalid UTF-8 → a
    // replacement char or a read error — tolerant of either).
    let default: Result<String, _> =
        db.query_one(&format!("SELECT proj_name FROM read_ags('{p}', 'PROJ')"));
    assert!(
        default.as_deref().map(|s| s != "Café").unwrap_or(true),
        "default UTF-8 read must not decode the cp1252 byte as é (got {default:?})"
    );

    // An unrecognised label is a clean bind error, not a panic.
    let bad_enc: Result<i64, _> = db.query_one(&format!(
        "SELECT count(*) FROM read_ags('{p}', 'PROJ', encoding := 'not-a-real-encoding')"
    ));
    assert!(bad_enc.is_err(), "an unknown encoding label must error");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Footer + LOAD the built extension into a fresh unsigned in-memory DuckDB.
/// `None` when `LATERITE_AGS4_DYLIB` is unset (test self-skips).
fn load_extension() -> Option<InMemoryDb> {
    let dylib = std::env::var("LATERITE_AGS4_DYLIB").ok()?;
    let mut ext = std::fs::read(&dylib).expect("read LATERITE_AGS4_DYLIB");
    ext.extend_from_slice(&metadata_footer());
    // DuckDB derives the init symbol from the file's basename, so the file MUST
    // be named exactly `laterite_ags4.duckdb_extension` (→ `laterite_ags4_init_c_api`).
    // `cargo test` runs these e2e tests as parallel THREADS in one process, so a
    // process-id subdir is NOT unique per test — they'd race writing + loading the
    // same fixed-name file (an intermittent "<512 bytes / not a DuckDB extension"
    // LOAD as one thread reads mid-write from another). A per-CALL counter isolates
    // each test's extension file while preserving the required basename.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "laterite_ags4_e2e_{}_{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&dir).expect("create temp ext dir");
    let out = dir.join("laterite_ags4.duckdb_extension");
    std::fs::write(&out, &ext).expect("write .duckdb_extension");

    let db = InMemoryDb::open_unsigned().expect("open unsigned in-memory DuckDB");
    // Locally-built artifact: tolerate platform/version-field mismatch.
    db.execute_batch("SET allow_extensions_metadata_mismatch=true")
        .unwrap();
    db.execute_batch(&format!("LOAD '{}'", out.display()))
        .expect("LOAD laterite_ags4 extension");
    Some(db)
}

fn fixture() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini.ags")
}

/// The 512-byte DuckDB extension metadata footer (C_STRUCT ABI, unsigned).
/// Layout per DuckDB `ParseExtensionMetaData`: eight 32-byte null-terminated
/// ASCII fields (reserved×3, abi, ext-version, duckdb-C-API-version, platform,
/// magic "4") then a zero-filled 256-byte signature area.
fn metadata_footer() -> Vec<u8> {
    const FIELD: usize = 32;
    let fields = ["", "", "", "C_STRUCT", "v0.4.0", "v1.2.0", platform(), "4"];
    let mut block = vec![0u8; 512];
    for (i, s) in fields.iter().enumerate() {
        let b = s.as_bytes();
        block[i * FIELD..i * FIELD + b.len()].copy_from_slice(b);
    }
    block
}

const fn platform() -> &'static str {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "osx_arm64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "osx_amd64"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux_amd64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux_arm64"
    } else {
        "windows_amd64"
    }
}
