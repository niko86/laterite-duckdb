// The wasm functional gate for `wasm_threads` — a real browser (#50).
//
// `wasm_threads` maps to duckdb-wasm's `coi` runtime: SharedArrayBuffer plus a
// pthread worker, browser-only (there is no node bundle for it), so gate.mjs
// cannot reach it. This serves a page with the cross-origin-isolation headers,
// drives it with Playwright's Chromium, LOADs the artifact from a same-origin
// extension repository and runs checks.mjs — the SAME assertions the node gate
// runs for wasm_mvp and wasm_eh.
//
// A positive control runs first and is fatal: the page must be cross-origin
// isolated and the coi bundle must run plain SQL as `wasm_threads` before the
// extension is touched. Without isolation the coi runtime cannot start, and a
// harness that let duckdb-wasm fall back to eh/mvp would pass while gating
// nothing.
//
// Usage: node browser-gate.mjs [wasm_threads|wasm_eh|wasm_mvp] [--control-only] [--no-coi] [--artifact <file>]
//   wasm_threads is the default and the point; the other two run the same page on
//   their own bundles, which is how the transport was checked against artifacts
//   that load.
//   --control-only  stop after the positive control (no artifact needed)
//   --no-coi        serve WITHOUT the COOP/COEP headers (fail-first: must go red)
//   --artifact      LOAD this file instead of build/wasm_threads/... (fail-first)

import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';
import { chromium } from 'playwright';
import { REPO, FIXTURE_BYTES, artifactPath, makeReport, runChecks, targetDuckdbVersion } from './checks.mjs';

const require = createRequire(import.meta.url);
const HERE = path.dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const PLATFORM = args.find((a) => a.startsWith('wasm_')) ?? 'wasm_threads';
const BUNDLE = { wasm_threads: 'coi', wasm_eh: 'eh', wasm_mvp: 'mvp' }[PLATFORM];
if (!BUNDLE) {
    console.error(`browser-gate: unsupported platform '${PLATFORM}'`);
    process.exit(2);
}
const flag = (name) => args.includes(name);
const CONTROL_ONLY = flag('--control-only');
const COI = !flag('--no-coi');
const ARTIFACT = args.includes('--artifact') ? path.resolve(args[args.indexOf('--artifact') + 1]) : artifactPath(PLATFORM);
if (!CONTROL_ONLY && !fs.existsSync(ARTIFACT)) {
    console.error(`browser-gate: no artifact at ${path.relative(REPO, ARTIFACT)} — run \`make ${PLATFORM}\` first`);
    process.exit(2);
}

// A hung LOAD (the classic shape is a sync fetch nobody answers) would otherwise
// sit until the CI job timeout with nothing in the log.
const CALL_TIMEOUT_MS = 120_000;
const within = (what, p) => Promise.race([
    p,
    new Promise((_r, reject) => setTimeout(() => reject(new Error(`${what}: no answer in ${CALL_TIMEOUT_MS / 1000}s`)), CALL_TIMEOUT_MS).unref()),
]);

// --- the page and everything it fetches, from one origin ----------------
const DIST = path.dirname(require.resolve('@duckdb/duckdb-wasm/dist/duckdb-browser.mjs'));
const bundle = await build({
    entryPoints: [path.join(HERE, 'browser', 'page.mjs')],
    bundle: true, format: 'esm', platform: 'browser', write: false, logLevel: 'error',
});
const TARGET_DUCKDB_VERSION = targetDuckdbVersion();
const routes = new Map([
    ['/', { type: 'text/html', body: fs.readFileSync(path.join(HERE, 'browser', 'index.html')) }],
    ['/page.js', { type: 'text/javascript', body: Buffer.from(bundle.outputFiles[0].contents) }],
    ['/files/mini.ags', { type: 'text/plain', body: FIXTURE_BYTES() }],
]);
const distFiles = [`duckdb-${BUNDLE}.wasm`, `duckdb-browser-${BUNDLE}.worker.js`];
if (BUNDLE === 'coi') distFiles.push('duckdb-browser-coi.pthread.worker.js');
for (const f of distFiles) {
    routes.set(`/dist/${f}`, { type: f.endsWith('.wasm') ? 'application/wasm' : 'text/javascript', file: path.join(DIST, f) });
}
// DuckDB's extension-repository layout: <repo>/<duckdb version>/<platform>/<name>.duckdb_extension.wasm
routes.set(`/repo/${TARGET_DUCKDB_VERSION}/${PLATFORM}/laterite_ags4.duckdb_extension.wasm`,
    { type: 'application/wasm', file: ARTIFACT });

// In-process, unlike gate.mjs's serve.mjs: here LOAD's blocking fetch parks a
// duckdb-wasm worker inside Chromium, never this event loop.
const server = http.createServer((req, res) => {
    const route = routes.get(new URL(req.url, 'http://x').pathname);
    // Every artifact/fixture fetch is logged: a LOAD that hangs is otherwise
    // indistinguishable from one whose request never left the browser.
    if (/^\/(repo|files)\//.test(req.url)) {
        console.log(`  [serve] ${req.method} ${req.url}${req.headers.range ? ` ${req.headers.range}` : ''}${route ? '' : ' -> 404'}`);
    }
    if (COI) {
        res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
        res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
    }
    res.setHeader('Cross-Origin-Resource-Policy', 'same-origin');
    res.setHeader('Cache-Control', 'no-store');
    if (!route) {
        process.stderr.write(`browser-gate: 404 ${req.url}\n`);
        res.statusCode = 404;
        res.end('not found');
        return;
    }
    const body = route.body ?? fs.readFileSync(route.file);
    res.setHeader('Content-Type', route.type);
    res.setHeader('Accept-Ranges', 'bytes');
    // registerFileURL reads over HTTP with Range requests, as it would a real remote file.
    const range = /^bytes=(\d+)-(\d*)$/.exec(req.headers.range ?? '');
    if (range) {
        const start = Number(range[1]);
        const end = range[2] ? Math.min(Number(range[2]), body.length - 1) : body.length - 1;
        res.statusCode = 206;
        res.setHeader('Content-Range', `bytes ${start}-${end}/${body.length}`);
        res.setHeader('Content-Length', String(end - start + 1));
        res.end(req.method === 'HEAD' ? undefined : body.subarray(start, end + 1));
        return;
    }
    res.setHeader('Content-Length', String(body.length));
    res.end(req.method === 'HEAD' ? undefined : body);
});
await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
const ORIGIN = `http://127.0.0.1:${server.address().port}`;

const browser = await chromium.launch();
let exitCode = 1;
try {
    const page = await browser.newPage();
    page.on('console', (m) => { if (m.type() === 'error' || m.type() === 'warning') console.log(`  [page] ${m.text()}`); });
    // The coi runtime's pthreads are workers; an error there never reaches the
    // page console, only this.
    page.on('worker', (w) => w.on('console', (m) => { if (m.type() === 'error') console.log(`  [worker] ${m.text()}`); }));
    page.on('pageerror', (e) => console.log(`  [page] ${e.message}`));
    await page.goto(`${ORIGIN}/`);
    await page.waitForFunction(() => window.gate !== undefined);

    const report = makeReport();
    const { check, checkThat } = report;
    console.log(`\nlaterite_ags4 wasm gate — ${PLATFORM} (Chromium ${browser.version()}, ${BUNDLE} runtime)\n`);

    // --- positive control: isolation, then plain SQL on the coi bundle ----
    const iso = await page.evaluate(() => window.gate.isolation());
    const isolated = checkThat('page is cross-origin isolated (crossOriginIsolated === true)', iso.crossOriginIsolated,
        'served without COOP/COEP: no SharedArrayBuffer, so the coi runtime cannot run');
    checkThat('SharedArrayBuffer is available', iso.sharedArrayBuffer);
    if (!isolated || !iso.sharedArrayBuffer) {
        console.log('\n  The positive control failed, so nothing below would gate wasm_threads. Stopping.\n');
        throw new Error('not cross-origin isolated');
    }

    const boot = await within(`boot the ${BUNDLE} bundle`, page.evaluate((b) => window.gate.boot(b), BUNDLE))
        .then((v) => ({ ok: true, ...v }), (e) => ({ ok: false, message: String(e?.message ?? e) }));
    if (!checkThat(`the ${BUNDLE} bundle instantiates${BUNDLE === 'coi' ? ' (pthread worker + shared memory)' : ''}`, boot.ok, boot.message)) {
        throw new Error(`${BUNDLE} bundle did not boot`);
    }

    const attempt = async (sql) => {
        const r = await within(sql.slice(0, 60), page.evaluate((s) => window.gate.attempt(s), sql));
        return r.ok
            ? { ok: true, rows: JSON.parse(r.rows, (_k, x) => (x && typeof x === 'object' && '$bigint' in x ? BigInt(x.$bigint) : x)) }
            : r;
    };

    const plain = await attempt("SELECT 42 AS answer, (SELECT platform FROM pragma_platform()) AS platform");
    if (!checkThat('plain SQL runs with no extension loaded', plain.ok, plain.message)
        || !check(`  …on the ${PLATFORM} platform (no fallback to another bundle)`, plain.rows, [{ answer: 42, platform: PLATFORM }])) {
        throw new Error('positive control failed');
    }
    console.log(`  --   duckdb-wasm ${boot.packageVersion}, positive control passed\n`);

    if (CONTROL_ONLY) {
        console.log(`${report.failures === 0 ? 'PASS' : 'FAIL'}: positive control only (no extension loaded)\n`);
        exitCode = report.failures === 0 ? 0 : 1;
    } else {
        await page.evaluate(([name, url]) => window.gate.registerURL(name, url), ['remote.ags', `${ORIGIN}/files/mini.ags`]);
        const failures = await runChecks({
            platform: PLATFORM,
            attempt,
            registerBuffer: async (name, bytes) => {
                await page.evaluate(([n, b]) => window.gate.registerBuffer(n, b), [name, Array.from(bytes)]);
                return name;
            },
            pathReaders: [{ what: 'an http URL via registerFileURL (how a page reaches a remote file)', path: 'remote.ags' }],
            repositoryUrl: `${ORIGIN}/repo`,
            report,
        });
        console.log(`\n${failures === 0 ? 'PASS' : `FAIL — ${failures} check(s) failed`}: laterite_ags4 on ${PLATFORM}\n`);
        exitCode = failures === 0 ? 0 : 1;
    }
} catch (e) {
    console.log(`\nFAIL: ${e.message}\n`);
    exitCode = 1;
} finally {
    await browser.close();
    server.close();
}
process.exit(exitCode);
