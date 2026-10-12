#!/usr/bin/env python3
"""Gate: the column lists the docs state are the columns the functions return.

functions.json (`returns`) and docs/function_descriptions.csv (`comment`) both
name each table function's output columns, and DuckDB's community page is built
from the csv. Nothing compared either with the real output, so ags_rules shipped
0.15.0 documented without its `checks` and `observations` columns and
ags_dictionary without three of its nine. This loads the built extension,
DESCRIBEs every function, and fails on any list that differs, name or order.

A function whose columns depend on its input (read_ags, read_ags_text) states no
list; it is printed as skipped on every run, never silently dropped.

Usage: scripts/check_function_docs.py --extension build/debug/laterite_ags4.duckdb_extension
"""

from __future__ import annotations

import argparse
import csv
import json
import re
import sys
from pathlib import Path

import duckdb

ROOT = Path(__file__).resolve().parent.parent
FIXTURE = ROOT / "test/sql/mini.ags"

# How to call each function so DESCRIBE can see its output. A function missing
# here fails the run: a new function must be enrolled, not skipped by accident.
CALLS = {
    "read_ags": None,
    "read_ags_text": None,
    "ags_groups": f"ags_groups('{FIXTURE}')",
    "ags_headings": f"ags_headings('{FIXTURE}')",
    "ags_dictionary": "ags_dictionary()",
    "ags_relationships": "ags_relationships()",
    "ags_rules": "ags_rules()",
    "load_ags": f"load_ags('{FIXTURE}')",
    "to_duckdb": f"to_duckdb('{FIXTURE}', 'unused.duckdb')",  # DESCRIBE binds only; nothing is written
}

TUPLE = re.compile(r"\(([a-z_]+(?:, [a-z_]+)+)\)")


def stated(text: str) -> list[str] | None:
    """The first `(a, b, c)` column list in a doc string, or None."""
    m = TUPLE.search(text)
    return m.group(1).split(", ") if m else None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--extension", required=True)
    args = ap.parse_args()

    manifest = {
        f["name"]: f
        for f in json.loads((ROOT / "functions.json").read_text())["functions"]
    }
    with (ROOT / "docs/function_descriptions.csv").open(newline="") as fh:
        rows = {r["function"]: r for r in csv.DictReader(fh)}

    ext = Path(args.extension).resolve()
    con = duckdb.connect(config={"allow_unsigned_extensions": "true"})
    con.execute(f"LOAD '{ext}'")

    problems: list[str] = []
    checked: list[str] = []
    skipped: list[str] = []
    for name in sorted(manifest):
        if name not in CALLS:
            problems.append(f"{name}: not enrolled in CALLS — add how to call it")
            continue
        sources = {
            "functions.json": stated(manifest[name]["returns"]),
            "function_descriptions.csv": stated(rows[name]["comment"])
            if name in rows
            else None,
        }
        if name not in rows:
            problems.append(f"{name}: missing from docs/function_descriptions.csv")
        call = CALLS[name]
        if call is None:
            for src, cols in sources.items():
                if cols is not None:
                    problems.append(
                        f"{name}: {src} states fixed columns {cols}, but its columns depend on the input"
                    )
            skipped.append(name)
            continue
        actual = [
            r[0] for r in con.execute(f"DESCRIBE SELECT * FROM {call}").fetchall()
        ]
        for src, cols in sources.items():
            if cols is None:
                problems.append(
                    f"{name}: {src} states no column list; the function returns {actual}"
                )
            elif cols != actual:
                problems.append(
                    f"{name}: {src} says {cols}, the function returns {actual}"
                )
        checked.append(name)

    for p in problems:
        print(f"DIFF {p}")
    verdict = "FAIL" if problems else "PASS"
    print(
        f"{verdict}: function docs — {len(checked)} functions checked against DESCRIBE in both "
        f"functions.json and function_descriptions.csv, {len(skipped)} skipped with input-dependent "
        f"columns ({', '.join(skipped)}), {len(problems)} differences"
    )
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
