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

    // validate_ags (opt-in): mini.ags lacks a TRAN group etc., so it has
    // findings; every severity is from the known set.
    let n_findings: i64 = db
        .query_one(&format!("SELECT count(*) FROM validate_ags('{ags}')"))
        .unwrap();
    assert!(
        n_findings > 0,
        "incomplete mini.ags should yield findings, got {n_findings}"
    );
    let bad_sev: i64 = db
        .query_one(&format!(
            "SELECT count(*) FROM validate_ags('{ags}') WHERE severity NOT IN ('error','warning','fyi')"
        ))
        .unwrap();
    assert_eq!(bad_sev, 0, "severities must be error/warning/fyi");

    // validate_ags(path, dict_version := ...): the optional named param forces a
    // bundled dictionary edition. It still yields findings with valid severities.
    let forced_findings: i64 = db
        .query_one(&format!(
            "SELECT count(*) FROM validate_ags('{ags}', dict_version := '4.2')"
        ))
        .unwrap();
    assert!(
        forced_findings > 0,
        "validate_ags(path, dict_version := '4.2') should still produce findings, got {forced_findings}"
    );
    let forced_bad_sev: i64 = db
        .query_one(&format!(
            "SELECT count(*) FROM validate_ags('{ags}', dict_version := '4.2') WHERE severity NOT IN ('error','warning','fyi')"
        ))
        .unwrap();
    assert_eq!(forced_bad_sev, 0, "forced-edition severities must be valid");

    // The severity knobs (#194): error-only by default, the FYI / WARNING tiers
    // opt-in. mini.ags is incomplete, so it already has error findings; turning a
    // tier on must never DROP a finding (monotonic), and severities stay valid.
    let with_tiers: i64 = db
        .query_one(&format!(
            "SELECT count(*) FROM validate_ags('{ags}', warnings := true, fyi := true)"
        ))
        .unwrap();
    assert!(
        with_tiers >= n_findings,
        "warnings/fyi tiers must not drop findings: {with_tiers} < {n_findings}"
    );

    // load_ags_script: the generated SQL materialises queryable, keyed tables.
    let script: String = db
        .query_one(&format!(
            "SELECT string_agg(stmt, chr(10) ORDER BY seq) FROM load_ags_script('{ags}')"
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

/// PR D: the `.ags.idx` certificate lifecycle — `certify_ags` mints, `read_ags`
/// takes the sliced fast-path, `validate_ags` skips re-validation, and the
/// freshness gate refuses a stale cert. Loads the real extension (gated on
/// `LATERITE_AGS4_DYLIB`) and drives it all through SQL.
#[test]
fn cert_lifecycle() {
    let Some(db) = load_extension() else {
        eprintln!("skipping cert E2E: set LATERITE_AGS4_DYLIB to a built liblaterite_duckdb.dylib");
        return;
    };
    // Work on a temp COPY — the test mutates it (overwrite, mint a sibling
    // .idx), and must never touch the committed fixture.
    let dir = std::env::temp_dir().join(format!("laterite_ags4_cert_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp cert dir");
    let clean = dir.join("clean.ags");
    std::fs::copy(clean_fixture(), &clean).expect("copy clean.ags fixture");
    let clean = clean.display().to_string();
    let idx = format!("{clean}.idx");

    // --- mint: a clean file certifies; 4 groups, no errors, an .idx appears ---
    let certified: bool = db
        .query_one(&format!("SELECT certified FROM certify_ags('{clean}')"))
        .unwrap();
    assert!(certified, "a clean file should certify");
    let groups: i64 = db
        .query_one(&format!("SELECT groups FROM certify_ags('{clean}')"))
        .unwrap();
    assert_eq!(groups, 4, "PROJ + TRAN + UNIT + TYPE");
    let errors: i64 = db
        .query_one(&format!("SELECT errors FROM certify_ags('{clean}')"))
        .unwrap();
    assert_eq!(errors, 0);

    // the cert is a well-formed, cross-surface `.ags.idx`: version 1, this
    // engine's identity, a 64-hex SHA — the SAME shape the Python wheel writes.
    let cert_json = std::fs::read_to_string(&idx).expect("certify wrote <path>.idx");
    assert!(
        cert_json.contains("\"version\": 1"),
        "cert json: {cert_json}"
    );
    assert!(
        cert_json.contains("\"validator\": \"laterite_ags4\""),
        "cert carries the engine identity: {cert_json}"
    );

    // --- consume (read): with a fresh cert present, read_ags slices one group's
    // bytes; the result is identical to the whole-file read (the correctness
    // guarantee — slice parity itself is exhaustively unit-tested in core). ---
    let proj_rows: i64 = db
        .query_one(&format!("SELECT count(*) FROM read_ags('{clean}','PROJ')"))
        .unwrap();
    assert_eq!(proj_rows, 1, "PROJ has one DATA row via the slice path");
    let proj_id: String = db
        .query_one(&format!("SELECT proj_id FROM read_ags('{clean}','PROJ')"))
        .unwrap();
    assert_eq!(proj_id, "P1");

    // --- consume (validate): a fresh, matching cert means validate_ags returns
    // clean without re-running the rule pass. (Clean either way, so this asserts
    // the contract; the staleness case below proves the gate actually bites.) ---
    let findings_fresh: i64 = db
        .query_one(&format!("SELECT count(*) FROM validate_ags('{clean}')"))
        .unwrap();
    assert_eq!(findings_fresh, 0, "a clean certified file validates clean");

    // --- refuse: a file WITH errors is not certified and writes no .idx ---
    let mini = fixture().display().to_string();
    let mini_certified: bool = db
        .query_one(&format!("SELECT certified FROM certify_ags('{mini}')"))
        .unwrap();
    assert!(!mini_certified, "an invalid file must not certify");
    let mini_errors: i64 = db
        .query_one(&format!("SELECT errors FROM certify_ags('{mini}')"))
        .unwrap();
    assert!(mini_errors > 0, "the invalid file reports its error count");
    assert!(
        !std::path::Path::new(&format!("{mini}.idx")).exists(),
        "no cert is written for an invalid file"
    );

    // --- freshness gate: overwrite the certified file with DIFFERENT, invalid
    // content (size + SHA now differ) while its clean `.idx` still sits beside
    // it. The cert must NOT be trusted: validate_ags re-runs and surfaces the new
    // file's findings — observable proof the stale cert was rejected. ---
    std::fs::write(&clean, std::fs::read(&mini).unwrap()).expect("overwrite clean.ags");
    let findings_stale: i64 = db
        .query_one(&format!("SELECT count(*) FROM validate_ags('{clean}')"))
        .unwrap();
    assert!(
        findings_stale > 0,
        "a size-changed file's stale cert is ignored; real findings surface ({findings_stale})"
    );
    // and the read path likewise ignores the stale cert: it reads the NEW content
    // (mini.ags has a LOCA group; the original clean.ags did not).
    let loca_now: i64 = db
        .query_one(&format!("SELECT count(*) FROM read_ags('{clean}','LOCA')"))
        .unwrap();
    assert_eq!(
        loca_now, 2,
        "stale cert ignored — read sees the new content"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// #294 #12: the `encoding` named param on `read_ags` / `validate_ags`. A
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

    // validate_ags(..., encoding := 'windows-1252') runs under the right decode —
    // no crash, severities stay valid.
    let bad_sev: i64 = db
        .query_one(&format!(
            "SELECT count(*) FROM validate_ags('{p}', encoding := 'windows-1252') WHERE severity NOT IN ('error','warning','fyi')"
        ))
        .expect("windows-1252 validate should run");
    assert_eq!(bad_sev, 0, "encoded validate must produce valid severities");

    // certify_ags(..., encoding := 'windows-1252') accepts the same knob and runs
    // under the decode (this single-group fixture has findings so it won't certify,
    // but the call must succeed — parity with read_ags/validate_ags).
    let enc_certify: Result<bool, _> = db.query_one(&format!(
        "SELECT certified FROM certify_ags('{p}', encoding := 'windows-1252')"
    ));
    assert!(
        matches!(enc_certify, Ok(false)),
        "certify_ags must accept `encoding` and run (findings → not certified): {enc_certify:?}"
    );

    // An unrecognised label is a clean bind error, not a panic.
    let bad_enc: Result<i64, _> = db.query_one(&format!(
        "SELECT count(*) FROM read_ags('{p}', 'PROJ', encoding := 'not-a-real-encoding')"
    ));
    assert!(bad_enc.is_err(), "an unknown encoding label must error");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The content variants: `validate_ags_text` / `certify_ags_text` take the AGS4
/// as a VARCHAR (no path), the twins of `read_ags_text`. Same verdicts as the
/// path verbs; `certify_ags_text` returns the cert JSON in a column instead of
/// writing an `.ags.idx`.
#[test]
fn text_verbs() {
    let Some(db) = load_extension() else {
        eprintln!("skipping text-verbs E2E: set LATERITE_AGS4_DYLIB to a built liblaterite_duckdb.dylib");
        return;
    };
    let clean = std::fs::read_to_string(clean_fixture()).expect("read clean.ags");
    let mini = std::fs::read_to_string(fixture()).expect("read mini.ags");

    // --- validate_ags_text: same verdict as validate_ags, from content ---
    let clean_findings: i64 = db
        .query_one(&format!(
            "SELECT count(*) FROM validate_ags_text({})",
            sql_str(&clean)
        ))
        .expect("validate_ags_text on clean content");
    assert_eq!(clean_findings, 0, "clean content validates clean");
    let clean_path_findings: i64 = db
        .query_one(&format!(
            "SELECT count(*) FROM validate_ags('{}')",
            clean_fixture().display()
        ))
        .unwrap();
    assert_eq!(
        clean_findings, clean_path_findings,
        "text and path validate agree on the clean file"
    );
    let mini_errors: i64 = db
        .query_one(&format!(
            "SELECT count(*) FROM validate_ags_text({}) WHERE severity = 'error'",
            sql_str(&mini)
        ))
        .expect("validate_ags_text on invalid content");
    assert!(mini_errors > 0, "invalid content surfaces errors");
    // the warnings knob still applies (errors-only drops the warning tier)
    let errors_only: i64 = db
        .query_one(&format!(
            "SELECT count(*) FROM validate_ags_text({}, warnings := false) WHERE severity = 'warning'",
            sql_str(&mini)
        ))
        .unwrap();
    assert_eq!(errors_only, 0, "warnings := false drops the warning tier");

    // --- certify_ags_text: clean content → certified, cert JSON in a column ---
    let certified: bool = db
        .query_one(&format!(
            "SELECT certified FROM certify_ags_text({})",
            sql_str(&clean)
        ))
        .expect("certify_ags_text on clean content");
    assert!(certified, "clean content certifies");
    let cert_json: String = db
        .query_one(&format!(
            "SELECT cert FROM certify_ags_text({})",
            sql_str(&clean)
        ))
        .expect("certify_ags_text returns the cert JSON");
    // the SAME cross-surface `.ags.idx` shape the path verb / the Python wheel write
    assert!(cert_json.contains("\"version\": 1"), "cert json: {cert_json}");
    assert!(
        cert_json.contains("\"validator\": \"laterite_ags4\""),
        "cert carries the engine identity: {cert_json}"
    );
    let groups: i64 = db
        .query_one(&format!(
            "SELECT groups FROM certify_ags_text({})",
            sql_str(&clean)
        ))
        .unwrap();
    assert_eq!(groups, 4, "PROJ + TRAN + UNIT + TYPE");

    // --- refuse: invalid content is not certified and its cert column is NULL ---
    let mini_certified: bool = db
        .query_one(&format!(
            "SELECT certified FROM certify_ags_text({})",
            sql_str(&mini)
        ))
        .unwrap();
    assert!(!mini_certified, "invalid content must not certify");
    let cert_is_null: bool = db
        .query_one(&format!(
            "SELECT cert IS NULL FROM certify_ags_text({})",
            sql_str(&mini)
        ))
        .unwrap();
    assert!(cert_is_null, "an uncertified file has a NULL cert column");
}

/// A DuckDB single-quoted string literal (doubling any interior quote). AGS4 uses
/// double-quotes, so this is just the wrapper in practice — but escape anyway.
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A complete, valid AGS4 4.2 file (PROJ + TRAN + UNIT + TYPE) that validates
/// with zero findings — the precondition `certify_ags` requires. CRLF as the spec
/// mandates; mirrors the Python cert suite's fixture so both surfaces certify the
/// same bytes.
fn clean_fixture() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/clean.ags")
}

/// Footer + LOAD the built extension into a fresh unsigned in-memory DuckDB.
/// `None` when `LATERITE_AGS4_DYLIB` is unset (test self-skips).
fn load_extension() -> Option<InMemoryDb> {
    let dylib = std::env::var("LATERITE_AGS4_DYLIB").ok()?;
    let mut ext = std::fs::read(&dylib).expect("read LATERITE_AGS4_DYLIB");
    ext.extend_from_slice(&metadata_footer());
    // DuckDB derives the init symbol from the file's basename, so the file MUST
    // be named exactly `laterite_ags4.duckdb_extension` (→ `laterite_ags4_init_c_api`).
    // A per-process subdir keeps that fixed name collision-free.
    let dir = std::env::temp_dir().join(format!("laterite_ags4_e2e_{}", std::process::id()));
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
