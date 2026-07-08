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

## Use it from your stack

It loads into any DuckDB client — there's no Python/Node package to install, just DuckDB.

**DuckDB CLI**

```sh
duckdb -c "INSTALL laterite_ags4 FROM community; LOAD laterite_ags4;
           SELECT loca_id, loca_gl FROM read_ags('site.ags','LOCA');"
```

**Python** (`pip install duckdb`)

```python
import duckdb

con = duckdb.connect(config={"allow_unsigned_extensions": "true"})
con.execute("INSTALL laterite_ags4 FROM community; LOAD laterite_ags4")
df = con.sql("SELECT loca_id, loca_gl FROM read_ags('site.ags','LOCA')").pl()  # -> polars
```

**Node** (`npm i @duckdb/node-api`)

```js
import { DuckDBInstance } from "@duckdb/node-api";

const con = await (await DuckDBInstance.create()).connect();
await con.run("INSTALL laterite_ags4 FROM community; LOAD laterite_ags4");
const reader = await con.runAndReadAll(
  "SELECT loca_id, loca_gl FROM read_ags('site.ags','LOCA')",
);
console.table(reader.getRowObjects());
```

## Part of the laterite suite

One clean-room Rust AGS4 engine, surfaced for every stack — this extension is its
DuckDB face. The same typing, keys and validation back each surface.

| Surface | Get it |
|---|---|
| **DuckDB** | `INSTALL laterite_ags4 FROM community` (you are here) |
| **Python** | [`laterite`](https://pypi.org/project/laterite/) — `pip install laterite` |
| **Node.js** | [`laterite`](https://www.npmjs.com/package/laterite) — `npm install laterite` |
| **CLI / browser** | [`lat-check`](https://github.com/niko86/laterite) · [web validator + explorer](https://niko86.github.io/laterite/) |

## SQL surface

| function | what it does |
|---|---|
| `read_ags(path, group[, encoding := 'windows-1252'])` | one group as a typed table: `_id` + `_parent_id` (UUIDv8) first, then a column per heading typed from the file's `TYPE` row; streamed lazily. `encoding :=` (a WHATWG label, default `utf-8`) decodes a non-UTF-8 source file |
| `read_ags_text(content, group)` | same typed output, but the AGS4 text is passed as a VARCHAR argument (literal or bound parameter) instead of a path — no filesystem, so it's the reader available in the **WASM** build. No `encoding` param: a VARCHAR is already-decoded text (use `read_ags(path, encoding := …)` for a non-UTF-8 file) |
| `ags_groups(path)` | the file's groups — `(group, n_rows, n_headings, parent)` |
| `ags_headings(path)` | per-heading detail — `(group, heading, unit, ags_type, sql_type, status, is_key, ordinal)` |
| `ags_dictionary([edition := '4.2'])` / `ags_relationships()` | the embedded AGS dictionary and its relationship graph; no arg = the union registry, `edition :=` = that edition's bundled standard dictionary |
| `ags_rules()` | the numbered AGS4 rule catalogue — `(rule, title, checks, severity, fixable, observations)` — the AGS4 rules the `laterite` validator enforces |
| `load_ags(path)` | emits CREATE TABLE DDL to materialise an indexed, keyed copy |

Paths go through DuckDB's filesystem, so local files, `http(s)://`, and `s3://`
(with `LOAD httpfs`) all work.

### The `.ags.idx` certificate

A sibling `<file>.ags.idx` is a byte-offset index over each group's section plus
a record of *who* validated the file clean (the engine, its version, the edition).
It's **minted outside this extension** — by `lat certify` or the `laterite` Python
wheel's `Ags4File.certify()`, which run the AGS4 rule pass and write the sidecar
only for a file that validated clean. This read-only extension **consumes** one:

- **`read_ags`** range-reads just the requested group's bytes (one `seek` + `read`,
  local or remote) instead of slurping + parsing the whole file — the cold
  single-group win, biggest on large deliveries.

A read trusts a cheap **size** match (re-hashing a remote object to read one group
would mean re-downloading it). Any change to the file makes the certificate stale —
it's then ignored and the whole-file read runs, so a stale `.ags.idx` can never
serve wrong data. The certificate is a regenerable cache: delete it freely, or
re-mint it with `lat certify`.

## Cookbook

**Explore an unfamiliar file** — what's in it, and how each group is typed:

```sql
SELECT * FROM ags_groups('site.ags');                  -- groups, row/heading counts, parent
SELECT heading, unit, ags_type, sql_type, is_key       -- LOCA's columns and their types
FROM ags_headings('site.ags') WHERE "group" = 'LOCA';
```

**Walk the hierarchy** — borehole → sample → lab test, joined on the content-hash keys
(no `USING (LOCA_ID, SAMP_TOP, …)` to spell out):

```sql
SELECT l.loca_id, s.samp_top, t.llpl_ll, t.llpl_pi
FROM read_ags('site.ags','LLPL') t
JOIN read_ags('site.ags','SAMP') s ON t._parent_id = s._id
JOIN read_ags('site.ags','LOCA') l ON s._parent_id = l._id;
```

**Aggregate across groups** — mean plasticity index per borehole:

```sql
SELECT l.loca_id, count(*) AS n, round(avg(t.llpl_pi), 1) AS mean_pi
FROM read_ags('site.ags','LLPL') t
JOIN read_ags('site.ags','SAMP') s ON t._parent_id = s._id
JOIN read_ags('site.ags','LOCA') l ON s._parent_id = l._id
GROUP BY l.loca_id ORDER BY mean_pi DESC;
```

**Persist to native tables** — `load_ags` emits the `CREATE TABLE` DDL; run it to
materialise an indexed, keyed copy you can query without the reader:

```sql
SELECT seq, stmt FROM load_ags('site.ags') ORDER BY seq;
```

**Read one group from a remote delivery** — with `httpfs` and a sibling `site.ags.idx`
(minted externally by `lat certify` / the `laterite` library), only that group's bytes
are fetched (an HTTP range request), not the whole file:

```sql
LOAD httpfs;
SELECT * FROM read_ags('https://example.com/site.ags', 'LOCA');
```

**Merge two deliveries, dedup by content key** — union two phases of a site and
collapse identical `LOCA` rows on `_id` (the content hash), no key columns to name:

```sql
SELECT DISTINCT ON (_id) *
FROM (SELECT * FROM read_ags('phase1.ags','LOCA')
      UNION ALL SELECT * FROM read_ags('phase2.ags','LOCA'));
```

When a location was *revised* between phases (same `LOCA_ID`, changed data), dedup
on the AGS key and keep the later row — carry a version and let `QUALIFY` pick the
winner per key:

```sql
SELECT * FROM (
  SELECT *, 1 AS ver FROM read_ags('phase1.ags','LOCA')
  UNION BY NAME
  SELECT *, 2 AS ver FROM read_ags('phase2.ags','LOCA')
)
QUALIFY row_number() OVER (PARTITION BY loca_id ORDER BY ver DESC) = 1;
```

**Join AGS4 to external data** — `read_ags` is just another table, so it joins
straight to a Parquet file; here each borehole is tagged with its planning zone:

```sql
SELECT l.loca_id, l.loca_gl, z.zone
FROM read_ags('site.ags','LOCA') l
JOIN 'planning_zones.parquet' z ON z.parcel = l.loca_id;
```

…and the reverse — export a typed group back out to Parquet for a warehouse:

```sql
COPY (SELECT * FROM read_ags('site.ags','LOCA')) TO 'loca.parquet';
```

**Boreholes near an alignment** — with DuckDB's `spatial` extension the born-typed
easting/northing become geometry; find holes within 50 m of a route centre-line:

```sql
LOAD spatial;
SELECT loca_id
FROM read_ags('site.ags','LOCA')
WHERE ST_DWithin(
        ST_Point(loca_nate, loca_natn),
        ST_GeomFromText('LINESTRING(531000 181000, 531200 181150)'),
        50);
```

**Deepest sample and its plasticity, per borehole** — the content keys make a
three-group walk a plain join; `arg_max` reads a value from the deepest sample's
lab test in one pass:

```sql
SELECT l.loca_id,
       max(s.samp_top)                AS deepest,
       arg_max(t.llpl_pi, s.samp_top) AS pi_at_deepest
FROM read_ags('site.ags','LOCA') l
JOIN read_ags('site.ags','SAMP') s ON s._parent_id = l._id
JOIN read_ags('site.ags','LLPL') t ON t._parent_id = s._id
GROUP BY l.loca_id;
```

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
