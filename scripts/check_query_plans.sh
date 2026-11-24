#!/usr/bin/env bash
#
# Proves the list queries pathvector sends use the migrations/0003-0004 indexes.
# Applies every migration to a throwaway copy of a SQLite DB (no planner
# stats, like a fresh D1) and checks EXPLAIN QUERY PLAN for each query: it
# must name the expected index and need no temp sort.
#
#   scripts/check_query_plans.sh [path/to/db]   # default: peeringdb_dump.db
#
# `__in` filters are deliberately absent: `x COLLATE NOCASE IN (...)` never
# matches the index (the collation sits on the indexed side), and moving it
# onto the values changes which rows match. They stay correct but scan, as
# before; pathvector doesn't send them.
#
# The SQL below is what build_query_plan emits for these requests; the
# plan_* unit tests in src/lib.rs pin the builder to exactly these strings.
set -euo pipefail

here="$(cd "$(dirname "$0")/.." && pwd)"
src_db="${1:-$here/peeringdb_dump.db}"
[ -f "$src_db" ] || { echo "no database at $src_db" >&2; exit 2; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cp "$src_db" "$work/plan.db"
db="$work/plan.db"
sqlite3 "$db" "DROP TABLE IF EXISTS sqlite_stat1; DROP TABLE IF EXISTS sqlite_stat4;"
for m in "$here"/migrations/*.sql; do
  sqlite3 "$db" <"$m"
done

NOT_DELETED="json_extract(payload, '\$.status') IS NOT 'deleted'"
fail=0

# check <label> <expected index> <sql>
check() {
  local label="$1" index="$2" sql="$3" plan
  plan="$(sqlite3 "$db" "EXPLAIN QUERY PLAN $sql")"
  if grep -q "USING INDEX $index " <<<"$plan" && ! grep -q "TEMP B-TREE" <<<"$plan"; then
    echo "ok    $label -> $index"
  else
    echo "FAIL  $label (want $index, no TEMP B-TREE):" >&2
    sed 's/^/        /' <<<"$plan" >&2
    fail=1
  fi
}

for res in net netixlan; do
  check "/api/$res?asn=13335" "idx_${res}_asn" \
    "SELECT payload FROM objects WHERE resource = '$res' AND $NOT_DELETED \
     AND CAST(json_extract(payload, '\$.asn') AS TEXT) = '13335' COLLATE NOCASE \
     ORDER BY obj_id LIMIT 5001"
done
check "/api/netixlan?asn=13335&limit=5&skip=2" "idx_netixlan_asn" \
  "SELECT payload FROM objects WHERE resource = 'netixlan' AND $NOT_DELETED \
   AND CAST(json_extract(payload, '\$.asn') AS TEXT) = '13335' COLLATE NOCASE \
   ORDER BY obj_id LIMIT 5 OFFSET 2"

# Same results as the pre-0003 full scan, for a few real ASNs.
for asn in 13335 15169 6939 8075 99999999; do
  for res in net netixlan; do
    indexed="$(sqlite3 "$db" "SELECT group_concat(obj_id) FROM (SELECT obj_id FROM objects INDEXED BY idx_${res}_asn WHERE resource = '$res' AND CAST(json_extract(payload, '\$.asn') AS TEXT) = '$asn' COLLATE NOCASE ORDER BY obj_id)")"
    scanned="$(sqlite3 "$db" "SELECT group_concat(obj_id) FROM (SELECT obj_id FROM objects NOT INDEXED WHERE resource = '$res' AND CAST(json_extract(payload, '\$.asn') AS TEXT) = '$asn' COLLATE NOCASE ORDER BY obj_id)")"
    if [ "$indexed" != "$scanned" ]; then
      echo "FAIL  $res asn=$asn: index and scan disagree" >&2
      fail=1
    fi
  done
done
[ "$fail" -eq 0 ] && echo "ok    index results equal full-scan results"

exit "$fail"
