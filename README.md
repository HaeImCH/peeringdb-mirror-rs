# PeeringDB Mirror on Cloudflare Workers + D1

Rust Cloudflare Worker that mirrors the public PeeringDB API using a D1 database. It fetches the published JSON dumps from `public.peeringdb.com`, stores objects per resource/id, and serves endpoints compatible with `https://www.peeringdb.com/api/...` so callers can simply change the base URL.

## What you get
- `GET /api/:resource/:id` – mirror of single-object calls, e.g. `/api/ix/3352`; 404 if the object doesn't exist or was deleted.
- `GET /api/:resource` – supports `?id=`, `?since=` (unix seconds), `?limit=` and `?skip=` (offset), plus PeeringDB field filters: equality (`?asn=44324`, `?info_never_via_route_servers=true`; text compares case-insensitively) and the `__in`, `__lt`, `__lte`, `__gt`, `__gte`, `__contains` and `__startswith` operators (e.g. `?asn__in=6939,44324`, `?name__contains=hurricane`). Filters match against the stored JSON payload; an unknown field or operator returns `{"data": []}` — the mirror never falls back to an unfiltered page. As on the official API:
  - every match is returned unless you pass `limit` (`limit=0` also means no limit) — up to 5,000 objects per response; a larger result gets a 400 asking you to page with `limit`/`skip` (see below);
  - objects deleted upstream are hidden except in `?since=` results;
  - a malformed `since`, `limit` or `skip` gets a 400, and an `?id=` that is malformed or matches nothing gets a 404, each with `meta.error` set.
- `POST /admin/sync` – trigger sync (requires `SYNC_SECRET`); add `?resource=org` (etc.) to sync a single resource (an unknown name gets a 400). Resources sync independently; if any fail, the response is a 500 listing them under `failed`, and the rest are still synced.
- `GET /health` – simple health check returning "ok".
- Scheduled sync every 3 hours via Cloudflare Cron (configure in `wrangler.toml`).
- Incremental-only sync uses `since=` against `www.peeringdb.com/api`; first run falls back to the full snapshot.
- Data stored raw as JSON for fidelity; schema defined in `migrations/`.
- Supported resources: `org`, `campus`, `fac`, `net`, `ix`, `carrier`, `carrierfac`, `ixfac`, `ixlan`, `ixpfx`, `netfac`, `netixlan`.

## Quick start
1) Install tools: `npm i -g wrangler` (build uses `worker-build 0.7.x`, installed automatically during `wrangler` build).
2) Copy the example config:
   ```bash
   cp wrangler.toml.example wrangler.toml
   ```
3) Create a D1 database and record its id:
   ```bash
   wrangler d1 create peeringdb-mirror
   ```
4) Update `wrangler.toml`:
   - Set `database_id` under `[[d1_databases]]`.
   - Adjust `name`/`crons` as desired.
5) Apply migrations:
   ```bash
   wrangler d1 migrations apply peeringdb-mirror
   # for local dev: wrangler d1 migrations apply peeringdb-mirror --local
   ```
6) Add a sync secret (required for manual sync endpoint):
   ```bash
   wrangler secret put SYNC_SECRET
   ```
   Recommended: also add a PeeringDB API key (create one in your PeeringDB profile; read-only is enough). Anonymous API access is tightly rate-limited and Workers share outbound IPs, so without a key incremental syncs regularly get HTTP 429 — they are retried, but can still fail:
   ```bash
   wrangler secret put PEERINGDB_API_KEY
   ```
7) Run locally:
   ```bash
   wrangler dev
   ```
8) Deploy:
   ```bash
   wrangler deploy
   ```

### Upgrading an existing deployment
Neither `wrangler deploy` nor Workers Builds applies migrations. When an update adds one, apply it to the remote database **before** deploying the new code:
```bash
wrangler d1 migrations apply peeringdb-mirror --remote
```
`0002_sync_state.sql` seeds each resource's sync watermark from the data already stored, so existing deployments keep syncing incrementally. Until it is applied, the new code's syncs fail with `no such table: sync_state` (reads are unaffected).

## Initial database import

The first sync via API can be slow. For faster bootstrapping, download and import the full PeeringDB dump:

1) Generate the SQL files (requires Python 3):
   ```bash
   python3 scripts/build_peeringdb_dump.py
   ```

2) Import each resource in order (this can take a while):
   ```bash
   wrangler d1 execute peeringdb-mirror --remote --file d1_sql/org.sql
   wrangler d1 execute peeringdb-mirror --remote --file d1_sql/campus.sql
   wrangler d1 execute peeringdb-mirror --remote --file d1_sql/fac.sql
   wrangler d1 execute peeringdb-mirror --remote --file d1_sql/net.sql
   wrangler d1 execute peeringdb-mirror --remote --file d1_sql/ix.sql
   wrangler d1 execute peeringdb-mirror --remote --file d1_sql/carrier.sql
   wrangler d1 execute peeringdb-mirror --remote --file d1_sql/carrierfac.sql
   wrangler d1 execute peeringdb-mirror --remote --file d1_sql/ixfac.sql
   wrangler d1 execute peeringdb-mirror --remote --file d1_sql/ixlan.sql
   wrangler d1 execute peeringdb-mirror --remote --file d1_sql/ixpfx.sql
   wrangler d1 execute peeringdb-mirror --remote --file d1_sql/netfac.sql
   wrangler d1 execute peeringdb-mirror --remote --file d1_sql/netixlan.sql
   ```

After import, the scheduled cron will keep data up to date via incremental syncs.

## API examples
- Mirror an IX record (matches `https://www.peeringdb.com/api/ix/3352`):
  ```bash
  curl https://<your-worker>/api/ix/3352
  ```
- Fetch by query:
  ```bash
  curl "https://<your-worker>/api/ix?id=3352"
  ```
- Fetch changes since a unix timestamp:
  ```bash
  curl "https://<your-worker>/api/net?since=1704067200&limit=500"
  ```
- Filter by field (e.g. resolve a network by ASN, as `pathvector` does):
  ```bash
  curl "https://<your-worker>/api/net?asn=44324"
  ```
- Trigger a manual refresh (if `SYNC_SECRET` set):
  ```bash
  curl -X POST https://<your-worker>/admin/sync \
    -H "Authorization: Bearer <your-secret>"
  ```

## How sync works
- Each resource has a watermark in the `sync_state` table, written only after that resource syncs completely.
- Without a watermark, the sync pulls the full dataset from `https://public.peeringdb.com/{resource}-0.json` (see `FETCHING_FULL_DATABASE.md`). An interrupted full sync leaves no watermark, so the next run starts it over instead of skipping the rows it never wrote.
- With a watermark, syncs fetch only changes since then using `https://www.peeringdb.com/api/{resource}?since=<unix>&limit=1000&skip=...`, authenticated with `PEERINGDB_API_KEY` when set. A throttled request (HTTP 429) is retried after the server's `Retry-After` delay, up to 5 attempts.
- To force a full re-sync of a resource, delete its row: `wrangler d1 execute peeringdb-mirror --remote --command "DELETE FROM sync_state WHERE resource = 'netixlan'"`.
- A resource whose full sync can't finish within Worker limits will retry it every run (and log the failure); bootstrap that resource with the dump import above instead.
- A failure in one resource is logged and doesn't stop the others. The cron invocation itself still reports success, so alert on the `scheduled sync failed for:` error log line.
- Resources processed in dependency-friendly order defined in `RESOURCES`.
- Each object is stored per `(resource, obj_id)` with the original JSON payload and `updated` timestamp.
- `ON CONFLICT` upserts keep data fresh; scheduled cron handles automatic refreshes.

## Notes and limitations
- `poc` (point of contact) is omitted because it requires PeeringDB authentication; add it to `RESOURCES` in `src/lib.rs` if you have access.
- `since` filtering uses SQLite `datetime(?,'unixepoch')` against the stored `updated` strings (ISO 8601).
- Objects deleted upstream are kept with `status: "deleted"` (so `?since=` can report them) but hidden from all other reads.
- Single-object responses don't include the nested `*_set` lists (e.g. `netixlan_set` on `/api/net/:id`) that the official API adds, and `depth`/`fields` are ignored.
- List responses are capped at 5,000 objects (`MAX_RESPONSE_ROWS`); this is where the official API differs. Larger results, such as all of `/api/net`, would exceed the Worker's memory/CPU limits (Cloudflare error 1102), so they get a 400 instead of being truncated. Page with `limit`/`skip`, or use the `public.peeringdb.com` snapshots for full dumps.
- If you see `no such table: objects` or `no such table: sync_state`, run the migration step above to create the schema in your D1 database.
- Build targets `workers-rs 0.7.x` and uses upstream `worker-build`/`wasm-bindgen 0.2.106` (no local patches).

## Project layout
- `src/lib.rs` – Worker entrypoint, routes, sync logic, D1 queries.
- `migrations/` – D1 schema: objects (`0001_init.sql`) and sync watermarks (`0002_sync_state.sql`).
- `wrangler.toml.example` – Example config (copy to `wrangler.toml` and set your D1 database ID).
- `scripts/build_peeringdb_dump.py` – Script to download and convert PeeringDB dump for D1.
