# PeeringDB Mirror on Cloudflare Workers + D1

Rust Cloudflare Worker that mirrors the public PeeringDB API using a D1 database. It fetches the published JSON dumps from `public.peeringdb.com`, stores objects per resource/id, and serves endpoints compatible with `https://www.peeringdb.com/api/...` so callers can simply change the base URL.

## What you get
- `GET /api/:resource/:id` – mirror of single-object calls, e.g. `/api/ix/3352`.
- `GET /api/:resource` – supports `?id=`, `?since=` (unix seconds), `?limit=` (default 250).
- `POST /admin/sync` – trigger sync (requires `SYNC_SECRET`); add `?resource=org` (etc.) to sync a single resource.
- `GET /health` – simple health check returning "ok".
- Scheduled sync every 3 hours via Cloudflare Cron (configure in `wrangler.toml`).
- Incremental-only sync uses `since=` against `www.peeringdb.com/api`; first run falls back to the full snapshot.
- Data stored raw as JSON for fidelity; schema defined in `migrations/0001_init.sql`.
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
7) Run locally:
   ```bash
   wrangler dev
   ```
8) Deploy:
   ```bash
   wrangler deploy
   ```

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
- Trigger a manual refresh (if `SYNC_SECRET` set):
  ```bash
  curl -X POST https://<your-worker>/admin/sync \
    -H "Authorization: Bearer <your-secret>"
  ```

## How sync works
- First sync (or fallback) pulls the full dataset from `https://public.peeringdb.com/{resource}-0.json` (see `FETCHING_FULL_DATABASE.md`).
- Subsequent syncs fetch only changes since the last `updated` using `https://www.peeringdb.com/api/{resource}?since=<unix>&limit=1000&skip=...`.
- Resources processed in dependency-friendly order defined in `RESOURCES`.
- Each object is stored per `(resource, obj_id)` with the original JSON payload and `updated` timestamp.
- `ON CONFLICT` upserts keep data fresh; scheduled cron handles automatic refreshes.

## Notes and limitations
- `poc` (point of contact) is omitted because it requires PeeringDB authentication; add it to `RESOURCES` in `src/lib.rs` if you have access.
- `since` filtering uses SQLite `datetime(?,'unixepoch')` against the stored `updated` strings (ISO 8601).
- Deletions upstream are not explicitly purged; the upstream `status` field remains available to mark records.
- If you see `no such table: objects`, run the migration step above to create the schema in your D1 database.
- Build targets `workers-rs 0.7.x` and uses upstream `worker-build`/`wasm-bindgen 0.2.106` (no local patches).

## Project layout
- `src/lib.rs` – Worker entrypoint, routes, sync logic, D1 queries.
- `migrations/0001_init.sql` – D1 schema.
- `wrangler.toml.example` – Example config (copy to `wrangler.toml` and set your D1 database ID).
- `scripts/build_peeringdb_dump.py` – Script to download and convert PeeringDB dump for D1.
