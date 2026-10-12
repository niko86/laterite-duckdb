#!/usr/bin/env python3
"""Check that this extension's SQL functions give the same answers as the
`laterite` Python package at the pinned version (#57).

Release X of the extension pins the same engine crates as laterite X, so the
functions the two share must agree. This script holds both halves of that:

1. The engine pin: every engine crate this extension resolves (Cargo.lock) must
   satisfy the requirement the crates.io facade crate `laterite` at the pinned
   version places on it — the authoritative "engine of laterite X".
2. The answers: load the built extension into python duckdb (as the
   sqllogictest runner does), install `laterite==<pin>` beside it, and compare
   every shared function on every fixture, failing on any difference.

Differences the two surfaces have ON PURPOSE are listed in BY_DESIGN; known
divergences awaiting a fix are listed in KNOWN_DIVERGENCES. Each entry is
applied by name and printed on every run, and a known divergence that stops
diverging fails the run, so the list cannot outlive the bug it excuses.

Usage: scripts/laterite_parity.py --extension build/debug/laterite_ags4.duckdb_extension
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import sys
import tomllib
import typing
import urllib.request
from dataclasses import dataclass, field
from importlib.metadata import version as installed_version
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
PIN_FILE = ROOT / "LATERITE_VERSION"
ENGINE_CRATES = (
    "laterite-ags4-core",
    "laterite-ags4-types",
    "laterite-ags4-reference",
    "laterite-ags4-parse",
)
FIXTURE_GLOBS = ("test/sql/*.ags", "tests/fixtures/*.ags", "test/parity/*.ags")
USER_AGENT = "laterite-duckdb-parity (https://github.com/niko86/laterite-duckdb)"

# Functions with no laterite counterpart: they emit DDL for DuckDB itself.
SKIPPED = {
    "load_ags": "emits CREATE TABLE DDL for DuckDB; laterite has no DDL emitter",
    "to_duckdb": "emits ATTACH/CREATE/DETACH DDL for DuckDB; laterite writes the file itself",
}

# Differences by design. Each is applied by name where the comparison meets it.
BY_DESIGN = {
    "temporal-as-varchar": (
        "read_ags carries DT columns as their canonical VARCHAR (native temporal "
        "typing deferred, src/typing.rs); laterite types them, so the values are "
        "compared after parsing the string"
    ),
    "relationship-key-order": (
        "ags_relationships.shared_keys is a comma-joined string; laterite returns "
        "the inherited keys as a set, so they are compared as sets"
    ),
}

# Known divergences, not by design. Each must still diverge or the run fails.
KNOWN_DIVERGENCES = {}


class Fail(Exception):
    pass


@dataclass
class Run:
    diffs: list[str] = field(default_factory=list)
    compared: set[str] = field(default_factory=set)
    by_design_hits: dict[str, int] = field(default_factory=dict)
    known_hits: dict[str, int] = field(default_factory=dict)

    def diff(self, fn: str, where: str, what: str, ext: object, lat: object) -> None:
        self.diffs.append(f"{fn} [{where}] {what}: extension={ext!r} laterite={lat!r}")

    def by_design(self, name: str) -> None:
        assert name in BY_DESIGN, name
        self.by_design_hits[name] = self.by_design_hits.get(name, 0) + 1

    def known(self, name: str) -> None:
        assert name in KNOWN_DIVERGENCES, name
        self.known_hits[name] = self.known_hits.get(name, 0) + 1


# --------------------------------------------------------------------------
# 1. the engine pin
# --------------------------------------------------------------------------


def crates_io(path: str) -> dict:
    req = urllib.request.Request(
        f"https://crates.io/api/v1/{path}", headers={"User-Agent": USER_AGENT}
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp)


def parse_version(v: str) -> tuple[int, int, int]:
    m = re.fullmatch(r"(\d+)(?:\.(\d+))?(?:\.(\d+))?", v.strip())
    if not m:
        raise Fail(f"cannot parse version {v!r}")
    return tuple(int(x or 0) for x in m.groups())  # type: ignore[return-value]


def satisfies(version: str, requirement: str) -> bool:
    """Cargo's requirement grammar for the forms crates.io serves
    (caret, tilde, exact, comparisons, comma-joined). Anything else raises:
    an unreadable requirement must fail the gate, not pass it."""
    v = parse_version(version)
    for part in requirement.split(","):
        part = part.strip()
        m = re.fullmatch(r"(\^|~|=|>=|<=|>|<)?\s*([\d.]+)", part)
        if not m:
            raise Fail(f"unsupported version requirement {requirement!r}")
        op, raw = m.group(1) or "^", m.group(2)
        r = parse_version(raw)
        n = raw.count(".") + 1
        if op == "^":
            if r[0] > 0 or n == 1:
                upper = (r[0] + 1, 0, 0)
            elif r[1] > 0 or n == 2:
                upper = (0, r[1] + 1, 0)
            else:
                upper = (0, 0, r[2] + 1)
            ok = r <= v < upper
        elif op == "~":
            upper = (r[0] + 1, 0, 0) if n == 1 else (r[0], r[1] + 1, 0)
            ok = r <= v < upper
        elif op == "=":
            ok = v[:n] == r[:n]
        else:
            ok = {">=": v >= r, "<=": v <= r, ">": v > r, "<": v < r}[op]
        if not ok:
            return False
    return True


def check_engine_pin(pin: str) -> list[str]:
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
    resolved: dict[str, list[str]] = {}
    for pkg in lock["package"]:
        if pkg["name"] in ENGINE_CRATES:
            resolved.setdefault(pkg["name"], []).append(pkg["version"])
    deps = crates_io(f"crates/laterite/{pin}/dependencies")["dependencies"]
    reqs = {
        d["crate_id"]: (d["req"], f"laterite {pin}")
        for d in deps
        if d["kind"] == "normal" and d["crate_id"].startswith("laterite-ags4-")
    }
    # An engine crate the facade does not name directly (types) is held by the
    # requirement of the facade engine crate that does — at the version this
    # extension resolved it, which the check above already holds to the facade.
    for crate in ENGINE_CRATES:
        if crate in reqs:
            continue
        for via in ENGINE_CRATES:
            if via not in reqs or via not in resolved:
                continue
            sub = crates_io(f"crates/{via}/{resolved[via][0]}/dependencies")
            for d in sub["dependencies"]:
                if d["crate_id"] == crate and d["kind"] == "normal":
                    reqs[crate] = (d["req"], f"{via} {resolved[via][0]}")
            if crate in reqs:
                break
    problems = []
    for crate in ENGINE_CRATES:
        got = resolved.get(crate)
        if not got:
            problems.append(f"{crate}: not in Cargo.lock")
            continue
        if crate not in reqs:
            problems.append(f"{crate}: laterite {pin} places no requirement on it")
            continue
        req, source = reqs[crate]
        for v in got:
            mark = "ok" if satisfies(v, req) else "MISMATCH"
            print(f"  engine pin: {crate} {v} vs {req} (from {source}) {mark}")
            if mark != "ok":
                problems.append(
                    f"{crate}: Cargo.lock resolves {v}, but laterite {pin} requires "
                    f"{req} (from {source})"
                )
    return problems


# --------------------------------------------------------------------------
# 2. the answers
# --------------------------------------------------------------------------

# polars dtype → the DuckDB column type read_ags emits for it.
POLARS_TO_SQL = {
    "String": "VARCHAR",
    "Float64": "DOUBLE",
    "Int64": "BIGINT",
    "Boolean": "BOOLEAN",
    "Datetime": "TIMESTAMP",
    "Date": "DATE",
    "Time": "TIME",
}
TEMPORAL = {"TIMESTAMP", "DATE", "TIME"}
# laterite canonical type → SQL type (laterite_ags4_types::CanonicalType::sql_type).
CANONICAL_TO_SQL = {
    "string": "VARCHAR",
    "enum": "VARCHAR",
    "integer": "BIGINT",
    "decimal": "DOUBLE",
    "datetime": "TIMESTAMP",
    "date": "DATE",
    "time": "TIME",
    "bool": "BOOLEAN",
}


def polars_sql(dtype: object) -> str:
    name = type(dtype).__name__ if not isinstance(dtype, type) else dtype.__name__
    return POLARS_TO_SQL.get(name, f"<polars {dtype}>")


def canonical_sql(ags_type: str) -> str:
    from laterite.ags_types import canonical_type

    try:
        return CANONICAL_TO_SQL[str(canonical_type(ags_type))]
    except ValueError:
        return "VARCHAR"  # an unknown code passes through as text on both sides


def blank(x: str | None) -> str | None:
    return x if x else None


def temporal_equal(ext: object, lat: object) -> bool:
    """An extension temporal cell (canonical VARCHAR) against laterite's typed one."""
    if ext is None or lat is None:
        return ext is None and lat is None
    parse = {
        dt.datetime: dt.datetime.fromisoformat,
        dt.date: dt.date.fromisoformat,
        dt.time: dt.time.fromisoformat,
    }[type(lat)]
    try:
        return parse(str(ext)) == lat
    except ValueError:
        return False


class Surfaces:
    def __init__(self, extension: Path) -> None:
        import duckdb

        self.con = duckdb.connect(config={"allow_unsigned_extensions": "true"})
        self.con.execute(f"LOAD '{extension}'")

    def rows(self, sql: str, params: list | None = None) -> tuple[list, list, list]:
        rel = self.con.execute(sql, params or [])
        cols = [d[0] for d in rel.description]
        types = [str(d[1]) for d in rel.description]
        return cols, types, rel.fetchall()


def file_dict(ags: Any) -> tuple[dict, dict]:
    """laterite's read of the file's own DICT group: declared parents, statuses."""
    parents: dict[str, str | None] = {}
    statuses: dict[tuple[str, str], str] = {}
    if "DICT" not in ags.groups:
        return parents, statuses
    for r in ags.table("DICT").iter_rows(named=True):
        kind, grp = r.get("DICT_TYPE"), r.get("DICT_GRP")
        if kind == "GROUP" and grp:
            p = r.get("DICT_PGRP")
            parents[grp] = p if p and p != "-" else None
        elif kind == "HEADING" and grp and r.get("DICT_HDNG"):
            statuses[(grp, r["DICT_HDNG"])] = r.get("DICT_STAT") or ""
    return parents, statuses


def compare_read(run: Run, s: Surfaces, fixture: str, ags: Any, fn: str) -> None:
    run.compared.add(fn)
    if fn == "read_ags":
        call = lambda g: s.rows("SELECT * FROM read_ags(?, ?)", [fixture, g])
    else:
        text = (ROOT / fixture).read_text(encoding="utf-8")
        call = lambda g: s.rows("SELECT * FROM read_ags_text(?, ?)", [text, g])
    for g in ags.groups:
        where = f"{fixture} {g}"
        cols, types, rows = call(g)
        df = ags.table(g, keys=True)
        if cols != df.columns:
            run.diff(fn, where, "columns", cols, df.columns)
            continue
        for i, (c, et) in enumerate(zip(cols, types)):
            lt = polars_sql(df.schema[c])
            if lt in TEMPORAL and et == "VARCHAR":
                run.by_design("temporal-as-varchar")
            elif et != lt:
                run.diff(fn, where, f"type of {c}", et, lt)
                continue
            lat_vals = df[c].to_list()
            if len(rows) != len(lat_vals):
                run.diff(fn, where, f"row count of {c}", len(rows), len(lat_vals))
                continue
            for n, (ev, lv) in enumerate(zip((r[i] for r in rows), lat_vals)):
                same = temporal_equal(ev, lv) if lt in TEMPORAL else ev == lv
                if same:
                    continue
                run.diff(fn, where, f"{c} row {n}", ev, lv)


def compare_groups(run: Run, s: Surfaces, fixture: str, ags: Any) -> None:
    from laterite.registry import GROUPS

    run.compared.add("ags_groups")
    _, _, rows = s.rows("SELECT * FROM ags_groups(?)", [fixture])
    parents, _ = file_dict(ags)
    want = []
    for g in ags.groups:
        reg = GROUPS.get(g)
        parent = (reg.parent if reg else None) or parents.get(g)
        n_rows = ags.table(g).height
        want.append((g, n_rows, len(ags.headings(g)), parent))
    if [r[0] for r in rows] != [w[0] for w in want]:
        run.diff(
            "ags_groups",
            fixture,
            "group list",
            [r[0] for r in rows],
            [w[0] for w in want],
        )
        return
    for r, w in zip(rows, want):
        for name, ev, lv in zip(("n_rows", "n_headings", "parent"), r[1:], w[1:]):
            if ev != lv:
                run.diff("ags_groups", f"{fixture} {r[0]}", name, ev, lv)


def compare_headings(run: Run, s: Surfaces, fixture: str, ags: Any) -> None:
    from laterite.registry import GROUPS

    run.compared.add("ags_headings")
    _, _, rows = s.rows("SELECT * FROM ags_headings(?)", [fixture])
    _, declared = file_dict(ags)
    want = []
    for g in ags.groups:
        df = ags.table(g, keys=True)
        reg = {h.name: h for h in GROUPS[g].headings} if g in GROUPS else {}
        heads = ags.headings(g)
        units, types = ags.units(g), ags.types(g)
        for i, h in enumerate(heads):
            if h in reg:
                status, key = reg[h].status, reg[h].is_key
            elif (g, h) in declared:
                status = declared[(g, h)]
                key = "KEY" in status.upper()
            else:
                status, key = "OTHER", False
            unit = units[i] if i < len(units) else ""
            ags_type = types[i] if i < len(types) else ""
            sql = polars_sql(df.schema[h])
            want.append((g, h, blank(unit), ags_type, sql, status, key, i))
    if [r[:2] for r in rows] != [w[:2] for w in want]:
        run.diff(
            "ags_headings",
            fixture,
            "heading list",
            [r[:2] for r in rows],
            [w[:2] for w in want],
        )
        return
    names = ("unit", "ags_type", "sql_type", "status", "is_key", "ordinal")
    for r, w in zip(rows, want):
        for name, ev, lv in zip(names, r[2:], w[2:]):
            if ev != lv:
                run.diff("ags_headings", f"{fixture} {r[0]}.{r[1]}", name, ev, lv)


def dictionary_rows(groups: list[dict]) -> dict[str, list[tuple]]:
    out = {}
    for g in groups:
        out[g["code"]] = [
            (
                blank(g.get("parent")),
                h["name"],
                h["status"],
                h["type"],
                canonical_sql(h["type"]),
                blank(h.get("unit")),
                blank(h.get("description")),
                i,
            )
            for i, h in enumerate(g["headings"])
        ]
    return out


def compare_dictionary(run: Run, s: Surfaces) -> int:
    import laterite
    from laterite.registry import GROUPS, dictionary

    run.compared.add("ags_dictionary")
    editions: list[str | None] = [None, *typing.get_args(laterite.Edition)]
    for ed in editions:
        label = f"edition {ed}" if ed else "union registry"
        if ed:
            _, _, rows = s.rows("SELECT * FROM ags_dictionary(edition := ?)", [ed])
            want = dictionary_rows(dictionary(ed)["groups"])
        else:
            _, _, rows = s.rows("SELECT * FROM ags_dictionary()")
            want = dictionary_rows(
                [
                    {
                        "code": g.code,
                        "parent": g.parent,
                        "headings": [
                            {
                                "name": h.name,
                                "status": h.status,
                                "type": h.type,
                                "unit": h.unit,
                                "description": h.description,
                            }
                            for h in g.headings
                        ],
                    }
                    for g in GROUPS.values()
                ]
            )
        got: dict[str, list[tuple]] = {}
        for r in rows:
            got.setdefault(r[0], []).append(tuple(r[1:]))
        if set(got) != set(want):
            run.diff(
                "ags_dictionary",
                label,
                "group set",
                sorted(set(got) - set(want)),
                sorted(set(want) - set(got)),
            )
        names = (
            "parent",
            "heading",
            "status",
            "ags_type",
            "sql_type",
            "unit",
            "description",
            "ordinal",
        )
        for code in sorted(set(got) & set(want)):
            e, w = got[code], want[code]
            if len(e) != len(w):
                run.diff(
                    "ags_dictionary", f"{label} {code}", "heading count", len(e), len(w)
                )
                continue
            for er, wr in zip(e, w):
                for name, ev, lv in zip(names, er, wr):
                    if ev != lv:
                        run.diff(
                            "ags_dictionary", f"{label} {code}.{wr[1]}", name, ev, lv
                        )
    return len(editions) - 1


def compare_relationships(run: Run, s: Surfaces) -> None:
    from laterite.registry import GROUPS, child_groups, inherited_key_names

    run.compared.add("ags_relationships")
    _, _, rows = s.rows("SELECT * FROM ags_relationships()")
    got = {(c, p): k for c, p, k in rows}
    want = {(c.code, parent) for parent in GROUPS for c in child_groups(parent)}
    if set(got) != want:
        run.diff(
            "ags_relationships",
            "graph",
            "edges",
            sorted(set(got) - want),
            sorted(want - set(got)),
        )
    for c, p in sorted(set(got) & want):
        run.by_design("relationship-key-order")
        keys = {k for k in got[(c, p)].split(",") if k}
        if keys != inherited_key_names(c):
            run.diff(
                "ags_relationships",
                f"{c}->{p}",
                "shared_keys",
                sorted(keys),
                sorted(inherited_key_names(c)),
            )


def compare_rules(run: Run, s: Surfaces) -> None:
    import laterite

    run.compared.add("ags_rules")
    _, _, rows = s.rows("SELECT * FROM ags_rules()")
    want = laterite.list_rules()
    if [r[0] for r in rows] != [w["rule"] for w in want]:
        run.diff(
            "ags_rules",
            "catalogue",
            "rule list",
            [r[0] for r in rows],
            [w["rule"] for w in want],
        )
        return
    for r, w in zip(rows, want):
        obs = [
            o["id"] if isinstance(o, dict) else o for o in w.get("observations") or []
        ]
        expect = (
            w["rule"],
            blank(w.get("title")),
            blank(w.get("checks")),
            blank(w.get("severity")),
            w.get("fixable"),
            ",".join(obs) or None,
        )
        names = ("rule", "title", "checks", "severity", "fixable", "observations")
        for name, ev, lv in zip(names, r, expect):
            if ev != lv:
                run.diff("ags_rules", f"rule {w['rule']}", name, ev, lv)


# --------------------------------------------------------------------------


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--extension", required=True, type=Path)
    args = ap.parse_args()
    extension = args.extension.resolve()
    if not extension.is_file():
        print(f"FAIL: no built extension at {extension} (run `make debug` first)")
        return 1
    # Fixture paths are passed to read_ags relative to the repo root, as the
    # sqllogictest file does, so every message names them the same way.
    os.chdir(ROOT)

    pin = PIN_FILE.read_text().strip()
    have = installed_version("laterite")
    if have != pin:
        print(f"FAIL: laterite {have} is installed but {PIN_FILE.name} pins {pin}")
        return 1
    print(f"laterite {pin} (pinned in {PIN_FILE.name}) vs {extension}")

    problems = check_engine_pin(pin)
    if problems:
        print(f"FAIL: the extension's engine is not laterite {pin}'s engine:")
        for p in problems:
            print(f"  - {p}")
        print(
            "  Move the engine requirements in Cargo.toml to the set laterite "
            f"{pin} ships, or change {PIN_FILE.name}."
        )
        return 1

    import laterite

    manifest = json.loads((ROOT / "functions.json").read_text())
    functions = {f["name"] for f in manifest["functions"]}
    fixtures = sorted(
        str(p.relative_to(ROOT)) for pat in FIXTURE_GLOBS for p in ROOT.glob(pat)
    )
    s = Surfaces(extension)
    run = Run()
    for fx in fixtures:
        ags = laterite.read(path=ROOT / fx, keys=True, content_hash=True)
        compare_read(run, s, fx, ags, "read_ags")
        compare_read(run, s, fx, ags, "read_ags_text")
        compare_groups(run, s, fx, ags)
        compare_headings(run, s, fx, ags)
    n_editions = compare_dictionary(run, s)
    compare_relationships(run, s)
    compare_rules(run, s)

    unaccounted = functions - run.compared - set(SKIPPED)
    for fn in sorted(unaccounted):
        run.diffs.append(f"{fn}: in functions.json but neither compared nor skipped")
    for name in sorted(set(KNOWN_DIVERGENCES) - set(run.known_hits)):
        run.diffs.append(
            f"known divergence '{name}' no longer seen: remove it from KNOWN_DIVERGENCES"
        )
    for name in sorted(set(BY_DESIGN) - set(run.by_design_hits)):
        run.diffs.append(
            f"by-design exclusion '{name}' never applied: remove it from BY_DESIGN"
        )

    for name, reason in BY_DESIGN.items():
        print(f"  by design   {name} (x{run.by_design_hits.get(name, 0)}): {reason}")
    for name, reason in KNOWN_DIVERGENCES.items():
        print(f"  KNOWN DIVERGENCE {name} (x{run.known_hits.get(name, 0)}): {reason}")
    for fn, reason in SKIPPED.items():
        print(f"  skipped     {fn}: {reason}")
    for d in run.diffs:
        print(f"  DIFF {d}")

    verdict = "FAIL" if run.diffs else "PASS"
    print(
        f"{verdict}: laterite {pin} parity — {len(run.compared)} functions compared "
        f"({', '.join(sorted(run.compared))}), {len(SKIPPED)} skipped with no laterite "
        f"counterpart ({', '.join(SKIPPED)}), {len(BY_DESIGN)} by-design exclusions and "
        f"{len(KNOWN_DIVERGENCES)} known divergences applied, {len(fixtures)} fixtures "
        f"checked, {n_editions} dictionary editions, {len(run.diffs)} differences"
    )
    return 1 if run.diffs else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Fail as e:
        print(f"FAIL: {e}")
        sys.exit(1)
