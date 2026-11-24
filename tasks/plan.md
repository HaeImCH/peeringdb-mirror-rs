# Implementation Plan: D1 Row-Read Reduction — V1

Source spec: `SPEC.md` (approved 2026-11-24)
Baseline: `origin/main` @ `9f072ce`
Tasks tracked in: `tasks/todo.md`

## Overview

Cut D1 rows read from about 6M/day to under 1M/day for pathvector's traffic (`net?asn`, `netixlan?asn`, `net?info_never_via_route_servers=1`) without changing any API response. There are three levers, in order of impact:

1. An edge response cache that expires at the sync boundary.
2. SQL that can use an index: literal resource and path, and the `json_type` branch only for boolean values.
3. Partial expression indexes on `net.asn` and `netixlan.asn`.

## Architecture Decisions

- **The cache wraps the router in `main`**, not each handler.
  - One place covers both `/api/:resource/:id` and `/api/:resource`.
  - Only `GET /api/*` responses with status 200 are stored.
  - The key is the request URL.
- **TTL comes from a pure function** `cache_ttl_secs(now_ms) = secs to next */3h UTC tick + 600`. It is unit-tested off-wasm, like `utc_hour`, and set as `Cache-Control: public, max-age=N` on the stored copy.
- **The cache ships first.** It alone turns about 9 runs per 3h into 1, needs no migration, and adds no risk to the SQL semantics.
- **The resource and field path are inlined as SQL literals.**
  - Safe because `resource ∈ RESOURCES` and `is_filterable_field` guarantees `[a-z][a-z0-9_]*`.
  - Required, because SQLite matches expression and partial indexes by literal text.
- **Partial indexes (`WHERE resource='<res>'`)** keep both the index size and the extra writes per upsert confined to `net` and `netixlan`.
- **Migrations are applied by hand.** Workers Builds deploys on push to `main` and does not run D1 migrations, so the indexes are built on the remote by hand: `0003` (net) on day 1 before the merge, and `0004` (netixlan) on day 2, to stay under 100k rows written per day. The indexes do no harm to the old code.
- **All work goes on a branch.** Merging to `main` is the deploy, and needs the user's go-ahead.

## Dependency Graph

```
T1 golden baseline (old code)
 ├── T2 response cache ──────────────┐
 └── T3 index-friendly SQL           │
      └── T4 migrations 0003-0004 + plan check
           └── T5 reconcile ORDER BY │
                └── T6 rollout (after 2026-11-25 00:00 UTC, ask first)
```

T2 and T3 are independent, but both edit `src/lib.rs`, so they are done one after the other.

## Task List

### Phase 1: Safety net + biggest win
- [x] T1: Capture golden responses from the current code
- [x] T2: Edge response cache with a sync-aligned TTL

### Checkpoint A
- [x] `cargo test` passes
- [x] Golden diff is clean
- [x] Second identical request is a cache hit in `wrangler dev`

### Phase 2: Make the remaining queries cheap
- [x] T3: Index-friendly field-filter SQL
- [x] T4: Migration `0003_net_asn_index.sql` + `0004_netixlan_asn_index.sql` + query-plan check script
- [x] T5: Reconcile scan without `ORDER BY`

### Checkpoint B
- [x] `cargo test` passes
- [x] Golden diff is clean
- [x] Plan check passes for every pathvector query
- [ ] Review with the user

### Phase 3: Rollout (after the reset, each step confirmed)
- [ ] T6: `0003` remote + merge on day 1, `0004` on day 2, observe

## Risks and Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| `worker::Cache` API in workers-rs 0.7 differs from expected, e.g. `put` needs a cloned response or `wait_until` needs a `'static` future | Med | Check the crate source in T2 first. Fallback: `await` the put inline, which costs a few ms on a miss only. |
| Cache on a custom domain but not `workers.dev` | Low | Pathvector uses `peeringdb-mirror.thisisch.net`. On `workers.dev` the code does nothing and responses come straight from D1, so they are still correct. |
| Inlined SQL changes results subtly, e.g. dropping the `json_type` branch | High | Golden diff in T1 covers boolean, `__in`, case-insensitive name, `since`, paging and 404s. Existing `plan_*` tests stay. |
| Planner ignores the index on D1 (no stats) | Med | Index includes `obj_id` so no sort is needed. This was verified without stats locally. T6 checks D1 insights. |
| Index build costs reads or writes on a day close to the limit | Low | About 96k reads plus about 96k index writes, run right after the reset. |
| Merge to `main` deploys before the migration exists | Low | T6 order: migration first, then merge. The old code is unaffected by the indexes. |
| Stale cache after a manual `/admin/sync` | Low | Accepted. It is bounded by the TTL, which is ≤ 3h10m, the same staleness the mirror already has. |

## Open Questions

- None.
