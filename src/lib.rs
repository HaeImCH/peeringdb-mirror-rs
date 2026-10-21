use serde::{Deserialize, Serialize};
use js_sys::Date;
use std::time::Duration;
use serde_json::{json, Value};
use wasm_bindgen::{prelude::wasm_bindgen, JsValue};
use worker::*;

const PUBLIC_BASE: &str = "https://public.peeringdb.com";
// Order matters: parents before children to satisfy FKs during initial full sync.
const RESOURCES: &[&str] = &[
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
];
const BATCH_SIZE: usize = 50;
const SYNC_PAGE_SIZE: i64 = 1000;
/// Tries per PeeringDB request when it answers 429 (throttled).
const MAX_FETCH_ATTEMPTS: u32 = 5;
/// Most rows one list response may carry. Bigger results overrun the Worker's
/// memory/CPU limits (Cloudflare error 1102; ~1 KB `net` rows already fail
/// between 10k and 15k), so they get a 400 asking the client to page instead.
const MAX_RESPONSE_ROWS: usize = 5000;
const USER_AGENT: &str = concat!(
    "peeringdb-mirror/",
    env!("CARGO_PKG_VERSION"),
    " (contact: telegram @haeimch)"
);

#[derive(Deserialize)]
struct ApiResponse {
    data: Vec<Value>,
}

#[derive(Deserialize)]
struct PayloadRow {
    payload: String,
}

#[derive(Serialize)]
struct SyncReport {
    resource: String,
    imported: usize,
}

#[derive(Serialize)]
struct SyncFailure {
    resource: String,
    error: String,
}

#[derive(Serialize)]
struct SyncSummary {
    synced: Vec<SyncReport>,
    failed: Vec<SyncFailure>,
}

#[derive(Deserialize)]
struct TsRow {
    ts: Option<String>,
}

#[derive(Deserialize)]
struct SinceRow {
    since: i64,
}

/// SQL predicate hiding soft-deleted objects. `since` syncs from PeeringDB
/// return rows with status "deleted"; we keep them so watermarks and `?since=`
/// work, but the official API never serves them otherwise. `IS NOT` (rather
/// than `!=`) keeps rows that have no status at all.
const NOT_DELETED: &str = "json_extract(payload, '$.status') IS NOT 'deleted'";

#[event(fetch, respond_with_errors)]
pub async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    Router::new()
        .get_async("/api/:resource/:id", |_, ctx| async move { get_by_id(ctx).await })
        .get_async("/api/:resource", |req, ctx| async move { query_resource(req, ctx).await })
        .get("/health", |_, _| Response::ok("ok"))
        .post_async("/admin/sync", |req, ctx| async move { run_sync(req, ctx).await })
        .run(req, env)
        .await
}

#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    let summary = sync_all(&env, RESOURCES).await;
    if !summary.failed.is_empty() {
        let failed: Vec<&str> = summary.failed.iter().map(|f| f.resource.as_str()).collect();
        console_error!("scheduled sync failed for: {}", failed.join(", "));
    }
}

async fn get_by_id(ctx: RouteContext<()>) -> Result<Response> {
    let resource = ctx
        .param("resource")
        .ok_or_else(|| Error::RustError("resource missing".into()))?;
    if !RESOURCES.contains(&resource.as_str()) {
        return Response::error("unknown resource", 400);
    }
    let id_raw = ctx
        .param("id")
        .ok_or_else(|| Error::RustError("id missing".into()))?;
    // Not-found statuses and messages follow the official API.
    let Ok(id) = id_raw.parse::<i64>() else {
        return error_response(404, "Not found.");
    };

    let db = ctx.env.d1("PEERINGDB")?;
    // A deleted object is answered exactly like a missing one.
    let statement = db.prepare(&format!(
        "SELECT payload FROM objects WHERE resource = ?1 AND obj_id = ?2 AND {}",
        NOT_DELETED
    ));
    let query = statement.bind(&[
        JsValue::from_str(&resource),
        JsValue::from_f64(id as f64),
    ])?;
    let row = query.first::<PayloadRow>(None).await?;

    if let Some(row) = row {
        let mut payload: Value = serde_json::from_str(&row.payload)?;
        // Normalize on read too, so rows ingested before the fix (or not yet
        // re-synced) are still served clean to the client.
        normalize_in_place(&mut payload);
        json_response(vec![payload])
    } else {
        error_response(404, "No object matches the given query.")
    }
}

async fn query_resource(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let resource = ctx
        .param("resource")
        .ok_or_else(|| Error::RustError("resource missing".into()))?;
    if !RESOURCES.contains(&resource.as_str()) {
        return Response::error("unknown resource", 400);
    }
    let url = req.url()?;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let plan = match build_query_plan(resource, &pairs) {
        Ok(plan) => plan,
        Err(err) => return error_response(err.status, err.message),
    };

    let db = ctx.env.d1("PEERINGDB")?;
    let bindings: Vec<JsValue> = plan
        .binds
        .iter()
        .map(|b| match b {
            Bind::Num(n) => JsValue::from_f64(*n),
            Bind::Text(s) => JsValue::from_str(s),
        })
        .collect();

    let statement = db.prepare(&plan.sql);
    let query = statement.bind(&bindings)?;
    let result = query.all().await?;
    let rows: Vec<PayloadRow> = result.results()?;
    // The plan fetches one row past the cap, so this means "too many to serve".
    if rows.len() > MAX_RESPONSE_ROWS {
        return error_response(
            400,
            &format!(
                "More than {} objects match; page with limit and skip (at most {} per request)",
                MAX_RESPONSE_ROWS, MAX_RESPONSE_ROWS
            ),
        );
    }
    let payloads: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            serde_json::from_str::<Value>(&row.payload).map(|mut v| {
                normalize_in_place(&mut v);
                v
            })
        })
        .collect::<std::result::Result<_, _>>()?;

    // Upstream answers an `?id=` lookup that finds nothing with a 404.
    if plan.by_id && payloads.is_empty() {
        return error_response(404, "Entity not found");
    }
    json_response(payloads)
}

/// A SQL bind value, kept independent of `JsValue` so query planning stays
/// pure and unit-testable off-wasm.
#[derive(Debug, PartialEq)]
enum Bind {
    Num(f64),
    Text(String),
}

struct QueryPlan {
    sql: String,
    binds: Vec<Bind>,
    /// Whether the request looked up `?id=`; upstream 404s when that finds nothing.
    by_id: bool,
}

/// A request the official API rejects, with the status and error message it
/// answers with.
#[derive(Debug, PartialEq)]
struct ApiError {
    status: u16,
    message: &'static str,
}

/// Reserved query params that shape the response or are handled as dedicated
/// columns — never as JSON field filters. `depth`/`fields`/`q`/`format` are
/// PeeringDB response-shaping/search knobs the mirror doesn't model; we ignore
/// them rather than mistaking them for field filters (which would wrongly
/// return an empty set).
fn is_reserved_param(key: &str) -> bool {
    matches!(
        key,
        "id" | "since" | "limit" | "skip" | "depth" | "fields" | "q" | "format"
    )
}

/// PeeringDB field names are lowercase snake_case alphanumerics (e.g. `asn`,
/// `irr_as_set`, `info_prefixes4`). Restrict filter keys to that shape so the
/// `$.<field>` JSON path we build is always well-formed; anything else is
/// dropped. Keys may carry an operator suffix (`asn__in`), see `FilterOp`.
fn is_filterable_field(key: &str) -> bool {
    let mut bytes = key.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_lowercase() => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Filter operators PeeringDB accepts as a `field__op` suffix.
#[derive(Clone, Copy, Debug, PartialEq)]
enum FilterOp {
    Eq,
    In,
    Lt,
    Lte,
    Gt,
    Gte,
    Contains,
    StartsWith,
}

/// Split `asn__in` into its field and operator. An unsupported suffix stays
/// part of the field name (`$.name__bogus`), which matches nothing: an empty
/// result rather than an unfiltered page.
fn parse_filter_key(key: &str) -> (&str, FilterOp) {
    let with_op = key.split_once("__").and_then(|(field, op)| {
        let op = match op {
            "in" => FilterOp::In,
            "lt" => FilterOp::Lt,
            "lte" => FilterOp::Lte,
            "gt" => FilterOp::Gt,
            "gte" => FilterOp::Gte,
            "contains" => FilterOp::Contains,
            "startswith" => FilterOp::StartsWith,
            _ => return None,
        };
        Some((field, op))
    });
    with_op.unwrap_or((key, FilterOp::Eq))
}

/// Push a bind value and return its 1-based `?N` placeholder index.
fn push_bind(binds: &mut Vec<Bind>, bind: Bind) -> usize {
    binds.push(bind);
    binds.len()
}

/// Append the SQL for one field filter. Fields compare in their text form so a
/// JSON integer (asn 44324) matches its decimal string; absent or null fields
/// extract to NULL, which no comparison matches, so the row drops. Paths and
/// values are bound parameters — no SQL/JSON injection — and each placeholder
/// is used once.
fn push_field_filter(
    sql: &mut String,
    binds: &mut Vec<Bind>,
    field: &str,
    op: FilterOp,
    value: &str,
) {
    let path = || Bind::Text(format!("$.{}", field));
    match op {
        FilterOp::Eq => {
            // Upstream matches text case-insensitively and accepts true/True/1
            // for booleans. json_extract turns JSON booleans into 1/0, so also
            // compare json_type against the value's boolean reading ('' when
            // it has none, which no json_type equals).
            let as_bool = match value.to_ascii_lowercase().as_str() {
                "1" | "true" => "true",
                "0" | "false" => "false",
                _ => "",
            };
            let p1 = push_bind(binds, path());
            let b = push_bind(binds, Bind::Text(as_bool.to_string()));
            let p2 = push_bind(binds, path());
            let v = push_bind(binds, Bind::Text(value.to_string()));
            sql.push_str(&format!(
                " AND (json_type(payload, ?{}) = ?{} \
                 OR CAST(json_extract(payload, ?{}) AS TEXT) = ?{} COLLATE NOCASE)",
                p1, b, p2, v
            ));
        }
        FilterOp::In => {
            let p = push_bind(binds, path());
            let placeholders: Vec<String> = value
                .split(',')
                .map(|v| format!("?{}", push_bind(binds, Bind::Text(v.trim().to_string()))))
                .collect();
            sql.push_str(&format!(
                " AND CAST(json_extract(payload, ?{}) AS TEXT) COLLATE NOCASE IN ({})",
                p,
                placeholders.join(", ")
            ));
        }
        FilterOp::Lt | FilterOp::Lte | FilterOp::Gt | FilterOp::Gte => {
            let cmp = match op {
                FilterOp::Lt => "<",
                FilterOp::Lte => "<=",
                FilterOp::Gt => ">",
                _ => ">=",
            };
            let p = push_bind(binds, path());
            // Numbers compare numerically; anything else (e.g. ISO dates) as text.
            match value.parse::<f64>().ok().filter(|n| n.is_finite()) {
                Some(n) => {
                    let v = push_bind(binds, Bind::Num(n));
                    sql.push_str(&format!(" AND json_extract(payload, ?{}) {} ?{}", p, cmp, v));
                }
                None => {
                    let v = push_bind(binds, Bind::Text(value.to_string()));
                    sql.push_str(&format!(
                        " AND CAST(json_extract(payload, ?{}) AS TEXT) {} ?{}",
                        p, cmp, v
                    ));
                }
            }
        }
        FilterOp::Contains | FilterOp::StartsWith => {
            // Both are case-insensitive upstream. instr() sidesteps LIKE's
            // wildcard escaping; a first match at position 1 means "starts with".
            let test = if op == FilterOp::Contains { "> 0" } else { "= 1" };
            let p = push_bind(binds, path());
            let v = push_bind(binds, Bind::Text(value.to_string()));
            sql.push_str(&format!(
                " AND instr(lower(CAST(json_extract(payload, ?{}) AS TEXT)), lower(?{})) {}",
                p, v, test
            ));
        }
    }
}

/// Build the list query from raw query-string pairs. PeeringDB's list
/// endpoints support arbitrary field filters (`?asn=44324`,
/// `?info_never_via_route_servers=1`, `?name__contains=hurricane`); we honor
/// them by extracting the field from the stored JSON payload. Unmatched
/// filters yield an empty result — we never fall back to an unfiltered page,
/// which would let a client (e.g. pathvector reading `data[0]`) mistake a
/// wrong record for the right one. Malformed `id`/`since`/`limit`/`skip`
/// values are rejected with the official API's status and message.
fn build_query_plan(
    resource: &str,
    pairs: &[(String, String)],
) -> std::result::Result<QueryPlan, ApiError> {
    let mut id_filter: Option<i64> = None;
    let mut since_filter: Option<i64> = None;
    let mut skip: i64 = 0;
    // 0 = no limit: like upstream, every match is returned unless the client
    // pages — up to MAX_RESPONSE_ROWS, past which the request is refused.
    let mut limit: i64 = 0;
    let mut field_filters: Vec<(&str, &str)> = Vec::new();

    for (key, value) in pairs {
        match key.as_str() {
            "id" => {
                id_filter = Some(value.parse().map_err(|_| ApiError {
                    status: 404,
                    message: "Entity not found",
                })?)
            }
            "since" => {
                since_filter = Some(value.parse().map_err(|_| ApiError {
                    status: 400,
                    message: "'since' needs to be a unix timestamp (epoch seconds)",
                })?)
            }
            "limit" => {
                limit = value.parse().map_err(|_| ApiError {
                    status: 400,
                    message: "'limit' needs to be a number",
                })?
            }
            "skip" => {
                skip = value.parse().map_err(|_| ApiError {
                    status: 400,
                    message: "'skip' needs to be a number",
                })?
            }
            other if !is_reserved_param(other) && is_filterable_field(other) => {
                field_filters.push((other, value.as_str()));
            }
            _ => {}
        }
    }

    let mut sql = String::from("SELECT payload FROM objects WHERE resource = ?1");
    let mut binds: Vec<Bind> = vec![Bind::Text(resource.to_string())];

    if let Some(id) = id_filter {
        let idx = push_bind(&mut binds, Bind::Num(id as f64));
        sql.push_str(&format!(" AND obj_id = ?{}", idx));
    }

    if let Some(since_ts) = since_filter {
        let idx = push_bind(&mut binds, Bind::Num(since_ts as f64));
        sql.push_str(&format!(
            " AND datetime(updated) > datetime(?{}, 'unixepoch')",
            idx
        ));
    } else {
        // Like the official API: deleted objects only show up in `since`
        // queries, so incremental consumers can propagate the deletion.
        sql.push_str(&format!(" AND {}", NOT_DELETED));
    }

    for (key, value) in field_filters {
        let (field, op) = parse_filter_key(key);
        push_field_filter(&mut sql, &mut binds, field, op, value);
    }

    // Fetch at most one row past the cap: enough to tell an oversized result
    // (refused by the caller) from one that fits, without loading all of it.
    let cap = MAX_RESPONSE_ROWS as i64 + 1;
    let limit = if limit > 0 { limit.min(cap) } else { cap };
    let idx = push_bind(&mut binds, Bind::Num(limit as f64));
    sql.push_str(&format!(" ORDER BY obj_id LIMIT ?{}", idx));
    if skip > 0 {
        let idx = push_bind(&mut binds, Bind::Num(skip as f64));
        sql.push_str(&format!(" OFFSET ?{}", idx));
    }

    Ok(QueryPlan {
        sql,
        binds,
        by_id: id_filter.is_some(),
    })
}

async fn run_sync(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let secret = ctx.secret("SYNC_SECRET")?;
    let expected = format!("Bearer {}", secret.to_string());
    let authorized = matches!(req.headers().get("Authorization")?, Some(header) if header == expected);
    if !authorized {
        return Response::error("unauthorized", 401);
    }

    let requested = req
        .url()?
        .query_pairs()
        .find(|(k, _)| k == "resource")
        .map(|(_, v)| v.into_owned());

    // An unknown ?resource= is an error, not a cue to sync everything.
    let resources: Vec<&str> = match requested.as_deref() {
        None => RESOURCES.to_vec(),
        Some(name) => match RESOURCES.iter().find(|&&known| known == name) {
            Some(&known) => vec![known],
            None => return Response::error("unknown resource", 400),
        },
    };

    let summary = sync_all(&ctx.env, &resources).await;
    let status = if summary.failed.is_empty() { 200 } else { 500 };
    Ok(Response::from_json(&summary)?.with_status(status))
}

/// Resources sync independently: a failure is logged and reported, and the
/// rest still run, so one bad resource can't leave every later one stale.
async fn sync_all(env: &Env, resources: &[&str]) -> SyncSummary {
    let mut summary = SyncSummary {
        synced: Vec::new(),
        failed: Vec::new(),
    };
    for resource in resources {
        match sync_resource(env, resource).await {
            Ok(report) => {
                console_log!("synced {} objects for {}", report.imported, report.resource);
                summary.synced.push(report);
            }
            Err(err) => {
                console_error!("sync failed for {}: {:?}", resource, err);
                summary.failed.push(SyncFailure {
                    resource: resource.to_string(),
                    error: err.to_string(),
                });
            }
        }
    }
    summary
}

async fn sync_resource(
    env: &Env,
    resource: &str,
) -> Result<SyncReport> {
    let db = env.d1("PEERINGDB")?;
    // Optional: a PeeringDB API key lifts the strict anonymous rate limits.
    let api_key = env.secret("PEERINGDB_API_KEY").ok().map(|k| k.to_string());
    let since = sync_watermark(&db, resource).await?;

    // Incremental from the last completed sync; full snapshot when there is none.
    let imported = match since {
        Some(since_ts) => sync_since(&db, resource, since_ts, api_key.as_deref()).await?,
        None => sync_full(&db, resource).await?,
    };

    // Advance the watermark only once the resource is fully in. Deriving it
    // from MAX(updated) mid-import would strand snapshot rows not yet written,
    // since the snapshot is ordered by id, not by updated.
    if let Some(ts) = max_updated_epoch(&db, resource).await? {
        set_sync_watermark(&db, resource, ts).await?;
    }

    Ok(SyncReport {
        resource: resource.to_string(),
        imported,
    })
}

async fn sync_full(db: &D1Database, resource: &str) -> Result<usize> {
    let url = format!("{}/{}-0.json", PUBLIC_BASE, resource);
    // The public snapshot CDN needs no API key.
    let parsed = fetch_api(&url, None).await?;
    upsert_objects(db, resource, &parsed.data).await
}

async fn sync_since(
    db: &D1Database,
    resource: &str,
    since_ts: i64,
    api_key: Option<&str>,
) -> Result<usize> {
    let now_secs = (Date::now() / 1000.0) as i64;
    let effective_since = since_ts.min(now_secs);

    let mut total = 0usize;
    let mut skip = 0i64;
    let limit = SYNC_PAGE_SIZE;

    loop {
        let url = format!(
            "https://www.peeringdb.com/api/{}?since={}&limit={}&skip={}",
            resource, effective_since, limit, skip
        );
        let parsed = fetch_api(&url, api_key).await?;
        if parsed.data.is_empty() {
            break;
        }

        let imported = upsert_objects(db, resource, &parsed.data).await?;
        total += imported;

        if (parsed.data.len() as i64) < limit {
            break;
        }
        skip += limit;
    }

    Ok(total)
}

// Whitespace-trimmed string fields, to match PeeringDB's live API which strips
// leading/trailing whitespace on save. The bulk CDN snapshots can carry raw
// operator-entered dirt (e.g. a trailing newline in `irr_as_set`); normalize it
// here so the mirror stays byte-faithful to the official API rather than the snapshot.
const TRIM_STRING_FIELDS: &[&str] = &["irr_as_set"];

fn normalize_in_place(obj: &mut Value) {
    if let Some(map) = obj.as_object_mut() {
        for field in TRIM_STRING_FIELDS {
            if let Some(Value::String(s)) = map.get_mut(*field) {
                let trimmed = s.trim();
                if trimmed.len() != s.len() {
                    *s = trimmed.to_string();
                }
            }
        }
    }
}

fn normalize_object(obj: &Value) -> Value {
    let mut obj = obj.clone();
    normalize_in_place(&mut obj);
    obj
}

async fn upsert_objects(db: &D1Database, resource: &str, objects: &[Value]) -> Result<usize> {
    let mut imported = 0usize;
    let mut batch: Vec<D1PreparedStatement> = Vec::with_capacity(BATCH_SIZE);
    for obj in objects {
        let normalized = normalize_object(obj);
        // One malformed object shouldn't block the rest of the resource.
        let Some(id) = normalized.get("id").and_then(|v| v.as_i64()) else {
            console_warn!("skipping {} object without an integer id", resource);
            continue;
        };
        let updated = normalized
            .get("updated")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let payload = serde_json::to_string(&normalized)?;

        // Never replace a row with an older version: a full re-sync over data
        // already advanced by `since` would otherwise roll it back to the
        // snapshot. `updated` is fixed-format ISO 8601, so text order is time order.
        let prepared = db
            .prepare("INSERT INTO objects (resource, obj_id, updated, payload) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(resource, obj_id) DO UPDATE SET updated = excluded.updated, payload = excluded.payload WHERE excluded.updated >= objects.updated")
            .bind(&[
                JsValue::from_str(resource),
                JsValue::from_f64(id as f64),
                JsValue::from_str(updated),
                JsValue::from_str(&payload),
            ])?;
        batch.push(prepared);
        imported += 1;

        if batch.len() >= BATCH_SIZE {
            db.batch(batch).await?;
            batch = Vec::with_capacity(BATCH_SIZE);
        }
    }

    if !batch.is_empty() {
        db.batch(batch).await?;
    }

    Ok(imported)
}

async fn max_updated_epoch(db: &D1Database, resource: &str) -> Result<Option<i64>> {
    let stmt = db.prepare("SELECT strftime('%s', MAX(updated)) as ts FROM objects WHERE resource = ?1");
    let query = stmt.bind(&[JsValue::from_str(resource)])?;
    let row = query.first::<TsRow>(None).await?;
    Ok(row.and_then(|r| r.ts.and_then(|v| v.parse::<i64>().ok())))
}

async fn sync_watermark(db: &D1Database, resource: &str) -> Result<Option<i64>> {
    let stmt = db.prepare("SELECT since FROM sync_state WHERE resource = ?1");
    let query = stmt.bind(&[JsValue::from_str(resource)])?;
    let row = query.first::<SinceRow>(None).await?;
    Ok(row.map(|r| r.since))
}

async fn set_sync_watermark(db: &D1Database, resource: &str, since: i64) -> Result<()> {
    db.prepare("INSERT INTO sync_state (resource, since) VALUES (?1, ?2) ON CONFLICT(resource) DO UPDATE SET since = excluded.since")
        .bind(&[JsValue::from_str(resource), JsValue::from_f64(since as f64)])?
        .run()
        .await?;
    Ok(())
}

async fn fetch_api(url: &str, api_key: Option<&str>) -> Result<ApiResponse> {
    let mut attempt = 1;
    loop {
        let mut init = RequestInit::new();
        init.with_method(Method::Get);
        let mut request = Request::new_with_init(url, &init)?;

        {
            let headers = request.headers_mut()?;
            headers.set("Accept", "application/json")?;
            headers.set("User-Agent", USER_AGENT)?;
            if let Some(key) = api_key {
                headers.set("Authorization", &format!("Api-Key {}", key))?;
            }
        }

        let mut resp = Fetch::Request(request).send().await?;

        let status = resp.status_code();
        // PeeringDB throttles anonymous clients hard (and Workers share egress
        // IPs), so wait out a 429 as told rather than failing the resource.
        if status == 429 && attempt < MAX_FETCH_ATTEMPTS {
            let wait = retry_delay_secs(resp.headers().get("Retry-After")?.as_deref());
            console_warn!("throttled by {}; retry {} in {}s", url, attempt, wait);
            Delay::from(Duration::from_secs(wait)).await;
            attempt += 1;
            continue;
        }
        if status >= 400 {
            let body = resp.text().await.unwrap_or_else(|_| "<no-body>".into());
            return Err(Error::RustError(format!(
                "status {} from {} body_snip={}",
                status,
                url,
                body.get(..200).unwrap_or(&body)
            )));
        }

        return resp.json().await;
    }
}

/// Seconds to wait before retrying a throttled request: the server's
/// Retry-After (delta-seconds) plus a second of slack, since it is rounded
/// down; 2s when absent or unparseable; never more than 30s.
fn retry_delay_secs(retry_after: Option<&str>) -> u64 {
    let secs = retry_after
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(2);
    (secs + 1).min(30)
}

fn json_response(data: Vec<Value>) -> Result<Response> {
    Response::from_json(&json!({ "meta": {}, "data": data }))
}

fn error_response(status: u16, message: &str) -> Result<Response> {
    Ok(Response::from_json(&json!({ "meta": { "error": message }, "data": [] }))?
        .with_status(status))
}

// Worker-build shim checks for this export; we provide a no-op to silence warnings.
#[wasm_bindgen]
pub fn set_panic_hook() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_trailing_newline_on_irr_as_set() {
        let obj = json!({ "id": 1, "irr_as_set": "RADB::AS-FOO\n" });
        let out = normalize_object(&obj);
        assert_eq!(out["irr_as_set"], json!("RADB::AS-FOO"));
    }

    #[test]
    fn trims_leading_and_trailing_whitespace() {
        let obj = json!({ "irr_as_set": "  AS-BAR \t" });
        let out = normalize_object(&obj);
        assert_eq!(out["irr_as_set"], json!("AS-BAR"));
    }

    #[test]
    fn leaves_clean_value_untouched() {
        let obj = json!({ "irr_as_set": "RADB::AS-SIMPLE" });
        let out = normalize_object(&obj);
        assert_eq!(out["irr_as_set"], json!("RADB::AS-SIMPLE"));
    }

    #[test]
    fn does_not_trim_free_text_fields() {
        // notes/name/etc. are served verbatim by the official API; must not change.
        let obj = json!({ "notes": "peer with us. ", "name": "ACME ", "irr_as_set": "AS-X " });
        let out = normalize_object(&obj);
        assert_eq!(out["notes"], json!("peer with us. "));
        assert_eq!(out["name"], json!("ACME "));
        assert_eq!(out["irr_as_set"], json!("AS-X"));
    }

    #[test]
    fn in_place_trims_read_path_payload() {
        // Mirrors the read path: a row stored dirty is cleaned before responding.
        let mut payload: Value =
            serde_json::from_str(r#"{"id":37749,"irr_as_set":"RADB::AS-SIMPLE\n"}"#).unwrap();
        normalize_in_place(&mut payload);
        assert_eq!(payload["irr_as_set"], json!("RADB::AS-SIMPLE"));
    }

    #[test]
    fn ignores_non_string_and_missing_fields() {
        let obj = json!({ "id": 1, "irr_as_set": null });
        let out = normalize_object(&obj);
        assert_eq!(out["irr_as_set"], json!(null));
        let obj2 = json!({ "id": 2 });
        assert_eq!(normalize_object(&obj2), obj2);
    }

    #[test]
    fn retry_delay_follows_retry_after_with_slack() {
        assert_eq!(retry_delay_secs(Some("2")), 3);
        assert_eq!(retry_delay_secs(Some(" 0 ")), 1);
        assert_eq!(retry_delay_secs(None), 3);
        assert_eq!(retry_delay_secs(Some("Wed, 21 Oct 2026 07:28:00 GMT")), 3);
        assert_eq!(retry_delay_secs(Some("600")), 30);
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The LIMIT every plan without a smaller client limit carries.
    const CAP: f64 = (MAX_RESPONSE_ROWS + 1) as f64;

    fn plan_for(resource: &str, items: &[(&str, &str)]) -> QueryPlan {
        build_query_plan(resource, &pairs(items)).expect("valid query")
    }

    #[test]
    fn plan_without_params_fetches_one_past_the_cap() {
        // No silent page size: the old 250 truncated e.g. netixlan?asn=6939
        // (335 sessions). Fetching one row past the cap lets the handler
        // refuse an oversized result instead of truncating it.
        let plan = plan_for("net", &[]);
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 \
             AND json_extract(payload, '$.status') IS NOT 'deleted' \
             ORDER BY obj_id LIMIT ?2"
        );
        assert_eq!(plan.binds, vec![Bind::Text("net".into()), Bind::Num(CAP)]);
        assert!(!plan.by_id);
    }

    #[test]
    fn plan_filters_by_arbitrary_field() {
        // The bug fix: ?asn=44324 must actually filter, not return page 1.
        let plan = plan_for("net", &[("asn", "44324")]);
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 \
             AND json_extract(payload, '$.status') IS NOT 'deleted' \
             AND (json_type(payload, ?2) = ?3 \
             OR CAST(json_extract(payload, ?4) AS TEXT) = ?5 COLLATE NOCASE) \
             ORDER BY obj_id LIMIT ?6"
        );
        assert_eq!(
            plan.binds,
            vec![
                Bind::Text("net".into()),
                Bind::Text("$.asn".into()),
                Bind::Text("".into()),
                Bind::Text("$.asn".into()),
                Bind::Text("44324".into()),
                Bind::Num(CAP),
            ]
        );
    }

    #[test]
    fn plan_reads_boolean_filter_values() {
        // Upstream accepts 1/true/True (and 0/false) for boolean fields.
        for (value, as_bool) in [("1", "true"), ("True", "true"), ("false", "false"), ("x", "")] {
            let plan = plan_for("net", &[("info_never_via_route_servers", value)]);
            assert_eq!(plan.binds[1], Bind::Text("$.info_never_via_route_servers".into()));
            assert_eq!(plan.binds[2], Bind::Text(as_bool.into()));
            assert_eq!(plan.binds[4], Bind::Text(value.into()));
        }
    }

    #[test]
    fn plan_combines_multiple_filters_in_order() {
        let plan = plan_for("netixlan", &[("asn", "44324"), ("ix_id", "26")]);
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 \
             AND json_extract(payload, '$.status') IS NOT 'deleted' \
             AND (json_type(payload, ?2) = ?3 \
             OR CAST(json_extract(payload, ?4) AS TEXT) = ?5 COLLATE NOCASE) \
             AND (json_type(payload, ?6) = ?7 \
             OR CAST(json_extract(payload, ?8) AS TEXT) = ?9 COLLATE NOCASE) \
             ORDER BY obj_id LIMIT ?10"
        );
        assert_eq!(plan.binds[4], Bind::Text("44324".into()));
        assert_eq!(plan.binds[5], Bind::Text("$.ix_id".into()));
        assert_eq!(plan.binds[8], Bind::Text("26".into()));
    }

    #[test]
    fn plan_keeps_id_as_primary_key_lookup() {
        let plan = plan_for("net", &[("id", "34997")]);
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 AND obj_id = ?2 \
             AND json_extract(payload, '$.status') IS NOT 'deleted' \
             ORDER BY obj_id LIMIT ?3"
        );
        assert_eq!(plan.binds[1], Bind::Num(34997.0));
        assert!(plan.by_id);
    }

    #[test]
    fn plan_id_operator_is_a_filter_not_a_lookup() {
        // Only the exact `id` key gets upstream's 404-when-empty treatment.
        let plan = plan_for("net", &[("id__in", "1,2")]);
        assert!(!plan.by_id);
        assert_eq!(plan.binds[1], Bind::Text("$.id".into()));
    }

    #[test]
    fn plan_since_includes_deleted_objects() {
        // Upstream `since` returns deletions so consumers can propagate them.
        let plan = plan_for("netixlan", &[("since", "1700000000")]);
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 \
             AND datetime(updated) > datetime(?2, 'unixepoch') \
             ORDER BY obj_id LIMIT ?3"
        );
    }

    #[test]
    fn plan_status_deleted_filter_still_hides_deleted() {
        // Upstream returns nothing for ?status=deleted on a plain list; both
        // clauses apply, so the mirror does too.
        let plan = plan_for("netixlan", &[("status", "deleted")]);
        assert!(plan.sql.contains(NOT_DELETED));
        assert_eq!(plan.binds[1], Bind::Text("$.status".into()));
        assert_eq!(plan.binds[4], Bind::Text("deleted".into()));
    }

    #[test]
    fn plan_ignores_response_shaping_params() {
        // depth/fields/q/format must not become field filters (which would
        // force an empty result), and must not survive as filters at all.
        let plan = plan_for(
            "net",
            &[("depth", "1"), ("fields", "asn,name"), ("q", "foo"), ("format", "json")],
        );
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 \
             AND json_extract(payload, '$.status') IS NOT 'deleted' \
             ORDER BY obj_id LIMIT ?2"
        );
    }

    #[test]
    fn plan_drops_malformed_field_keys() {
        // Uppercase / punctuation keys can't form a valid JSON path; ignore them.
        let plan = plan_for("net", &[("ASN", "1"), ("a-b", "2")]);
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 \
             AND json_extract(payload, '$.status') IS NOT 'deleted' \
             ORDER BY obj_id LIMIT ?2"
        );
    }

    #[test]
    fn plan_supports_limit_and_skip() {
        let plan = plan_for("net", &[("limit", "5"), ("skip", "10")]);
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 \
             AND json_extract(payload, '$.status') IS NOT 'deleted' \
             ORDER BY obj_id LIMIT ?2 OFFSET ?3"
        );
        assert_eq!(plan.binds[1], Bind::Num(5.0));
        assert_eq!(plan.binds[2], Bind::Num(10.0));
    }

    #[test]
    fn plan_limit_zero_means_no_client_limit() {
        // Upstream's "no limit"; the mirror still stops one past its cap.
        let plan = plan_for("net", &[("limit", "0")]);
        assert!(plan.sql.ends_with("ORDER BY obj_id LIMIT ?2"));
        assert_eq!(plan.binds[1], Bind::Num(CAP));
    }

    #[test]
    fn plan_clamps_limits_above_the_cap() {
        // limit=100000 on a big table must be refused, not loaded in full.
        let plan = plan_for("netixlan", &[("limit", "100000")]);
        assert_eq!(plan.binds[1], Bind::Num(CAP));
    }

    #[test]
    fn plan_skip_without_limit_pages_from_offset() {
        let plan = plan_for("net", &[("skip", "10")]);
        assert!(plan.sql.ends_with("ORDER BY obj_id LIMIT ?2 OFFSET ?3"));
        assert_eq!(plan.binds[1], Bind::Num(CAP));
        assert_eq!(plan.binds[2], Bind::Num(10.0));
    }

    #[test]
    fn plan_rejects_malformed_numbers_like_upstream() {
        // These used to be ignored, which returned an unfiltered page.
        let err = |items: &[(&str, &str)]| build_query_plan("net", &pairs(items)).err();
        assert_eq!(
            err(&[("id", "abc")]),
            Some(ApiError { status: 404, message: "Entity not found" })
        );
        assert_eq!(
            err(&[("since", "abc")]),
            Some(ApiError {
                status: 400,
                message: "'since' needs to be a unix timestamp (epoch seconds)"
            })
        );
        assert_eq!(
            err(&[("limit", "abc")]),
            Some(ApiError { status: 400, message: "'limit' needs to be a number" })
        );
        assert_eq!(
            err(&[("skip", "abc")]),
            Some(ApiError { status: 400, message: "'skip' needs to be a number" })
        );
    }

    #[test]
    fn plan_in_operator_matches_any_listed_value() {
        let plan = plan_for("net", &[("asn__in", "6939, 44324")]);
        assert!(plan.sql.contains(
            "AND CAST(json_extract(payload, ?2) AS TEXT) COLLATE NOCASE IN (?3, ?4)"
        ));
        assert_eq!(plan.binds[1], Bind::Text("$.asn".into()));
        assert_eq!(plan.binds[2], Bind::Text("6939".into()));
        assert_eq!(plan.binds[3], Bind::Text("44324".into()));
    }

    #[test]
    fn plan_range_operators_compare_numbers_numerically() {
        let plan = plan_for("net", &[("asn__lt", "4"), ("updated__gte", "2026-01-01")]);
        assert!(plan.sql.contains("AND json_extract(payload, ?2) < ?3"));
        assert!(plan
            .sql
            .contains("AND CAST(json_extract(payload, ?4) AS TEXT) >= ?5"));
        assert_eq!(plan.binds[2], Bind::Num(4.0));
        assert_eq!(plan.binds[4], Bind::Text("2026-01-01".into()));
    }

    #[test]
    fn plan_contains_and_startswith_ignore_case() {
        let plan = plan_for(
            "net",
            &[("name__contains", "Hurricane"), ("name__startswith", "hurr")],
        );
        assert!(plan.sql.contains(
            "AND instr(lower(CAST(json_extract(payload, ?2) AS TEXT)), lower(?3)) > 0"
        ));
        assert!(plan.sql.contains(
            "AND instr(lower(CAST(json_extract(payload, ?4) AS TEXT)), lower(?5)) = 1"
        ));
        assert_eq!(plan.binds[1], Bind::Text("$.name".into()));
    }

    #[test]
    fn plan_unsupported_operator_suffix_matches_nothing_not_page_one() {
        // asn__bogus builds a $.asn__bogus path that no payload has -> empty
        // result, the safe failure mode (never a misleading full page).
        let plan = plan_for("net", &[("asn__bogus", "1")]);
        assert_eq!(plan.binds[1], Bind::Text("$.asn__bogus".into()));
    }
}
