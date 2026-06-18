# laterite_ags4 — AGS4 for DuckDB

A DuckDB **community extension** that reads [AGS4](https://www.ags.org.uk/)
geotechnical & geoenvironmental data files as first-class, **typed, UUID-keyed**
tables — straight from SQL. Written in 🦀 Rust on DuckDB's C Extension API (zero C++),
with **no bundled engine** (a light loadable extension).

```sql
INSTALL laterite_ags4 FROM community;
LOAD laterite_ags4;

-- Born-typed columns (typed from the file's own TYPE row):
SELECT loca_id, loca_gl FROM read_ags('site.ags', 'LOCA') WHERE loca_gl > 50.0;

-- Groups join across independent calls on deterministic keys, by construction:
SELECT l.loca_id, s.samp_ref
FROM read_ags('site.ags', 'SAMP') s
JOIN read_ags('site.ags', 'LOCA') l ON s._parent_id = l._id;

-- Remote, lazily:
LOAD httpfs;
SELECT loca_id FROM read_ags('s3://bucket/site.ags', 'LOCA');
```

## SQL surface

| function | what |
|---|---|
| `read_ags(path, group)` | a group as a typed table; `_id` + `_parent_id` (UUIDv8) first, then one column per heading typed from the file's `TYPE` row; lazy streaming |
| `ags_groups(path)` | the file's group list — `(group, n_rows, n_headings, parent)` |
| `ags_headings(path)` | per-heading `(group, heading, unit, ags_type, sql_type, status, is_key, ordinal)` |
| `ags_dictionary()` / `ags_relationships()` | the embedded AGS dictionary + relationship graph |
| `ags_validate(path[, edition := '4.2'])` | opt-in AGS4 rule check; never a gate on reads |
| `load_ags_script(path)` | CREATE-TABLE DDL to materialise an indexed, keyed store |

`read_ags` reads through DuckDB's virtual filesystem, so local paths, `http(s)://`
and `s3://` (with `LOAD httpfs`) all work. Requires **DuckDB 1.5.0+**.

## Why the keys join "by construction"

Every row's `_id` is `UUIDv8(SHA-256(its spec key-chain))`, and `_parent_id` is
the same function over the **parent's** key-chain (which an AGS4 child row carries
denormalised). So `child._parent_id == parent._id` with no shared state — two
separate `read_ags(...)` calls agree on every id. Hashing the raw AGS4 strings
(never parsed floats) keeps that stable across editions.

## Build from source

The four `laterite-*` library crates come from the [`laterite`](https://github.com/niko86/laterite)
mirror as a git submodule (not crates.io); `extension-ci-tools` is a submodule too:

```sh
git clone --recursive https://github.com/niko86/laterite-duckdb
cd laterite-duckdb
make            # build the extension
make test       # run the sqllogictests
```

## License

MIT. Built with [quack-rs](https://github.com/tomtom215/quack-rs); the engine is
the [laterite](https://github.com/niko86/laterite) AGS4 toolkit.
