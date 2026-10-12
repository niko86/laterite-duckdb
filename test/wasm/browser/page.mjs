// The in-page half of browser-gate.mjs: boots a duckdb-wasm runtime (`coi` for
// wasm_threads) and exposes a query runner on `window.gate`. It holds no
// assertions — those run in node, from checks.mjs, so this page can only be a
// transport.
//
// Bundled by esbuild when the gate starts: duckdb-browser.mjs imports
// `apache-arrow` by bare name, which a browser cannot resolve on its own.

import * as duckdb from '@duckdb/duckdb-wasm';

// Arrow hands back BigInt for 64-bit integers, which cannot cross
// `page.evaluate`. Tag them so node revives the same values the node-blocking
// gate sees.
const encode = (v) => JSON.stringify(v, (_k, x) => (typeof x === 'bigint' ? { $bigint: String(x) } : x));

let db;
let conn;

window.gate = {
    isolation: () => ({
        crossOriginIsolated: self.crossOriginIsolated === true,
        sharedArrayBuffer: typeof SharedArrayBuffer === 'function',
    }),

    async boot(bundle) {
        // The bundle named explicitly rather than chosen by selectBundle():
        // selectBundle falls back from coi to eh/mvp when isolation is missing,
        // which is exactly the degraded pass this gate exists to refuse.
        const dist = `${location.origin}/dist/`;
        const worker = new Worker(`${dist}duckdb-browser-${bundle}.worker.js`);
        db = new duckdb.AsyncDuckDB(new duckdb.ConsoleLogger(duckdb.LogLevel.WARNING), worker);
        await db.instantiate(`${dist}duckdb-${bundle}.wasm`,
            bundle === 'coi' ? `${dist}duckdb-browser-coi.pthread.worker.js` : null);
        // Unsigned artifact under test: the same flag a page loading an extension
        // from its own CDN needs.
        await db.open({ path: ':memory:', allowUnsignedExtensions: true });
        conn = await db.connect();
        return { packageVersion: duckdb.PACKAGE_VERSION };
    },

    async attempt(sql) {
        try {
            const table = await conn.query(sql);
            return { ok: true, rows: encode(table.toArray().map((r) => r.toJSON())) };
        } catch (e) {
            return { ok: false, message: String(e?.message ?? e) };
        }
    },

    async registerBuffer(name, bytes) {
        await db.registerFileBuffer(name, new Uint8Array(bytes));
    },

    async registerURL(name, url) {
        await db.registerFileURL(name, url, duckdb.DuckDBDataProtocol.HTTP, false);
    },
};
