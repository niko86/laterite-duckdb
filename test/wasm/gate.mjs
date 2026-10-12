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
import { REPO, FIXTURE_BYTES, artifactPath, runChecks, targetDuckdbVersion } from './checks.mjs';

const require = createRequire(import.meta.url);
const HERE = path.dirname(fileURLToPath(import.meta.url));

const PLATFORM = process.argv[2] ?? 'wasm_mvp';
// duckdb-wasm ships one prebuilt runtime per wasm feature level. `wasm_threads`
// maps to the `coi` runtime, which needs SharedArrayBuffer + a pthread worker
// and has no node-blocking bundle. It is not published (#50); browser-gate.mjs
// keeps a threads mode for when a loadable build exists.
const BUNDLE = { wasm_mvp: 'mvp', wasm_eh: 'eh' }[PLATFORM];
if (!BUNDLE) {
    console.error(`gate: unsupported platform '${PLATFORM}' (expected wasm_mvp or wasm_eh)`);
    process.exit(2);
}

// The version-alignment invariant (the npm pin must embed TARGET_DUCKDB_VERSION)
// and every assertion live in checks.mjs, shared with browser-gate.mjs.
const TARGET_DUCKDB_VERSION = targetDuckdbVersion();
const ARTIFACT = artifactPath(PLATFORM);
if (!fs.existsSync(ARTIFACT)) {
    console.error(`gate: no artifact at ${path.relative(REPO, ARTIFACT)} — run \`make ${PLATFORM}\` first`);
    process.exit(2);
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

const attempt = async (sql) => {
    try {
        return { ok: true, rows: conn.query(sql).toArray().map((r) => r.toJSON()) };
    } catch (e) {
        return { ok: false, message: String(e?.message ?? e) };
    }
};

console.log(`\nlaterite_ags4 wasm gate — ${PLATFORM} (duckdb-wasm ${duckdb.PACKAGE_VERSION})\n`);

// Both names live under the sandbox, and the fixture is read from a COPY, for a
// reason worth knowing: `read_ags` probes for a `<path>.ags.idx` certificate
// beside its input, and duckdb-wasm's node runtime opens files O_CREAT — so the
// probe MAKES a zero-byte sidecar wherever the input lives. Pointed at
// `test/sql/mini.ags` it would drop one next to the sqllogictest fixtures on
// every run.
const FIXTURE = path.join(SANDBOX, 'mini.ags');
fs.writeFileSync(FIXTURE, FIXTURE_BYTES());

const failures = await runChecks({
    platform: PLATFORM,
    attempt,
    registerBuffer: async (name, bytes) => {
        const p = path.join(SANDBOX, name);
        db.registerFileBuffer(p, bytes);
        return p;
    },
    pathReaders: [{ what: 'a host filesystem path (the node path)', path: FIXTURE }],
    repositoryUrl: `http://127.0.0.1:${port}`,
});

conn.close();

console.log(`\n${failures === 0 ? 'PASS' : `FAIL — ${failures} check(s) failed`}: laterite_ags4 on ${PLATFORM}\n`);
process.exit(failures === 0 ? 0 : 1);
