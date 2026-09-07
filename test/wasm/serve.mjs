// Static file server for the wasm functional gate — a SEPARATE PROCESS on purpose.
//
// duckdb-wasm fetches a loadable extension through an emscripten JS shim that
// makes an asynchronous `fetch` look synchronous to the C++ caller: it spawns a
// worker to do the fetch and then parks the calling thread on `Atomics.wait`
// until the worker signals. `LOAD` therefore blocks the whole JS thread that
// issued it. A server hosted on that same thread can never accept the
// connection, so the fetch never completes and the two sides deadlock — the
// symptom is a `LOAD` that hangs forever with the server logging no request.
//
// Hosting the repository in a child process keeps its event loop free while the
// gate's thread is parked. Prints `PORT=<n>` on stdout once listening; the
// parent reads that and builds `custom_extension_repository` from it.
//
// Usage: node serve.mjs <root-dir>

import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';

const root = path.resolve(process.argv[2] ?? '.');

const server = http.createServer((req, res) => {
    // Confine every request to `root` — a repository URL is attacker-free here,
    // but a traversal would silently serve the wrong file and pass the gate.
    const rel = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
    const file = path.resolve(root, '.' + rel);
    if (file !== root && !file.startsWith(root + path.sep)) {
        res.statusCode = 403;
        res.end('forbidden');
        return;
    }
    fs.readFile(file, (err, buf) => {
        if (err) {
            process.stderr.write(`serve: 404 ${rel}\n`);
            res.statusCode = 404;
            res.end('not found');
            return;
        }
        res.setHeader('content-type', 'application/wasm');
        res.setHeader('content-length', String(buf.length));
        res.end(buf);
    });
});

server.listen(0, '127.0.0.1', () => {
    process.stdout.write(`PORT=${server.address().port}\n`);
});
