// The wasm FUNCTIONAL gate — the browser-side counterpart of `make test_debug`.
//
// `make test_debug` LOADs the native artifact into a real DuckDB and runs real
// SQL; until this file existed the wasm side was only ever compiled, never run,
// so an artifact that registered zero functions or panicked on first bind would
// have shipped with every check green (issue #29). This loads the actual
// `.duckdb_extension.wasm` into `@duckdb/duckdb-wasm` and asserts the returned
// VALUES AND COLUMN TYPES — a `LOAD` that succeeds and registers nothing fails
// here, which is the whole point.
//
// Usage: node gate.mjs [wasm_mvp|wasm_eh]

import { createRequire } from 'node:module';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const require = createRequire(import.meta.url);
const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, '..', '..');

const PLATFORM = process.argv[2] ?? 'wasm_mvp';
// duckdb-wasm ships one prebuilt runtime per wasm feature level. `wasm_threads`
// maps to the `coi` runtime, which needs SharedArrayBuffer + a pthread worker
// and has no node-blocking bundle — it is built and shipped, but not gated here.
const BUNDLE = { wasm_mvp: 'mvp', wasm_eh: 'eh' }[PLATFORM];
if (!BUNDLE) {
    console.error(`gate: unsupported platform '${PLATFORM}' (expected wasm_mvp or wasm_eh)`);
    process.exit(2);
}

// --- the version-alignment invariant ------------------------------------
// The extension is built with USE_UNSTABLE_C_API=1: it compiles against one
// exact C-API struct layout, and is stamped for that DuckDB version. The
// @duckdb/duckdb-wasm pin in package.json must therefore embed the same DuckDB
// as TARGET_DUCKDB_VERSION. Read the Makefile rather than restating the version
// here — one source, no drift.
const makefile = fs.readFileSync(path.join(REPO, 'Makefile'), 'utf8');
const TARGET_DUCKDB_VERSION = /^TARGET_DUCKDB_VERSION=(\S+)$/m.exec(makefile)?.[1];
if (!TARGET_DUCKDB_VERSION) {
    console.error('gate: could not read TARGET_DUCKDB_VERSION from the Makefile');
    process.exit(2);
}

const ARTIFACT = path.join(
    REPO, 'build', PLATFORM, 'extension', 'laterite_ags4', 'laterite_ags4.duckdb_extension.wasm',
);
if (!fs.existsSync(ARTIFACT)) {
    console.error(`gate: no artifact at ${path.relative(REPO, ARTIFACT)} — run \`make ${PLATFORM}\` first`);
    process.exit(2);
}

// --- assertion plumbing --------------------------------------------------
let failures = 0;
const show = (v) => JSON.stringify(v, (_k, x) => (typeof x === 'bigint' ? Number(x) : x));

function check(what, actual, expected) {
    const ok = show(actual) === show(expected);
    console.log(`${ok ? '  ok  ' : ' FAIL '} ${what}`);
    if (!ok) {
        failures++;
        console.log(`         expected: ${show(expected)}`);
        console.log(`         actual:   ${show(actual)}`);
    }
    return ok;
}

function checkThat(what, ok, detail) {
    console.log(`${ok ? '  ok  ' : ' FAIL '} ${what}`);
    if (!ok) {
        failures++;
        if (detail !== undefined) console.log(`         ${detail}`);
    }
    return ok;
}

// --- a hermetic HOME -----------------------------------------------------
// duckdb-wasm's loader caches every extension it fetches under
// `${os.homedir()}/.duckdb/extensions/<host:port>/<version>/<platform>/` and
// reuses the file if it is already there — a stale cache would quietly gate a
// PREVIOUS build. Point HOME at a fresh directory so each run genuinely fetches
// the artifact it just built (and leaves the developer's ~/.duckdb alone).
const SANDBOX = fs.mkdtempSync(path.join(os.tmpdir(), 'laterite-wasm-gate-'));
process.env.HOME = SANDBOX;
process.env.USERPROFILE = SANDBOX;

// Stage the artifact in DuckDB's extension-repository layout:
//   <repo>/<duckdb version>/<platform>/<name>.duckdb_extension.wasm
const REPO_ROOT = path.join(SANDBOX, 'extension-repository');
const served = path.join(REPO_ROOT, TARGET_DUCKDB_VERSION, PLATFORM);
fs.mkdirSync(served, { recursive: true });
fs.copyFileSync(ARTIFACT, path.join(served, 'laterite_ags4.duckdb_extension.wasm'));

const server = spawn(process.execPath, [path.join(HERE, 'serve.mjs'), REPO_ROOT],
    { stdio: ['ignore', 'pipe', 'inherit'] });
const port = await new Promise((resolve, reject) => {
    server.stdout.on('data', (d) => {
        const m = /PORT=(\d+)/.exec(String(d));
        if (m) resolve(Number(m[1]));
    });
    server.on('exit', (code) => reject(new Error(`extension repository server exited with ${code}`)));
});

// Registered as an exit hook rather than called at each return: an unexpected
// throw would otherwise orphan the server process and leave the sandbox behind.
let cleaned = false;
function cleanup() {
    if (cleaned) return;
    cleaned = true;
    server.kill();
    fs.rmSync(SANDBOX, { recursive: true, force: true });
}
process.on('exit', cleanup);

// --- boot duckdb-wasm ----------------------------------------------------
const duckdb = require('@duckdb/duckdb-wasm/dist/duckdb-node-blocking.cjs');
const DIST = path.dirname(require.resolve('@duckdb/duckdb-wasm/dist/duckdb-node-blocking.cjs'));

const db = await duckdb.createDuckDB(
    { [BUNDLE]: { mainModule: path.join(DIST, `duckdb-${BUNDLE}.wasm`), mainWorker: null } },
    new duckdb.VoidLogger(),
    duckdb.NODE_RUNTIME,
);
await db.instantiate(() => {});
// The artifact under test is unsigned (community-extensions signs at publish),
// so the host has to be told to accept it — the same flag a page needs to load
// an extension from its own CDN.
db.open({ path: ':memory:', allowUnsignedExtensions: true });
const conn = db.connect();

const rows = (sql) => conn.query(sql).toArray().map((r) => r.toJSON());
const attempt = (sql) => {
    try {
        return { ok: true, rows: rows(sql) };
    } catch (e) {
        return { ok: false, message: String(e?.message ?? e) };
    }
};

console.log(`\nlaterite_ags4 wasm gate — ${PLATFORM} (duckdb-wasm ${duckdb.PACKAGE_VERSION})\n`);

// --- 1. host/artifact version alignment ---------------------------------
// This is the go/no-go, and it has to be asserted rather than relied upon: the
// wasm loader does NOT enforce the stamped duckdb_version — an artifact
// footered for the wrong release still loads — so a drifted pin would surface
// as ABI-level misbehaviour somewhere below, or as nothing at all, rather than
// as a refused load. Name the cause here while it is still nameable.
const host = rows('SELECT version() AS version, (SELECT platform FROM pragma_platform()) AS platform')[0];
if (!check(`host DuckDB is ${TARGET_DUCKDB_VERSION} (Makefile TARGET_DUCKDB_VERSION)`,
    host.version, TARGET_DUCKDB_VERSION)) {
    console.log(`\n  The @duckdb/duckdb-wasm pin in test/wasm/package.json embeds DuckDB ${host.version},`);
    console.log(`  but this extension is stamped for ${TARGET_DUCKDB_VERSION} and built against that exact`);
    console.log('  C API (USE_UNSTABLE_C_API=1). Re-pin the npm version to the release that carries');
    console.log(`  DuckDB ${TARGET_DUCKDB_VERSION}, or move TARGET_DUCKDB_VERSION — nothing below can pass first.\n`);
    process.exit(1);
}
check(`host platform is ${PLATFORM}`, host.platform, PLATFORM);

// Does this host report DuckDB's own errors intelligibly? duckdb-wasm's
// `wasm_mvp` runtime loses every C++ exception to a JS `_setThrew is not
// defined` — including for plain `SELECT * FROM no_such_table`, with no
// extension loaded at all — while `wasm_eh` reports them verbatim. Probe it
// rather than hard-coding which variant is which, so the error-message
// assertion at the end tightens on its own once the host is fixed.
const hostErrors = attempt('SELECT * FROM a_table_that_does_not_exist');
const hostReportsErrors = !hostErrors.ok && /Catalog Error/i.test(hostErrors.message);
console.log(`  --   host error reporting: ${hostReportsErrors ? 'intelligible' : `degraded (${hostErrors.message?.slice(0, 60)})`}`);

// --- 2. LOAD the artifact -----------------------------------------------
const load = attempt(`SET custom_extension_repository='http://127.0.0.1:${port}'`);
checkThat('SET custom_extension_repository', load.ok, load.message);
const loaded = attempt('LOAD laterite_ags4');
if (!checkThat('LOAD laterite_ags4 (fetched from the local extension repository)', loaded.ok, loaded.message)) {
    // The one failure here that is about the toolchain rather than the code, and
    // it is unreadable without this: an emsdk newer than the pinned 3.1.71
    // defaults WASM_BIGINT ON, so the side module imports `invoke_*` with real
    // i64 parameters while duckdb-wasm's main module offers the legalized
    // i32-pair form. CI pins the right emsdk; a local build may not.
    if (/LinkError|invoke_/.test(loaded.message ?? '')) {
        console.log('\n  That is an emscripten ABI mismatch, not an extension bug: the artifact was');
        console.log('  linked by an emsdk whose WASM_BIGINT default disagrees with the one');
        console.log('  @duckdb/duckdb-wasm was built with. CI and the community release matrix pin');
        console.log('  emsdk 3.1.71 — build with that (`emcc -v`) before reading this as a code failure.\n');
    }
    process.exit(1);
}
check('duckdb_extensions() reports it loaded',
    rows("SELECT loaded FROM duckdb_extensions() WHERE extension_name = 'laterite_ags4'"),
    [{ loaded: true }]);

// --- 3. it registered its whole surface ---------------------------------
// A LOAD that succeeds and registers nothing would pass a smoke test. Hold the
// wasm artifact to the same manifest the native build is held to.
const manifest = JSON.parse(fs.readFileSync(path.join(REPO, 'functions.json'), 'utf8'));
const expected = manifest.functions.map((f) => f.name).sort();
const registered = rows(
    `SELECT DISTINCT function_name FROM duckdb_functions() WHERE function_name IN (${expected.map((n) => `'${n}'`).join(', ')})`,
).map((r) => r.function_name).sort();
check(`all ${expected.length} functions.json functions are registered`, registered, expected);

// --- 4. read_ags_text: born-typed VALUES and COLUMN TYPES ---------------
// #16's third acceptance box: "SELECT ... FROM read_ags_text(?, 'LOCA') returns
// born-typed rows in the browser."
const AGS = fs.readFileSync(path.join(REPO, 'test', 'sql', 'mini.ags'), 'utf8');
const sqlText = (group) => `read_ags_text('${AGS.replace(/'/g, "''")}', '${group}')`;

check('read_ags_text LOCA schema (born-typed from the file\'s own TYPE row)',
    rows(`DESCRIBE SELECT * FROM ${sqlText('LOCA')}`)
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
    rows(`SELECT loca_id, loca_type, loca_gl FROM ${sqlText('LOCA')} ORDER BY loca_id`),
    [
        { LOCA_ID: 'BH01', LOCA_TYPE: 'CP', LOCA_GL: 100.5 },
        { LOCA_ID: 'BH02', LOCA_TYPE: 'TP', LOCA_GL: 98 },
    ]);

// Deterministic content-addressed keys: every SAMP row joins to its LOCA across
// two independent reads, with no shared state — the property the whole reader
// is built on, measured in wasm.
check('deterministic keys join SAMP to LOCA across two independent reads',
    rows(`SELECT count(*) AS n FROM ${sqlText('SAMP')} s JOIN ${sqlText('LOCA')} l ON s._parent_id = l._id`),
    [{ n: 3 }]);

// Cross-surface parity. `test/sql/laterite_ags4.test` asserts these same two
// literals natively and calls them "byte-identical to the wheel / Node /
// browser (one shared keychain leaf)". Until now that claim stopped at the
// native surface; asserting the identical constants here is what makes it a
// measured property of the browser build rather than a stated one.
check('_content_hash is byte-identical to the native/wheel output (cross-surface parity)',
    rows(`SELECT loca_id, _content_hash FROM ${sqlText('LOCA')} ORDER BY loca_id`),
    [
        { LOCA_ID: 'BH01', _content_hash: '22dd55fd-2ac8-8d07-8220-25cd62ce47fd' },
        { LOCA_ID: 'BH02', _content_hash: '089b7a54-d873-88e2-842d-3bbb17ded1c8' },
    ]);

// --- 5. read_ags: the path reader, through the host's filesystem --------
// #16's fourth box ("decide path/remote viability"). `read_ags` goes through
// DuckDB's VFS, which in wasm is whatever the host runtime provides — a
// registered buffer in the browser, the real filesystem under node. Both are
// exercised: the buffer is what a browser page actually does.
//
// Both names live under the sandbox, and the fixture is read from a COPY, for a
// reason worth knowing: `read_ags` probes for a `<path>.ags.idx` certificate
// beside its input, and duckdb-wasm's node runtime opens files O_CREAT — so the
// probe MAKES a zero-byte sidecar wherever the input lives. Pointed at
// `test/sql/mini.ags` it would drop one next to the sqllogictest fixtures on
// every run.
const FIXTURE = path.join(SANDBOX, 'mini.ags');
fs.copyFileSync(path.join(REPO, 'test', 'sql', 'mini.ags'), FIXTURE);
const REGISTERED = path.join(SANDBOX, 'registered.ags');
db.registerFileBuffer(REGISTERED, new Uint8Array(fs.readFileSync(FIXTURE)));
const viaBuffer = attempt(`SELECT loca_id, loca_gl FROM read_ags('${REGISTERED}', 'LOCA') ORDER BY loca_id`);
if (checkThat('read_ags reads a host-registered buffer (the browser path)', viaBuffer.ok, viaBuffer.message)) {
    check('  …and returns the same born-typed rows', viaBuffer.rows,
        [{ LOCA_ID: 'BH01', LOCA_GL: 100.5 }, { LOCA_ID: 'BH02', LOCA_GL: 98 }]);
}

const viaFs = attempt(`SELECT loca_id, loca_gl FROM read_ags('${FIXTURE}', 'LOCA') ORDER BY loca_id`);
if (checkThat('read_ags reads a host filesystem path (the node path)', viaFs.ok, viaFs.message)) {
    check('  …and returns the same born-typed rows', viaFs.rows,
        [{ LOCA_ID: 'BH01', LOCA_GL: 100.5 }, { LOCA_ID: 'BH02', LOCA_GL: 98 }]);
}

// --- 6. the metadata surface --------------------------------------------
check('ags_groups reads the file\'s structure',
    rows(`SELECT "group", n_rows FROM ags_groups('${REGISTERED}') ORDER BY "group"`),
    [{ group: 'LOCA', n_rows: 2n }, { group: 'PROJ', n_rows: 1n }, { group: 'SAMP', n_rows: 3n }]);

checkThat('ags_headings returns the per-heading schema',
    rows(`SELECT count(*) AS n FROM ags_headings('${REGISTERED}')`)[0].n > 0n);

// The embedded dictionary, relationships and rule catalogue need no filesystem
// at all — if they came back empty the artifact shipped without its data.
for (const [fn, floor] of [['ags_dictionary()', 100n], ['ags_relationships()', 10n], ['ags_rules()', 10n]]) {
    const n = rows(`SELECT count(*) AS n FROM ${fn}`)[0].n;
    checkThat(`${fn} carries its embedded data (${n} rows)`, n >= floor, `only ${n} rows, expected at least ${floor}`);
}

checkThat('load_ags emits CREATE TABLE DDL',
    rows(`SELECT stmt FROM load_ags('${REGISTERED}') ORDER BY seq`).some((r) => /CREATE TABLE/i.test(r.stmt)));

// --- 7. failure is a diagnostic, not a coin-flip ------------------------
// Step 4 of #29: a bad call must raise, and must leave the connection usable —
// a wasm artifact that takes the whole database down on first bad bind is not
// shippable. Where the host can carry an error message at all, it must name
// what went wrong.
const badGroup = attempt(`SELECT * FROM ${sqlText('NOSUCHGROUP')}`);
checkThat('an unknown group raises rather than returning rows', !badGroup.ok,
    badGroup.ok ? `returned ${badGroup.rows.length} rows` : undefined);
if (hostReportsErrors) {
    checkThat('  …with a message that names the group', /NOSUCHGROUP/i.test(badGroup.message ?? ''),
        `message was: ${badGroup.message}`);
} else {
    console.log('  --   error-message content not asserted: this host cannot carry DuckDB errors');
}
check('the connection still works after a failed call',
    rows(`SELECT count(*) AS n FROM ${sqlText('LOCA')}`), [{ n: 2n }]);

// --- done ----------------------------------------------------------------
conn.close();

console.log(`\n${failures === 0 ? 'PASS' : `FAIL — ${failures} check(s) failed`}: laterite_ags4 on ${PLATFORM}\n`);
process.exit(failures === 0 ? 0 : 1);
