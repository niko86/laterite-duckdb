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
| `ags_groups(path)` | the file's groups — `(group, n_rows, n_headings, parent)` |
| `ags_headings(path)` | per-heading detail — `(group, heading, unit, ags_type, sql_type, status, is_key, ordinal)` |
| `ags_dictionary()` / `ags_relationships()` | the embedded AGS dictionary and its relationship graph |
| `ags_validate(path[, edition := '4.2'])` | opt-in AGS4 rule check; never gates a read |
| `load_ags_script(path)` | emits CREATE TABLE DDL to materialise an indexed, keyed copy |

Paths go through DuckDB's filesystem, so local files, `http(s)://`, and `s3://`
(with `LOAD httpfs`) all work.

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
