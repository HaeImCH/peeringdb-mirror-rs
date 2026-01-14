use serde::{Deserialize, Serialize};
use js_sys::Date;
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
const DEFAULT_QUERY_LIMIT: i64 = 250;
const SYNC_PAGE_SIZE: i64 = 1000;
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

#[derive(Deserialize)]
struct TsRow {
    ts: Option<String>,
}

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
    if let Err(err) = sync_all(&env, RESOURCES).await {
        console_error!("scheduled sync failed: {:?}", err);
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
    let id: i64 = id_raw
        .parse()
        .map_err(|_| Error::RustError("id must be an integer".into()))?;

    let db = ctx.env.d1("PEERINGDB")?;
    let statement = db.prepare("SELECT payload FROM objects WHERE resource = ?1 AND obj_id = ?2");
    let query = statement.bind(&[
        JsValue::from_str(&resource),
        JsValue::from_f64(id as f64),
    ])?;
    let row = query.first::<PayloadRow>(None).await?;

    if let Some(row) = row {
        let payload: Value = serde_json::from_str(&row.payload)?;
        json_response(vec![payload])
    } else {
        json_response(Vec::new())
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

    let mut id_filter: Option<i64> = None;
    let mut since_filter: Option<i64> = None;
    let mut limit: i64 = DEFAULT_QUERY_LIMIT;

    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "id" => id_filter = value.parse().ok(),
            "since" => since_filter = value.parse().ok(),
            "limit" => limit = value.parse::<i64>().unwrap_or(limit),
            _ => {}
        }
    }

    let db = ctx.env.d1("PEERINGDB")?;
    let mut sql = String::from("SELECT payload FROM objects WHERE resource = ?1");
    let mut bindings: Vec<JsValue> = vec![JsValue::from_str(&resource)];

    if let Some(id) = id_filter {
        let idx = bindings.len() + 1;
        sql.push_str(&format!(" AND obj_id = ?{}", idx));
        bindings.push(JsValue::from_f64(id as f64));
    }

    if let Some(since_ts) = since_filter {
        let idx = bindings.len() + 1;
        sql.push_str(&format!(
            " AND datetime(updated) > datetime(?{}, 'unixepoch')",
            idx
        ));
        bindings.push(JsValue::from_f64(since_ts as f64));
    }

    let idx = bindings.len() + 1;
    sql.push_str(&format!(" ORDER BY obj_id LIMIT ?{}", idx));
    bindings.push(JsValue::from_f64(limit.max(1) as f64));

    let statement = db.prepare(&sql);
    let query = statement.bind(&bindings)?;
    let result = query.all().await?;
    let rows: Vec<PayloadRow> = result.results()?;
    let payloads: Vec<Value> = rows
        .into_iter()
        .map(|row| serde_json::from_str(&row.payload))
        .collect::<std::result::Result<_, _>>()?;

    json_response(payloads)
}

async fn run_sync(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let secret = ctx.secret("SYNC_SECRET")?;
    let expected = format!("Bearer {}", secret.to_string());
    let authorized = matches!(req.headers().get("Authorization")?, Some(header) if header == expected);
    if !authorized {
        return Response::error("unauthorized", 401);
    }

    let resource_filter = req
        .url()
        .ok()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "resource")
                .map(|(_, v)| v.to_string())
        })
        .and_then(|r| {
            if RESOURCES.contains(&r.as_str()) {
                Some(vec![r])
            } else {
                None
            }
        });

    let resources: Vec<&str> = resource_filter
        .as_ref()
        .map(|v| v.iter().map(|s| s.as_str()).collect())
        .unwrap_or_else(|| RESOURCES.to_vec());

    let reports = sync_all(&ctx.env, &resources).await?;
    Response::from_json(&json!({ "synced": reports }))
}

async fn sync_all(
    env: &Env,
    resources: &[&str],
) -> Result<Vec<SyncReport>> {
    let mut reports = Vec::new();
    for resource in resources {
        let report = sync_resource(env, resource).await?;
        console_log!("synced {} objects for {}", report.imported, report.resource);
        reports.push(report);
    }
    Ok(reports)
}

async fn sync_resource(
    env: &Env,
    resource: &str,
) -> Result<SyncReport> {
    let db = env.d1("PEERINGDB")?;
    let since = max_updated_epoch(&db, resource).await?;

    // Prefer incremental when we have a previous max(updated); fall back to full snapshot.
    let imported = match since {
        Some(since_ts) => sync_since(&db, resource, since_ts).await?,
        None => sync_full(&db, resource).await?,
    };

    Ok(SyncReport {
        resource: resource.to_string(),
        imported,
    })
}

async fn sync_full(db: &D1Database, resource: &str) -> Result<usize> {
    let url = format!("{}/{}-0.json", PUBLIC_BASE, resource);
    let parsed = fetch_api(&url).await?;
    upsert_objects(db, resource, &parsed.data).await
}

async fn sync_since(
    db: &D1Database,
    resource: &str,
    since_ts: i64,
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
        let parsed = fetch_api(&url).await?;
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

async fn upsert_objects(db: &D1Database, resource: &str, objects: &[Value]) -> Result<usize> {
    let mut imported = 0usize;
    let mut batch: Vec<D1PreparedStatement> = Vec::with_capacity(BATCH_SIZE);
    for obj in objects {
        let id = obj
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| Error::RustError("object is missing id".into()))?;
        let updated = obj
            .get("updated")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let payload = serde_json::to_string(obj)?;

        let prepared = db
            .prepare("INSERT INTO objects (resource, obj_id, updated, payload) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(resource, obj_id) DO UPDATE SET updated = excluded.updated, payload = excluded.payload")
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

async fn fetch_api(url: &str) -> Result<ApiResponse> {
    let mut init = RequestInit::new();
    init.with_method(Method::Get);
    let mut request = Request::new_with_init(url, &init)?;

    {
        let headers = request.headers_mut()?;
        headers.set("Accept", "application/json")?;
        headers.set("User-Agent", USER_AGENT)?;
    }

    let mut resp = Fetch::Request(request).send().await?;

    let status = resp.status_code();
    if status >= 400 {
        let body = resp.text().await.unwrap_or_else(|_| "<no-body>".into());
        return Err(Error::RustError(format!(
            "status {} from {} body_snip={}",
            status,
            url,
            body.get(..200).unwrap_or(&body)
        )));
    }

    resp.json().await
}

fn json_response(data: Vec<Value>) -> Result<Response> {
    Response::from_json(&json!({ "meta": {}, "data": data }))
}

// Worker-build shim checks for this export; we provide a no-op to silence warnings.
#[wasm_bindgen]
pub fn set_panic_hook() {}
