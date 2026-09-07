# The wasm functional gate

`make test_debug` LOADs the **native** artifact into a real DuckDB and runs real
SQL. Until this directory existed, the wasm side had no equivalent: `.github/`
built no wasm variant at all, and the community-extensions matrix that does
build one never loads it — so "it compiles for wasm" was checked while "it works
in wasm" was not ([#29](https://github.com/niko86/laterite-duckdb/issues/29)).
An artifact that registered zero functions, or panicked on first bind, would
have shipped with every check green.

`gate.mjs` closes that. It LOADs the `.duckdb_extension.wasm` this repo just
built into `@duckdb/duckdb-wasm` in headless node and asserts the **values and
column types** that come back — born typing, the `_parent_id`/`_id` join,
`_content_hash` parity with the native output, the path reader through the
host's filesystem, and the embedded dictionary/relationships/rules.

```sh
make test_wasm            # both variants, building each first
make test_wasm_mvp        # just wasm_mvp
node test/wasm/gate.mjs wasm_eh    # re-run against an already-built artifact
```

## The version pin is load-bearing

The extension is built with `USE_UNSTABLE_C_API=1`: it compiles against one
exact DuckDB C-API revision, and its metadata footer is stamped for that same
version (`TARGET_DUCKDB_VERSION` in the Makefile). The `@duckdb/duckdb-wasm`
release pinned in `package.json` must therefore embed **that** DuckDB —
`@latest` is frequently a release or two behind and will not do.

`gate.mjs` asserts the alignment before anything else and stops with an
explanatory message if it drifts, because every failure below it would otherwise
be an unreadable consequence of the same cause.

To re-pin, find the release that carries the DuckDB you target — npm's version
numbers don't encode it, so ask the artifact:

```sh
npm view @duckdb/duckdb-wasm versions --json | tail -20     # recent candidates
npm install --no-save @duckdb/duckdb-wasm@<candidate>
node --input-type=module -e '
import { createRequire } from "node:module"; import path from "node:path";
const require = createRequire(import.meta.url);
const d = require("@duckdb/duckdb-wasm/dist/duckdb-node-blocking.cjs");
const DIST = path.dirname(require.resolve("@duckdb/duckdb-wasm/dist/duckdb-node-blocking.cjs"));
const db = await d.createDuckDB({ eh: { mainModule: path.join(DIST, "duckdb-eh.wasm"), mainWorker: null } },
                                new d.VoidLogger(), d.NODE_RUNTIME);
await db.instantiate(() => {});
console.log(d.PACKAGE_VERSION, "=>", db.connect().query("SELECT version()").toArray()[0].toJSON());'
```

At the time of writing, `1.33.1-dev64.0` (npm's `next`) carries DuckDB v1.5.5
while `latest` (`1.33.1-dev57.0`) still carries v1.5.4 — which is why the pin is
exact and deliberately not tracked by Dependabot. Moving `TARGET_DUCKDB_VERSION`
means moving this pin in the same change.

Note that the wasm loader does **not** enforce the stamped `duckdb_version`: an
artifact footered for the wrong release still loads. The mismatch would surface
as an ABI-level misbehaviour at call time, not as a refused load — which is
precisely why this gate asserts returned values rather than a successful `LOAD`.

## The emsdk version matters too

CI and the community release matrix both link with **emsdk 3.1.71** (that is the
version `extension-ci-tools`' own wasm job pins). A newer emsdk defaults
`WASM_BIGINT` **on**, which makes the side module import `invoke_*` trampolines
with real `i64` parameters while duckdb-wasm's main module offers the legalized
`i32`-pair form — `LOAD` then fails with

```
LinkError: WebAssembly.Instance(): Import #27 "env" "invoke_viiiij":
imported function does not match the expected type
```

The gate says so when it sees that shape. It is an ABI mismatch between your
local emsdk and the prebuilt duckdb-wasm, not a fault in the extension; build
with 3.1.71 to reproduce what ships. Which `invoke_*` trampolines exist at all
also depends on the Rust version (1.87+ moved `wasm32-unknown-emscripten` to
native wasm exceptions), so a modern local toolchain can hide the mismatch
entirely — another reason the CI job pins both.

## Two implementation notes

**The repository server runs in a child process, on purpose.** duckdb-wasm
fetches a loadable extension through an emscripten shim that makes an async
`fetch` look synchronous to its C++ caller: it hands the fetch to a worker and
parks the calling thread on `Atomics.wait`. `LOAD` therefore blocks the whole JS
thread that issued it, so a server hosted on that thread can never accept the
connection — the two sides deadlock and `LOAD` hangs forever with the server
logging no request. `serve.mjs` keeps its event loop out of reach of that.

**`HOME` is redirected to a scratch directory per run.** The same shim caches
every extension it fetches under `~/.duckdb/extensions/…` and reuses the file if
it is already there. Left alone, a stale cache would quietly gate the *previous*
build.

## What this does not cover

- **`wasm_threads`.** It is built and shipped, but duckdb-wasm's matching
  runtime (`coi`) needs `SharedArrayBuffer` and a pthread worker, and has no
  node-blocking bundle. `wasm_mvp` and `wasm_eh` share one cargo build with it,
  so a compile break still surfaces.
- **A real browser.** This is node with duckdb-wasm's node runtime. It exercises
  the same wasm artifact and the same `LOAD` path; it does not exercise a
  browser's filesystem shims or its HTTP/S3 reads.
- **`http(s)://` / `s3://` sources.** `read_ags` is proven here against a
  host-registered buffer (what a browser page hands DuckDB) and a host
  filesystem path. Remote reads go through duckdb-wasm's own HTTP runtime and
  are unmeasured.
- **Error-message text on `wasm_mvp`.** That host loses every C++ exception to a
  JS `_setThrew is not defined` — including for a plain
  `SELECT * FROM no_such_table` with no extension loaded, so it is the host, not
  this extension. The gate probes for that and asserts message content only
  where the host can carry it (`wasm_eh` does), which means the assertion
  tightens by itself once duckdb-wasm fixes the mvp bundle.
