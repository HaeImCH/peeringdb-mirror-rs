# Spec: D1 Row-Read Reduction — V1

Status: approved (2026-11-24)
Code baseline: `origin/main` @ `9f072ce` ("reconcile deletions that since syncs never report")

## Objective

Cloudflare blocked D1 reads for the account after it went past the free tier's **5,000,000 rows read/day** (resets 2026-11-25 00:00 UTC). From D1 query insights:

| Query (source) | Calls | Rows read | Rows read per row returned | Runtime |
|---|---|---|---|---|
| List with field filter (`build_query_plan` → `push_field_filter`, `src/lib.rs:353`) | 156 | 5.77M | **~36,960** | 91.5% |
| Reconcile live-ID scan (`reconcile_resource`, `src/lib.rs:744`) | 17 | 492k | 2 | 7.9% |
| Upsert (`upsert_objects`) | 480 | 359 | — | 0.6% |

Goal: list queries with a field filter read roughly what they return, and normal daily traffic stays far below the limit. **API behaviour must not change**: same rows, same order, same errors.

**Traffic (confirmed):** the only client is [pathvector](https://github.com/natesales/pathvector/blob/b11cd26a25ef7aa4cba79a83fe838bcbd40bfb18/pkg/peeringdb/peeringdb.go). Each run calls:
- `GET /api/net?asn=<ASN>` once per peer (`networkInfo`, reads `data[0]`)
- `GET /api/netixlan?asn=<ASN>` (`IXLANs`)
- `GET /api/net?info_never_via_route_servers=1` (`NeverViaRouteServers`, uses every row)

Workers Logs show the same 12 ASNs requested about 9 times in 36 minutes. Pathvector only caches in memory within a single process, so each run asks again. The mirror's data only changes at the 3-hourly sync (cron `0 */3 * * *`), so almost every one of these calls is a repeat.

## Root Cause

Checked with `EXPLAIN QUERY PLAN` on a copy of `peeringdb_dump.db` (207k rows, no planner stats). The `Eq` filter today is:

```sql
AND (json_type(payload, ?2) = ?3 OR CAST(json_extract(payload, ?4) AS TEXT) = ?5 COLLATE NOCASE)
```

Four problems:

1. **The JSON path is a bind parameter (`?4`).** SQLite uses an expression index only when the query's expression is the same text as the indexed one, so a path bound at runtime can never use an index.
2. **The OR with `json_type`** turns any index use into a scan. That branch only matters for boolean values (`1/0/true/false`). For anything else, e.g. `?asn=13335`, it compares against `''`, which never matches, yet it is still emitted.
3. **There are no indexes on any payload field.** Every filtered call walks the primary key over the whole resource: `SEARCH ... sqlite_autoindex_objects_1 (resource=?)`. For `net` (34k rows) or `netixlan` (62k) that is about 37k rows read to return about 1.
4. **The resource is bound (`?1`)**, which rules out partial indexes. A `WHERE resource='net'` index needs a literal.

The reconcile scan has to read every live ID by design, because it diffs them against upstream's ID list. It costs about 2 reads per row (index entry plus table row for the `status` check), and `ORDER BY obj_id` adds a temp sort the code doesn't need.

## Verified Fix Shape

Tested on the local copy without planner stats:

```sql
-- index (migration)
CREATE INDEX idx_net_asn ON objects(CAST(json_extract(payload,'$.asn') AS TEXT) COLLATE NOCASE, obj_id)
  WHERE resource = 'net';

-- query (resource + path inlined as literals, value still bound)
SELECT payload FROM objects WHERE resource = 'net'
  AND json_extract(payload, '$.status') IS NOT 'deleted'
  AND CAST(json_extract(payload, '$.asn') AS TEXT) = ?1 COLLATE NOCASE
  ORDER BY obj_id LIMIT ?2;
-- plan: SEARCH objects USING INDEX idx_net_asn (<expr>=?)   (no TEMP B-TREE)
```

This returns the same rows as the old SQL; for `asn=13335` both give 1 row. If the resource is bound instead of literal, the plan falls back to the primary-key scan.

## New Behavior

1. **Inline the resource and path in `build_query_plan`/`push_field_filter`.**
   - The resource is written as `resource = '<res>'`. It is already checked against `RESOURCES`.
   - The path is written as `'$.<field>'`. The field already passes `is_filterable_field` (`[a-z][a-z0-9_]*`), so it can't contain a quote.
   - Values stay bound.
   - The `?N` numbering shifts, and the exact-SQL unit tests (`plan_*`, `src/lib.rs:1158+`) are updated to match.
2. **Emit the `json_type` branch only when the value has a boolean reading** (`1/0/true/false`). For every other value the SQL becomes the plain `CAST(...) = ? COLLATE NOCASE`, which can use an index. The results are the same, because the branch currently compares against `''` and never matches.
3. **Apply the same inlining to `__in`**: `CAST(json_extract(payload,'$.f') AS TEXT) COLLATE NOCASE IN (...)`, which can use the same index.
4. **Migration `0003_net_asn_index.sql` + `0004_netixlan_asn_index.sql`** adds one partial index for each hot `(resource, field)` pair:

   ```sql
   CREATE INDEX IF NOT EXISTS idx_<res>_<field> ON objects(CAST(json_extract(payload,'$.<field>') AS TEXT) COLLATE NOCASE, obj_id) WHERE resource = '<res>';
   ```

   The index set covers exactly the filters pathvector sends:

   | resource | fields |
   |---|---|
   | `net` | `asn` |
   | `netixlan` | `asn` |

   Building them reads about 96k rows (the `net` and `netixlan` rows).

   Because each index is partial, it holds only that resource's rows. An upsert writes one extra index row only for indexes on its own resource.
5. **Edge response cache (Workers Cache API, `caches.default`)** for `GET /api/*`:
   - **Where:** in Cloudflare's cache, in the data center that serves the request. It is not global and isn't stored in D1 or KV. It works because the worker runs on a custom domain (`peeringdb-mirror.thisisch.net`). The Cache API does nothing on `*.workers.dev`. It is free and not subject to D1 or KV limits. Pathvector always runs from the same place, so its requests reach the same data center and hit the same cache.
   - **Key:** the full request URL. Pathvector's `Authorization` header doesn't vary the response, so it is ignored.
   - **TTL:** expire at the next sync boundary, i.e. `max-age = seconds until the next 3h cron tick + 10 min`, which allows for the sync to finish. A cached response can therefore never be staler than the mirror would be anyway.
   - **What is cached:** only `200` responses. Errors (400/404/5xx) are not cached. `/admin/*` and `/health` are never cached.
   - **On hit:** D1 isn't queried. On miss, the response is stored with `ctx.wait_until(cache.put(...))`.
   - **Effect:** in each 3h window, one D1 query per distinct URL. That covers the `info_never_via_route_servers=1` scan, which no index can fix.
6. **Reconcile scan: drop `ORDER BY obj_id`.** The result is only diffed against a `HashSet`. With the same IDs, `reconcile_candidates` must return the same output, since `missing` keeps today's order once it is sorted in Rust before `truncate`.

## Success Criteria

- `EXPLAIN QUERY PLAN` for each indexed `(resource, field)` filter with `Eq` shows `USING INDEX idx_<res>_<field>`, with no `TEMP B-TREE`. Checked with `scripts/check_query_plans.sh`.
  - *Changed during implementation:* `__in` can't use these indexes without changing its case-insensitive results (verified), so it stays a scan, exactly as before. Pathvector doesn't send `__in`.
- In D1 query insights after deploy, rows read per row returned for indexed field filters is **≤ 3**. Today it is about 37,000.
- A repeated request inside the same 3h window is served from cache. `cf-cache-status`/`age` show this, or a log line on hit, and D1 query insights show no new query for it.
- Daily rows read in the D1 dashboard is **< 1M**, which is 20% of the limit, over 24h of normal traffic. Reconcile, at roughly 0.4–0.5M/day, becomes the main cost.
- **Must not change:**
  - All existing `cargo test` cases pass. Their expected SQL strings are updated, but they assert the same filter semantics.
  - JSON bodies are byte-identical, old code vs new code on the local DB, for a fixed set of sample requests: `/api/net?asn=13335`, `/api/netixlan?asn=13335`, `/api/netixlan?net_id=694&limit=10&skip=5`, `/api/net?name=cloudflare`, `/api/net?asn__in=13335,15169`, `/api/net?info_never_via_route_servers=1`, `/api/net?since=1700000000`, `/api/ix/1`, `/api/net?name__contains=hurricane`.
- Unindexed fields keep working, slowly, exactly as today.

## Boundaries (in addition to the repo's normal rules)

- **Always:**
  - Only allowlisted resources and regex-checked field names are written into SQL; values are always bound.
  - Check new SQL with `EXPLAIN QUERY PLAN` locally before deploying.
- **Ask first:**
  - Applying `0003`/`0004` to the remote D1. Each `CREATE INDEX` reads its resource's rows once and writes one row per index entry (net ~34k, netixlan ~62k), so they go on separate days to stay under 100k writes/day.
  - Deploying.
  - Changing the index set.
- **Never:**
  - Touch the remote DB before the 2026-11-25 00:00 UTC reset.
  - Edit `0001`/`0002` in place.
  - Change response semantics, such as dropping case-insensitive matching or the boolean handling.

## Commands

```
Test:        cd peeringdb-mirror-rs && cargo test
Build:       worker-build --release
Local D1:    wrangler d1 migrations apply peeringdb-mirror --local
Local run:   wrangler dev --local
Plan check:  sqlite3 <copy of peeringdb_dump.db> "EXPLAIN QUERY PLAN <sql>"
Remote:      wrangler d1 migrations apply peeringdb-mirror --remote   # ask first
Deploy:      wrangler deploy (or CI deploy script)                     # ask first
```

## Testing Strategy

- **Unit (`cargo test`):** extend the `plan_*` tests.
  - Literal resource and path appear in the SQL.
  - The `json_type` branch appears only for boolean values.
  - A field that fails the regex is still dropped.
  - Placeholder numbering is correct.
- **Cache:** unit-test the TTL calculation (seconds to the next `*/3h` tick + margin) for times just before and just after a tick. Check the second identical request in `wrangler dev` is a hit.
- **Plan check:** a script that applies all migrations to a DB copy and asserts the index is used for every pair in the table.
- **Golden responses:** capture the sample requests on old vs new code with `wrangler dev --local`, then `diff`.

## Out of Scope (Deferred)

- Making `?since=` sargable. `datetime(updated) > datetime(?, 'unixepoch')` scans the whole resource; it could compare against a precomputed ISO string instead. It isn't in today's top queries.
- A covering partial index for reconcile, `(resource, updated, obj_id) WHERE status IS NOT 'deleted'`, to bring it to 1 read per row. That would halve its cost but adds write overhead.
- Indexes for `__lt/__gt/__contains/__startswith` and for boolean filters. These are low selectivity or can't use an index; the cache covers `info_never_via_route_servers`.
- Indexes for other fields (`net_id`, `ix_id`, `org_id`, ...): add them if a client other than pathvector shows up in the logs.
- Purging the cache when a sync finishes. Expiring at the sync boundary makes that unnecessary.
- Moving to the D1 paid plan.
- Protection against expensive public queries. Decided 2026-11-24: the cache is enough for now, and the risk is accepted. Unindexed filters, `__contains`/`__in`, `?since=`, large `skip`, and junk params that bypass the cache can still scan a whole resource (~34k–62k rows each), so roughly 65–120 such requests a day would exhaust the 5M/day reads. Options if it recurs:
  - Workers Paid ($5/month)
  - a WAF rate-limit rule on `/api/*`
  - refusing unindexed shapes for anonymous clients
  - canonical cache keys

## Resolved Questions

1. Filter usage: pathvector only, using `net?asn`, `netixlan?asn` and `net?info_never_via_route_servers=1` (confirmed in Workers Logs).
2. Reconcile frequency: cron every 3h, and reconcile runs only on the 00:00 UTC tick, so 12 scans/day (one per resource). The 17 seen on 2026-11-24 includes manual `/admin/reconcile` calls. The frequency stays as is.
3. Cache: Workers Cache API in the serving data center, expiring at the sync boundary (New Behavior 5). It is part of V1.

## Open Questions

- None blocking.
