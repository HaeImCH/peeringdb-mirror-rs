# Tasks: D1 Row-Read Reduction — V1

Spec: `SPEC.md` · Plan: `tasks/plan.md` · Branch: `perf/d1-row-reads`

## Task 1: Capture golden responses from the current code

**Description:** Before any change, record the exact responses of the current worker for a fixed set of requests against the local D1, so every later task can prove nothing changed.

**Acceptance criteria:**
- [x] `scripts/golden.sh capture|diff` hits `wrangler dev --local` for these requests and stores status + body under `tasks/golden/` (gitignored):
  - `net?asn=13335`
  - `net?asn=99999999` (no match)
  - `netixlan?asn=13335`
  - `net?info_never_via_route_servers=1`
  - `net?asn__in=13335,15169`
  - `net?name=cloudflare`
  - `net?name__contains=hurricane`
  - `netixlan?asn=13335&limit=5&skip=2`
  - `net?since=1700000000&limit=50`
  - `net/1`
  - `net/abc`
  - `net?limit=x`
- [x] Local D1 has data (migrations 0001–0002 applied plus a dump import) and the baseline is captured on `9f072ce`.

**Verification:**
- [x] Running `capture` twice and then `diff` shows no differences (the baseline is stable)

**Dependencies:** None
**Files:** `scripts/golden.sh`, `.gitignore`
**Scope:** S

## Task 2: Edge response cache with a sync-aligned TTL

**Description:** In `main`, for `GET /api/*`, look up `Cache::default()` by URL. On a hit, return the cached response. On a miss, run the router; if the status is 200, store a copy with `Cache-Control: public, max-age=cache_ttl_secs(Date::now())`.

**Acceptance criteria:**
- [x] `cache_ttl_secs` returns the seconds until the next `*/3h` UTC tick + 600 has passed (refined: a response cached inside the 10-minute settle window expires when that window ends, not 3h later). Unit-tested at the boundaries.
- [x] Non-200 responses, `/admin/*`, `/health` and non-GET requests are never cached.
- [x] A second identical request does not query D1. Shown by a log line on hit and by `age`/`cf-cache-status`.

**Verification:**
- [x] `cargo test`
- [x] `worker-build --release` succeeds
- [x] `scripts/golden.sh diff` is clean
- [x] Manual: `curl` the same URL twice under `wrangler dev`; the second call is a hit

**Dependencies:** T1
**Files:** `src/lib.rs`
**Scope:** S

## Checkpoint A
- [x] Tests pass, the build is clean, the golden diff is clean, the cache hit is confirmed

## Task 3: Index-friendly field-filter SQL

**Description:**
- In `build_query_plan`/`push_field_filter`, write `resource = '<res>'` and `'$.<field>'` as literals instead of binds.
- For `Eq`, emit `CAST(json_extract(payload,'$.f') AS TEXT) = ?N COLLATE NOCASE`. Wrap it in the `json_type(...) = ?` OR only when the value reads as a boolean (`1/0/true/false`).
- Apply the same inlining to `__in`, the range operators and `__contains`/`__startswith`.

**Acceptance criteria:**
- [x] `plan_*` tests are updated to the new SQL and placeholder numbering. New tests cover: no `json_type` for `asn=13335`, `json_type` present for `=1`, `true` and `False`, and a malformed field key still dropped.
- [x] No request-derived text reaches SQL except a `RESOURCES` member and a field that passes `is_filterable_field`. A comment says why.

**Verification:**
- [x] `cargo test`
- [x] `scripts/golden.sh diff` is clean

**Dependencies:** T1
**Files:** `src/lib.rs`
**Scope:** S

## Task 4: Migration 0003 + query-plan check

**Description:** Add the `net.asn` and `netixlan.asn` partial indexes, plus a script that proves the generated SQL uses them.

**Acceptance criteria:**
- [x] `migrations/0003_filter_indexes.sql` creates:
  - `idx_net_asn`: `objects(CAST(json_extract(payload,'$.asn') AS TEXT) COLLATE NOCASE, obj_id) WHERE resource='net'`
  - `idx_netixlan_asn`: the same expression, `WHERE resource='netixlan'`
- [x] `scripts/check_query_plans.sh` copies `peeringdb_dump.db`, applies `0003` and runs `EXPLAIN QUERY PLAN` on the exact SQL T3 produces for `net?asn=` and `netixlan?asn=`, with and without paging. *`__in` was dropped: it can't use the index without changing its case-insensitive results, so it stays a scan as before (see SPEC.md).* It fails if the plan lacks `USING INDEX idx_` or contains `TEMP B-TREE`.

**Verification:**
- [x] `bash scripts/check_query_plans.sh` exits 0
- [x] `wrangler d1 migrations apply peeringdb-mirror --local`
- [x] `scripts/golden.sh diff` is clean

**Dependencies:** T3
**Files:** `migrations/0003_filter_indexes.sql`, `scripts/check_query_plans.sh`
**Scope:** S

## Task 5: Reconcile scan without ORDER BY

**Description:** Drop `ORDER BY obj_id` from the live-ID query in `reconcile_resource` and sort `live` in Rust, so `reconcile_candidates`' truncation order is unchanged.

**Acceptance criteria:**
- [x] The SQL has no `ORDER BY`, and `live.sort_unstable()` runs before `reconcile_candidates`.
- [x] The existing `reconcile_*` tests pass.

**Verification:**
- [x] `cargo test`
- [x] EXPLAIN on the scan shows no `TEMP B-TREE`

**Dependencies:** None
**Files:** `src/lib.rs`
**Scope:** XS

## Checkpoint B
- [x] `cargo test` and `worker-build --release` pass
- [x] Golden diff is clean, the plan check passes
- [x] Review the branch with the user before rollout

## Task 6: Rollout (after 2026-11-25 00:00 UTC; each step asks first)

**Description:** Build the indexes remotely over two days, staying under the free tier's 100k rows written per day. Then deploy the code and confirm the effect.

**Why two days:** D1 counts every index entry as a written row. Building `idx_netixlan_asn` writes about 62k rows and `idx_net_asn` about 34k. Together, plus the day's sync and reconcile writes, that is too close to 100k. If the write limit trips partway, syncs fail until the next reset.

**Steps:**
1. **Day 1, pre-check (done 2026-11-25; only 0003 and 0004 were pending):** run `wrangler d1 migrations list peeringdb-mirror --remote`. Only `0003` and `0004` should be pending. If `0002` shows up, stop: its `MAX(updated) … GROUP BY` reads the whole table.
2. **Day 1:** run `wrangler d1 execute peeringdb-mirror --remote --file migrations/0003_net_asn_index.sql`. This builds `idx_net_asn`. *Done 2026-11-25. Measured: 272,200 rows read (a partial index build still scans the whole table) and 36,980 written. On the remote, `net?asn=13335` now reads 2 rows instead of ~37k.*
3. **Day 1 (done; plan shows `SEARCH objects USING INDEX idx_net_asn (<expr>=?)`):** run `wrangler d1 execute peeringdb-mirror --remote --command "EXPLAIN QUERY PLAN <net asn SQL from scripts/check_query_plans.sh>"`. Confirm `USING INDEX idx_net_asn` on the real D1 engine, not just the local sqlite3.
4. **Day 1:** push the branch and merge to `main`, which deploys through Workers Builds. The cache and the new SQL go live, and `net?asn` is indexed from then on.
5. **Day 2 (after 00:00 UTC):** `wrangler d1 execute ... --file migrations/0004_netixlan_asn_index.sql`. Expect about 272k reads and about 63k writes. Then run the same EXPLAIN check for `netixlan`. *Done 2026-11-26. Measured: 309,152 rows read and 73,488 written. The plan shows `SEARCH objects USING INDEX idx_netixlan_asn (<expr>=?)`, and `netixlan?asn=13335` reads 436 rows for 418 returned (was ~62k).*
6. **Day 2:** run `wrangler d1 migrations apply peeringdb-mirror --remote`. This records `0003` and `0004` as applied; both are `IF NOT EXISTS`, so nothing is rebuilt. *Done 2026-11-26; `migrations list` reports nothing pending.*

**Acceptance criteria:**
- [x] Both indexes exist remotely, and EXPLAIN on remote D1 shows each one being used
- [x] After one full pathvector cycle, D1 query insights show rows read per row returned ≤ 3 for the `asn` filters and cache hits for repeat runs *(`wrangler d1 insights`, 1d to 2026-11-27 13:07 UTC: the `net` asn query ran 13 times at 2 rows read each, down from 156 runs at ~37k each. No `netixlan` asn query reached D1 in that window. The top reader is now the reconcile scan: 25,642 rows over 9 runs.)*
- [x] After 24h, daily rows read is under 1M and daily rows written is well under 100k *(first 24h after the deploy, 2026-11-26 12:13 UTC: 235,805 read and 1,736 written, with only `net.asn` indexed. With both indexes, 24h to 2026-11-27 13:07 UTC: 235,617 read and 1,103 written.)*

**Verification:**
- [x] Manual: D1 row metrics and query insights (via `wrangler d1 info` / `wrangler d1 insights`)

**Dependencies:** Checkpoint B
**Files:** none (operational)
**Scope:** XS
