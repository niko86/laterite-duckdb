# laterite_ags4

AGS4 ground-investigation files as typed, join-ready SQL tables, straight from DuckDB.

AGS4 is how the UK geotechnical industry exchanges borehole and lab data: a grouped,
comma-delimited text format where every value is a quoted string and the links between
groups (locations, samples, lab tests, geology) live in repeated key columns. Getting it
into a database usually means writing a parser, casting dozens of columns by hand, and
working out which keys join to what. This extension does that part for you.

`read_ags('site.ags', 'LOCA')` returns a DuckDB table whose columns are already typed
from the AGS data dictionary — ground levels as DOUBLE, dates as TIMESTAMP, and so on —
and every row carries an `_id` and `_parent_id`. Those ids are a hash of the AGS key
columns, not something allocated at runtime, so a sample from the SAMP group and its
borehole from LOCA land on the same id without you naming the join columns. That holds
whether you read the groups in one query or several, and from one file or many.

It's a loadable extension written in Rust on DuckDB's C extension API — no C++, and no
bundled engine, so it stays small.

```sql
INSTALL laterite_ags4 FROM community;
LOAD laterite_ags4;

-- columns typed straight from the file
SELECT loca_id, loca_gl FROM read_ags('site.ags', 'LOCA') WHERE loca_gl > 50.0;

-- samples joined to their boreholes; the keys line up by construction
SELECT l.loca_id, s.samp_top, s.samp_ref
FROM read_ags('site.ags', 'SAMP') s
JOIN read_ags('site.ags', 'LOCA') l ON s._parent_id = l._id;

-- remote files through DuckDB's filesystem
LOAD httpfs;
SELECT loca_id FROM read_ags('s3://bucket/site.ags', 'LOCA');
```

## SQL surface

| function | what it does |
|---|---|
| `read_ags(path, group)` | one group as a typed table: `_id` + `_parent_id` (UUIDv8) first, then a column per heading typed from the file's `TYPE` row; streamed lazily |
| `read_ags_text(content, group)` | same typed output, but the AGS4 text is passed as a VARCHAR argument (literal or bound parameter) instead of a path — no filesystem, so it's the reader available in the **WASM** build |
| `ags_groups(path)` | the file's groups — `(group, n_rows, n_headings, parent)` |
| `ags_headings(path)` | per-heading detail — `(group, heading, unit, ags_type, sql_type, status, is_key, ordinal)` |
| `ags_dictionary()` / `ags_relationships()` | the embedded AGS dictionary and its relationship graph |
| `validate_ags(path[, edition := '4.2'])` | opt-in AGS4 rule check; never gates a read |
| `certify_ags(path[, edition := '4.2'])` | validate and, if clean, mint a `.ags.idx` **certificate** (a byte-offset index + validation provenance) beside the file; returns a one-row status (an invalid file is reported, not certified) |
| `load_ags_script(path)` | emits CREATE TABLE DDL to materialise an indexed, keyed copy |

Paths go through DuckDB's filesystem, so local files, `http(s)://`, and `s3://`
(with `LOAD httpfs`) all work.

### The `.ags.idx` certificate

`certify_ags` writes a sibling `<file>.ags.idx` — a byte-offset index over each
group's section plus a record of *who* validated the file clean (the engine, its
version, the edition). It exists only for a file that validated clean, so two
fast-paths then consult a fresh one automatically:

- **`read_ags`** range-reads just the requested group's bytes (one `seek` + `read`,
  local or remote) instead of slurping + parsing the whole file — the cold
  single-group win, biggest on large deliveries.
- **`validate_ags`** returns clean without re-running the rule pass.

A *read* trusts a cheap **size** match (re-hashing a remote object to read one
group would mean re-downloading it); a *verdict* (`validate_ags`) confirms the
strong **SHA-256**. Any change to the file makes the certificate stale — it's then
ignored and the validating whole-file path runs, so a stale `.ags.idx` can never
serve wrong data. The certificate is a regenerable cache: delete it freely, or
re-run `certify_ags`. Its format and checker identity match the `laterite` Python
wheel's `Ags4File.certify()`, so a certificate minted by either is trusted by both.

## In the browser (DuckDB-WASM)

The path readers use DuckDB's virtual filesystem, which depends on an unstable C
API revision DuckDB-WASM doesn't yet match — so the **WASM build ships a stable
subset**: `read_ags_text` + `ags_dictionary` + `ags_relationships`. Your app
already holds the AGS bytes (an upload or fetch), so you hand the text in as a
bound parameter:

```js
import * as duckdb from 'https://cdn.jsdelivr.net/npm/@duckdb/duckdb-wasm/+esm';
// … standard duckdb-wasm setup: instantiate AsyncDuckDB, then `const conn = await db.connect()` …

await conn.query("INSTALL laterite_ags4 FROM community");
await conn.query("LOAD laterite_ags4");

// the file's text — e.g. from <input type="file"> (.text()) or fetch(url).then(r => r.text())
const agsText = await file.text();

const stmt = await conn.prepare(
  "SELECT loca_id, loca_gl FROM read_ags_text(?, 'LOCA') WHERE loca_gl > 50.0"
);
const result = await stmt.query(agsText);
console.table(result.toArray().map(r => r.toJSON()));   // born-typed — loca_gl is a number
await stmt.close();
```

`read_ags_text` works on native too (handy when AGS content is already in memory).
The content must be a literal or bound parameter — DuckDB doesn't allow a subquery
such as `read_text(...)` as a table-function argument. The path/remote readers
return on WASM once its engine catches up to native.

## How the keys join by construction

Each row's `_id` is `UUIDv8(SHA-256(its AGS key-chain))`, and `_parent_id` is the same
hash over the parent's key-chain — which an AGS4 child row already carries, denormalised.
So `child._parent_id == parent._id` falls out of the data with no shared state, and two
separate `read_ags(...)` calls produce identical ids. The hash is taken over the raw AGS4
strings, never parsed numbers, which keeps it stable across files and dictionary editions.

## Build from source

The four `laterite-*` library crates come from the
[`laterite`](https://github.com/niko86/laterite) mirror as a git submodule (not
crates.io); `extension-ci-tools` is a submodule too.

```sh
git clone --recursive https://github.com/niko86/laterite-duckdb
cd laterite-duckdb
make configure      # one-time: build env + test DuckDB (writes a venv under configure/)
make release        # build the loadable extension
make test           # run the sqllogictests
```

The binary is built for one specific DuckDB version — the C extension API it uses for
filesystem access pins it, so a build is not portable across DuckDB releases.
community-extensions builds one per supported release, so `INSTALL ... FROM community`
fetches the matching one. Built and tested against DuckDB 1.5.4.

## License

MIT. Built with [quack-rs](https://github.com/tomtom215/quack-rs); the query engine is
the [laterite](https://github.com/niko86/laterite) AGS4 toolkit.
