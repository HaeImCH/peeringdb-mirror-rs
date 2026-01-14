#!/usr/bin/env python3
"""
Fetch PeeringDB public snapshots and build a SQLite dump for D1 import.

Outputs:
- peeringdb_dump.db (local SQLite)
- peeringdb_dump.sql (iterdump; suitable for `wrangler d1 execute ... --file`)
"""

import json
import sqlite3
import sys
import urllib.request

RESOURCES = [
    "org",
    "campus",
    "fac",
    "net",
    "ix",
    "carrier",
    "carrierfac",
    "ixfac",
    "ixlan",
    "ixpfx",
    "netfac",
    "netixlan",
]

DB_PATH = "peeringdb_dump.db"
SQL_PATH = "peeringdb_dump.sql"
CDN_BASE = "https://public.peeringdb.com"
D1_DIR = "d1_sql"


def main() -> int:
    import os

    os.makedirs(D1_DIR, exist_ok=True)
    conn = sqlite3.connect(DB_PATH)
    cur = conn.cursor()
    cur.executescript(
        """
DROP TABLE IF EXISTS objects;
CREATE TABLE objects (
  resource TEXT NOT NULL,
  obj_id   INTEGER NOT NULL,
  updated  TEXT NOT NULL,
  payload  TEXT NOT NULL,
  PRIMARY KEY (resource, obj_id)
);
CREATE INDEX IF NOT EXISTS objects_resource_updated_idx
  ON objects (resource, updated DESC);
"""
    )

    for res in RESOURCES:
        url = f"{CDN_BASE}/{res}-0.json"
        with urllib.request.urlopen(url) as resp:
            data = json.load(resp)["data"]

        rows = [
            (
                res,
                obj.get("id"),
                obj.get("updated", ""),
                json.dumps(obj, separators=(",", ":")),
            )
            for obj in data
        ]
        cur.executemany(
            "INSERT OR REPLACE INTO objects (resource,obj_id,updated,payload) VALUES (?,?,?,?)",
            rows,
        )
        print(f"{res}: {len(rows)} rows", file=sys.stderr)

    conn.commit()
    with open(SQL_PATH, "w", encoding="utf-8") as f:
        for line in conn.iterdump():
            f.write(f"{line}\n")
    conn.close()
    print(f"wrote {DB_PATH} and {SQL_PATH}")

    # Write a D1-friendly SQL bundle and per-resource chunk files (no BEGIN/COMMIT/PRAGMA).
    schema_path = os.path.join(D1_DIR, "00_schema.sql")
    with open(schema_path, "w", encoding="utf-8") as f:
        f.write(
            """
CREATE TABLE IF NOT EXISTS objects (
  resource TEXT NOT NULL,
  obj_id   INTEGER NOT NULL,
  updated  TEXT NOT NULL,
  payload  TEXT NOT NULL,
  PRIMARY KEY (resource, obj_id)
);
CREATE INDEX IF NOT EXISTS objects_resource_updated_idx
  ON objects (resource, updated DESC);
"""
        )
    print(f"wrote schema file {schema_path}")

    # Emit per-resource inserts to avoid oversized single files.
    conn = sqlite3.connect(DB_PATH)
    cur = conn.cursor()
    for res in RESOURCES:
        outfile = os.path.join(D1_DIR, f"{res}.sql")
        with open(outfile, "w", encoding="utf-8") as f:
            f.write(f"DELETE FROM objects WHERE resource = '{res}';\n")
            for row in cur.execute(
                "SELECT obj_id, updated, payload FROM objects WHERE resource = ?",
                (res,),
            ):
                obj_id, updated, payload = row
                payload_escaped = payload.replace("'", "''")
                f.write(
                    f"INSERT OR REPLACE INTO objects (resource,obj_id,updated,payload) VALUES ('{res}',{obj_id},'{updated}','{payload_escaped}');\n"
                )
        print(f"wrote chunk {outfile}")
    conn.close()

    # Also keep a single filtered dump for local use.
    D1_SQL_PATH = "peeringdb_dump_d1.sql"
    skip_prefixes = (
        "BEGIN TRANSACTION",
        "COMMIT",
        "PRAGMA",
    )
    with open(SQL_PATH, "r", encoding="utf-8") as src, open(
        D1_SQL_PATH, "w", encoding="utf-8"
    ) as dst:
        for line in src:
            stripped = line.strip()
            if not stripped or stripped.startswith(skip_prefixes):
                continue
            dst.write(line)
    print(f"wrote {D1_SQL_PATH} (filtered for D1 execute; may still be large)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
