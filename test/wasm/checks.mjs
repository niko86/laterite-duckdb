// WHAT the wasm functional gate asserts — shared by every harness that runs it.
//
// Two harnesses LOAD the wasm artifact: `gate.mjs` (node, duckdb-wasm's
// node-blocking runtime, `wasm_mvp` + `wasm_eh`) and `browser-gate.mjs`
// (Chromium, the `coi` runtime, `wasm_threads`). They differ only in HOW a query
// reaches DuckDB, so the assertions live here and each harness hands in a
// runner. Kept in one place so the browser gate cannot quietly become the
// weaker check (#50).
//
// A harness supplies:
//   attempt(sql)            -> { ok: true, rows } | { ok: false, message }
//                              rows are plain objects (Arrow `toJSON()`).
//   registerBuffer(name, u8) registers bytes under `name` in DuckDB's VFS.
//   pathReaders             extra { what, path } sources `read_ags` must read —
//                              the node host's filesystem, the browser's http URL.
//   repositoryUrl           where the staged artifact is served from.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const REPO = path.resolve(HERE, '..', '..');

// --- the version-alignment invariant ------------------------------------
// The extension is built with USE_UNSTABLE_C_API=1: it compiles against one
// exact C-API struct layout, and is stamped for that DuckDB version. The
// @duckdb/duckdb-wasm pin in package.json must therefore embed the same DuckDB
// as TARGET_DUCKDB_VERSION. Read the Makefile rather than restating the version
// here — one source, no drift.
export function targetDuckdbVersion() {
    const makefile = fs.readFileSync(path.join(REPO, 'Makefile'), 'utf8');
    const v = /^TARGET_DUCKDB_VERSION=(\S+)$/m.exec(makefile)?.[1];
    if (!v) {
        console.error('gate: could not read TARGET_DUCKDB_VERSION from the Makefile');
        process.exit(2);
    }
    return v;
}

export function artifactPath(platform) {
    return path.join(REPO, 'build', platform, 'extension', 'laterite_ags4', 'laterite_ags4.duckdb_extension.wasm');
}

export const FIXTURE_BYTES = () => fs.readFileSync(path.join(REPO, 'test', 'sql', 'mini.ags'));

// --- assertion plumbing --------------------------------------------------
export const show = (v) => JSON.stringify(v, (_k, x) => (typeof x === 'bigint' ? Number(x) : x));

export function makeReport() {
    const report = { failures: 0 };
    report.check = (what, actual, expected) => {
        const ok = show(actual) === show(expected);
        console.log(`${ok ? '  ok  ' : ' FAIL '} ${what}`);
        if (!ok) {
            report.failures++;
            console.log(`         expected: ${show(expected)}`);
            console.log(`         actual:   ${show(actual)}`);
        }
        return ok;
    };
    report.checkThat = (what, ok, detail) => {
        console.log(`${ok ? '  ok  ' : ' FAIL '} ${what}`);
        if (!ok) {
            report.failures++;
            if (detail !== undefined) console.log(`         ${detail}`);
        }
        return ok;
    };
    return report;
}

// Returns the number of failed checks. Stops early (and says why) where every
// check below would only be a consequence of the same cause.
export async function runChecks({ platform, attempt, registerBuffer, pathReaders = [], repositoryUrl, report = makeReport() }) {
    const { check, checkThat } = report;
    const TARGET_DUCKDB_VERSION = targetDuckdbVersion();
    const rows = async (sql) => {
        const r = await attempt(sql);
        if (!r.ok) throw new Error(`${sql.slice(0, 80)}: ${r.message}`);
        return r.rows;
    };

    // --- 1. host/artifact version alignment ---------------------------------
    // This is the go/no-go, and it has to be asserted rather than relied upon: the
    // wasm loader does NOT enforce the stamped duckdb_version — an artifact
    // footered for the wrong release still loads — so a drifted pin would surface
    // as ABI-level misbehaviour somewhere below, or as nothing at all, rather than
    // as a refused load. Name the cause here while it is still nameable.
    const host = (await rows('SELECT version() AS version, (SELECT platform FROM pragma_platform()) AS platform'))[0];
    if (!check(`host DuckDB is ${TARGET_DUCKDB_VERSION} (Makefile TARGET_DUCKDB_VERSION)`,
        host.version, TARGET_DUCKDB_VERSION)) {
        console.log(`\n  The @duckdb/duckdb-wasm pin in test/wasm/package.json embeds DuckDB ${host.version},`);
        console.log(`  but this extension is stamped for ${TARGET_DUCKDB_VERSION} and built against that exact`);
        console.log('  C API (USE_UNSTABLE_C_API=1). Re-pin the npm version to the release that carries');
        console.log(`  DuckDB ${TARGET_DUCKDB_VERSION}, or move TARGET_DUCKDB_VERSION — nothing below can pass first.\n`);
        return report.failures;
    }
    check(`host platform is ${platform}`, host.platform, platform);

    // Does this host report DuckDB's own errors intelligibly? duckdb-wasm's
    // `wasm_mvp` runtime loses every C++ exception to a JS `_setThrew is not
    // defined` — including for plain `SELECT * FROM no_such_table`, with no
    // extension loaded at all — while `wasm_eh` reports them verbatim. Probe it
    // rather than hard-coding which variant is which, so the error-message
    // assertion at the end tightens on its own once the host is fixed.
    const hostErrors = await attempt('SELECT * FROM a_table_that_does_not_exist');
    const hostReportsErrors = !hostErrors.ok && /Catalog Error/i.test(hostErrors.message);
    console.log(`  --   host error reporting: ${hostReportsErrors ? 'intelligible' : `degraded (${hostErrors.message?.slice(0, 60)})`}`);

    // --- 2. LOAD the artifact -----------------------------------------------
    const load = await attempt(`SET custom_extension_repository='${repositoryUrl}'`);
    checkThat('SET custom_extension_repository', load.ok, load.message);
    const loaded = await attempt('LOAD laterite_ags4');
    if (!checkThat('LOAD laterite_ags4 (fetched from the local extension repository)', loaded.ok, loaded.message)) {
        // The one failure here that is about the toolchain rather than the code, and
        // it is unreadable without this: an emsdk newer than the pinned 3.1.71
        // defaults WASM_BIGINT ON, so the side module imports `invoke_*` with real
        // i64 parameters while duckdb-wasm's main module offers the legalized
        // i32-pair form. CI pins the right emsdk; a local build may not.
        if (/shared state of memory/.test(loaded.message ?? '')) {
            // The coi host imports a SHARED memory; a side module linked without
            // -pthread declares a non-shared one, and wasm refuses the pairing.
            // extension-ci-tools links every wasm variant with the same flags, and
            // adding -pthread is refused by wasm-ld unless the Rust objects were
            // compiled with +atomics,+bulk-memory (which std needs build-std for).
            console.log('\n  The artifact declares a non-shared memory and the threads host offers a');
            console.log('  shared one. This is how the wasm_threads artifact is linked, not a fault in');
            console.log('  this harness: extension-ci-tools links wasm_threads exactly as wasm_eh.\n');
        } else if (/LinkError|invoke_/.test(loaded.message ?? '')) {
            console.log('\n  That is an emscripten ABI mismatch, not an extension bug: the artifact was');
            console.log('  linked by an emsdk whose WASM_BIGINT default disagrees with the one');
            console.log('  @duckdb/duckdb-wasm was built with. CI and the community release matrix pin');
            console.log('  emsdk 3.1.71 — build with that (`emcc -v`) before reading this as a code failure.\n');
        }
        return report.failures;
    }
    check('duckdb_extensions() reports it loaded',
        await rows("SELECT loaded FROM duckdb_extensions() WHERE extension_name = 'laterite_ags4'"),
        [{ loaded: true }]);

    // --- 3. it registered its whole surface ---------------------------------
    // A LOAD that succeeds and registers nothing would pass a smoke test. Hold the
    // wasm artifact to the same manifest the native build is held to.
    const manifest = JSON.parse(fs.readFileSync(path.join(REPO, 'functions.json'), 'utf8'));
    const expected = manifest.functions.map((f) => f.name).sort();
    const registered = (await rows(
        `SELECT DISTINCT function_name FROM duckdb_functions() WHERE function_name IN (${expected.map((n) => `'${n}'`).join(', ')})`,
    )).map((r) => r.function_name).sort();
    check(`all ${expected.length} functions.json functions are registered`, registered, expected);

    // --- 4. read_ags_text: born-typed VALUES and COLUMN TYPES ---------------
    // #16's third acceptance box: "SELECT ... FROM read_ags_text(?, 'LOCA') returns
    // born-typed rows in the browser."
    const AGS = FIXTURE_BYTES().toString('utf8');
    const sqlText = (group) => `read_ags_text('${AGS.replace(/'/g, "''")}', '${group}')`;

    check('read_ags_text LOCA schema (born-typed from the file\'s own TYPE row)',
        (await rows(`DESCRIBE SELECT * FROM ${sqlText('LOCA')}`))
            .map((r) => [r.column_name, r.column_type]),
        [
            ['_id', 'VARCHAR'],
            ['_parent_id', 'VARCHAR'],
            ['LOCA_ID', 'VARCHAR'],
            ['LOCA_TYPE', 'VARCHAR'],
            ['LOCA_GL', 'DOUBLE'],
            ['_content_hash', 'VARCHAR'],
        ]);

    check('read_ags_text LOCA values ("100.50" arrives as the double 100.5)',
        await rows(`SELECT loca_id, loca_type, loca_gl FROM ${sqlText('LOCA')} ORDER BY loca_id`),
        [
            { LOCA_ID: 'BH01', LOCA_TYPE: 'CP', LOCA_GL: 100.5 },
            { LOCA_ID: 'BH02', LOCA_TYPE: 'TP', LOCA_GL: 98 },
        ]);

    // Deterministic content-addressed keys: every SAMP row joins to its LOCA across
    // two independent reads, with no shared state — the property the whole reader
    // is built on, measured in wasm.
    check('deterministic keys join SAMP to LOCA across two independent reads',
        await rows(`SELECT count(*) AS n FROM ${sqlText('SAMP')} s JOIN ${sqlText('LOCA')} l ON s._parent_id = l._id`),
        [{ n: 3 }]);

    // Cross-surface parity. `test/sql/laterite_ags4.test` asserts these same two
    // literals natively and calls them "byte-identical to the wheel / Node /
    // browser (one shared keychain leaf)". Asserting the identical constants here
    // is what makes it a measured property of the wasm build rather than a stated one.
    check('_content_hash is byte-identical to the native/wheel output (cross-surface parity)',
        await rows(`SELECT loca_id, _content_hash FROM ${sqlText('LOCA')} ORDER BY loca_id`),
        [
            { LOCA_ID: 'BH01', _content_hash: '22dd55fd-2ac8-8d07-8220-25cd62ce47fd' },
            { LOCA_ID: 'BH02', _content_hash: '089b7a54-d873-88e2-842d-3bbb17ded1c8' },
        ]);

    // --- 5. read_ags: the path reader, through the host's filesystem --------
    // #16's fourth box ("decide path/remote viability"). `read_ags` goes through
    // DuckDB's VFS, which in wasm is whatever the host runtime provides. A
    // registered buffer is what a browser page actually does, so every harness
    // asserts it; each harness adds the sources its host can reach.
    const REGISTERED = await registerBuffer('registered.ags', new Uint8Array(FIXTURE_BYTES()));
    const sameRows = [{ LOCA_ID: 'BH01', LOCA_GL: 100.5 }, { LOCA_ID: 'BH02', LOCA_GL: 98 }];
    for (const { what, path: p } of [{ what: 'a host-registered buffer (the browser path)', path: REGISTERED }, ...pathReaders]) {
        const via = await attempt(`SELECT loca_id, loca_gl FROM read_ags('${p}', 'LOCA') ORDER BY loca_id`);
        if (checkThat(`read_ags reads ${what}`, via.ok, via.message)) {
            check('  …and returns the same born-typed rows', via.rows, sameRows);
        }
    }

    // --- 6. the metadata surface --------------------------------------------
    check('ags_groups reads the file\'s structure',
        await rows(`SELECT "group", n_rows FROM ags_groups('${REGISTERED}') ORDER BY "group"`),
        [{ group: 'LOCA', n_rows: 2n }, { group: 'PROJ', n_rows: 1n }, { group: 'SAMP', n_rows: 3n }]);

    checkThat('ags_headings returns the per-heading schema',
        (await rows(`SELECT count(*) AS n FROM ags_headings('${REGISTERED}')`))[0].n > 0n);

    // The embedded dictionary, relationships and rule catalogue need no filesystem
    // at all — if they came back empty the artifact shipped without its data.
    for (const [fn, floor] of [['ags_dictionary()', 100n], ['ags_relationships()', 10n], ['ags_rules()', 10n]]) {
        const n = (await rows(`SELECT count(*) AS n FROM ${fn}`))[0].n;
        checkThat(`${fn} carries its embedded data (${n} rows)`, n >= floor, `only ${n} rows, expected at least ${floor}`);
    }

    checkThat('load_ags emits CREATE TABLE DDL',
        (await rows(`SELECT stmt FROM load_ags('${REGISTERED}') ORDER BY seq`)).some((r) => /CREATE TABLE/i.test(r.stmt)));

    // --- 7. failure is a diagnostic, not a coin-flip ------------------------
    // Step 4 of #29: a bad call must raise, and must leave the connection usable —
    // a wasm artifact that takes the whole database down on first bad bind is not
    // shippable. Where the host can carry an error message at all, it must name
    // what went wrong.
    const badGroup = await attempt(`SELECT * FROM ${sqlText('NOSUCHGROUP')}`);
    checkThat('an unknown group raises rather than returning rows', !badGroup.ok,
        badGroup.ok ? `returned ${badGroup.rows.length} rows` : undefined);
    if (hostReportsErrors) {
        checkThat('  …with a message that names the group', /NOSUCHGROUP/i.test(badGroup.message ?? ''),
            `message was: ${badGroup.message}`);
    } else {
        console.log('  --   error-message content not asserted: this host cannot carry DuckDB errors');
    }
    check('the connection still works after a failed call',
        await rows(`SELECT count(*) AS n FROM ${sqlText('LOCA')}`), [{ n: 2n }]);

    return report.failures;
}
