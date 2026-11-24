-- Indexes for the field filters pathvector sends (`net?asn=`, `netixlan?asn=`).
-- Without them every such list call scans the whole resource (~37k rows read
-- to return one), which exhausted D1's free 5M rows-read/day.
--
-- The indexed expression must match the SQL `push_field_filter` builds
-- character for character — `CAST(json_extract(payload, '$.<field>') AS TEXT)`
-- compared `COLLATE NOCASE` — and the resource must be a literal, or SQLite
-- falls back to a scan. `obj_id` last serves `ORDER BY obj_id` without a sort.
-- Partial (`WHERE resource = ...`) so each index only holds, and upserts only
-- maintain, that resource's rows. Check with scripts/check_query_plans.sh.
--
-- One index per migration: building one writes a row per indexed object
-- (net ~34k, netixlan ~62k), and the free tier allows 100k rows written per
-- day, so they are built on different days. See tasks/todo.md Task 6.

CREATE INDEX IF NOT EXISTS idx_netixlan_asn
    ON objects (CAST(json_extract(payload, '$.asn') AS TEXT) COLLATE NOCASE, obj_id)
    WHERE resource = 'netixlan';
