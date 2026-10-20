#!/usr/bin/env python3
"""Fail when scripts/seed-demo-data.py creates a different `events` table than the server.

The seed script writes straight into the DuckDB file with its own CREATE TABLE.
If src/storage/schema.rs gains or renames a column, the script keeps producing a
database the server then misreads (or rejects) and the demo silently breaks.
This compares the column names and types of the two definitions.

Usage: python3 scripts/check-seed-schema.py        (run from the repository root)
"""
import re
import sys


def columns(sql):
    body = sql[sql.index("(") + 1 : sql.rindex(")")]
    cols = []
    for part in re.split(r",\s*(?![^()]*\))", body):  # split on commas outside parentheses
        words = part.split()
        if words:
            cols.append((words[0], words[1].upper()))
    return cols


schema = open("src/storage/schema.rs", encoding="utf-8").read()
seed = open("scripts/seed-demo-data.py", encoding="utf-8").read()

server = re.search(r"CREATE TABLE IF NOT EXISTS events \((.*?)\n\)", schema, re.S)
script = re.search(r"CREATE TABLE IF NOT EXISTS events \((.*?)\)\n\"\"\"", seed, re.S)
if not server or not script:
    sys.exit("could not find the events CREATE TABLE statement in schema.rs or seed-demo-data.py")

a = columns("(" + server.group(1) + ")")
b = columns("(" + script.group(1) + ")")
if a != b:
    print("seed-demo-data.py and src/storage/schema.rs define different events tables:")
    print("  only in schema.rs:      ", [c for c in a if c not in b])
    print("  only in the seed script:", [c for c in b if c not in a])
    sys.exit(1)
print(f"seed script matches schema.rs ({len(a)} columns)")
