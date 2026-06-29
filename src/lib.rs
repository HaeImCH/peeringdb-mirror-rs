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
        let mut payload: Value = serde_json::from_str(&row.payload)?;
        // Normalize on read too, so rows ingested before the fix (or not yet
        // re-synced) are still served clean to the client.
        normalize_in_place(&mut payload);
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
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let plan = build_query_plan(resource, &pairs);

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
    let payloads: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            serde_json::from_str::<Value>(&row.payload).map(|mut v| {
                normalize_in_place(&mut v);
                v
            })
        })
        .collect::<std::result::Result<_, _>>()?;

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
}

/// Reserved query params that shape the response or are handled as dedicated
/// columns — never as JSON field filters. `depth`/`fields`/`q` are
/// PeeringDB response-shaping/search knobs the mirror doesn't model; we ignore
/// them rather than mistaking them for field filters (which would wrongly
/// return an empty set).
fn is_reserved_param(key: &str) -> bool {
    matches!(key, "id" | "since" | "limit" | "skip" | "depth" | "fields" | "q")
}

/// PeeringDB field names are lowercase snake_case alphanumerics (e.g. `asn`,
/// `irr_as_set`, `info_prefixes4`). Restrict filter keys to that shape so the
/// `$.<field>` JSON path we build is always well-formed; anything else is
/// dropped. Operator-suffixed keys like `asn__in` pass this check and simply
/// match nothing (empty result) rather than silently returning a full page.
fn is_filterable_field(key: &str) -> bool {
    let mut bytes = key.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_lowercase() => {}
        _ => return false,
    }
    bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Build the list query from raw query-string pairs. PeeringDB's list
/// endpoints support arbitrary field-equality filters (`?asn=44324`,
/// `?info_never_via_route_servers=1`); we honor any such filter by extracting
/// the field from the stored JSON payload. Unmatched filters yield an empty
/// result — we never fall back to an unfiltered page, which would let a client
/// (e.g. pathvector reading `data[0]`) mistake a wrong record for the right one.
fn build_query_plan(resource: &str, pairs: &[(String, String)]) -> QueryPlan {
    let mut id_filter: Option<i64> = None;
    let mut since_filter: Option<i64> = None;
    let mut skip: i64 = 0;
    let mut limit: i64 = DEFAULT_QUERY_LIMIT;
    let mut field_filters: Vec<(String, String)> = Vec::new();

    for (key, value) in pairs {
        match key.as_str() {
            "id" => id_filter = value.parse().ok(),
            "since" => since_filter = value.parse().ok(),
            "limit" => limit = value.parse::<i64>().unwrap_or(limit),
            "skip" => skip = value.parse::<i64>().unwrap_or(skip),
            other if !is_reserved_param(other) && is_filterable_field(other) => {
                field_filters.push((other.to_string(), value.clone()));
            }
            _ => {}
        }
    }

    let mut sql = String::from("SELECT payload FROM objects WHERE resource = ?1");
    let mut binds: Vec<Bind> = vec![Bind::Text(resource.to_string())];

    if let Some(id) = id_filter {
        let idx = binds.len() + 1;
        sql.push_str(&format!(" AND obj_id = ?{}", idx));
        binds.push(Bind::Num(id as f64));
    }

    if let Some(since_ts) = since_filter {
        let idx = binds.len() + 1;
        sql.push_str(&format!(
            " AND datetime(updated) > datetime(?{}, 'unixepoch')",
            idx
        ));
        binds.push(Bind::Num(since_ts as f64));
    }

    for (field, value) in &field_filters {
        // Compare both sides as text so a JSON integer (e.g. asn 44324) matches
        // its decimal string form; absent fields json_extract to NULL and drop
        // the row. The path is a bound parameter — no SQL/JSON injection.
        let path_idx = binds.len() + 1;
        let val_idx = binds.len() + 2;
        sql.push_str(&format!(
            " AND CAST(json_extract(payload, ?{}) AS TEXT) = ?{}",
            path_idx, val_idx
        ));
        binds.push(Bind::Text(format!("$.{}", field)));
        binds.push(Bind::Text(value.clone()));
    }

    let limit_idx = binds.len() + 1;
    sql.push_str(&format!(" ORDER BY obj_id LIMIT ?{}", limit_idx));
    binds.push(Bind::Num(limit.max(1) as f64));

    if skip > 0 {
        let off_idx = binds.len() + 1;
        sql.push_str(&format!(" OFFSET ?{}", off_idx));
        binds.push(Bind::Num(skip as f64));
    }

    QueryPlan { sql, binds }
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
        let id = normalized
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| Error::RustError("object is missing id".into()))?;
        let updated = normalized
            .get("updated")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let payload = serde_json::to_string(&normalized)?;

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

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn plan_without_params_lists_first_page() {
        let plan = build_query_plan("net", &[]);
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 ORDER BY obj_id LIMIT ?2"
        );
        assert_eq!(
            plan.binds,
            vec![Bind::Text("net".into()), Bind::Num(DEFAULT_QUERY_LIMIT as f64)]
        );
    }

    #[test]
    fn plan_filters_by_arbitrary_field() {
        // The bug fix: ?asn=44324 must actually filter, not return page 1.
        let plan = build_query_plan("net", &pairs(&[("asn", "44324")]));
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 \
             AND CAST(json_extract(payload, ?2) AS TEXT) = ?3 \
             ORDER BY obj_id LIMIT ?4"
        );
        assert_eq!(
            plan.binds,
            vec![
                Bind::Text("net".into()),
                Bind::Text("$.asn".into()),
                Bind::Text("44324".into()),
                Bind::Num(DEFAULT_QUERY_LIMIT as f64),
            ]
        );
    }

    #[test]
    fn plan_filters_boolean_field() {
        let plan = build_query_plan("net", &pairs(&[("info_never_via_route_servers", "1")]));
        assert!(plan
            .sql
            .contains("AND CAST(json_extract(payload, ?2) AS TEXT) = ?3"));
        assert_eq!(plan.binds[1], Bind::Text("$.info_never_via_route_servers".into()));
        assert_eq!(plan.binds[2], Bind::Text("1".into()));
    }

    #[test]
    fn plan_combines_multiple_filters_in_order() {
        let plan = build_query_plan("netixlan", &pairs(&[("asn", "44324"), ("ix_id", "26")]));
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 \
             AND CAST(json_extract(payload, ?2) AS TEXT) = ?3 \
             AND CAST(json_extract(payload, ?4) AS TEXT) = ?5 \
             ORDER BY obj_id LIMIT ?6"
        );
        assert_eq!(plan.binds[1], Bind::Text("$.asn".into()));
        assert_eq!(plan.binds[2], Bind::Text("44324".into()));
        assert_eq!(plan.binds[3], Bind::Text("$.ix_id".into()));
        assert_eq!(plan.binds[4], Bind::Text("26".into()));
    }

    #[test]
    fn plan_keeps_id_as_primary_key_lookup() {
        let plan = build_query_plan("net", &pairs(&[("id", "34997")]));
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 AND obj_id = ?2 ORDER BY obj_id LIMIT ?3"
        );
        assert_eq!(plan.binds[1], Bind::Num(34997.0));
    }

    #[test]
    fn plan_ignores_response_shaping_params() {
        // depth/fields/q must not become field filters (which would force an
        // empty result), and must not survive as filters at all.
        let plan = build_query_plan(
            "net",
            &pairs(&[("depth", "1"), ("fields", "asn,name"), ("q", "foo")]),
        );
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 ORDER BY obj_id LIMIT ?2"
        );
    }

    #[test]
    fn plan_drops_malformed_field_keys() {
        // Uppercase / punctuation keys can't form a valid JSON path; ignore them.
        let plan = build_query_plan("net", &pairs(&[("ASN", "1"), ("a-b", "2")]));
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 ORDER BY obj_id LIMIT ?2"
        );
    }

    #[test]
    fn plan_supports_limit_and_skip() {
        let plan = build_query_plan("net", &pairs(&[("limit", "5"), ("skip", "10")]));
        assert_eq!(
            plan.sql,
            "SELECT payload FROM objects WHERE resource = ?1 ORDER BY obj_id LIMIT ?2 OFFSET ?3"
        );
        assert_eq!(plan.binds[1], Bind::Num(5.0));
        assert_eq!(plan.binds[2], Bind::Num(10.0));
    }

    #[test]
    fn plan_unsupported_operator_suffix_matches_nothing_not_page_one() {
        // asn__in builds a $.asn__in path that no payload has -> empty result,
        // which is the safe failure mode (never a misleading full page).
        let plan = build_query_plan("net", &pairs(&[("asn__in", "1,2,3")]));
        assert_eq!(plan.binds[1], Bind::Text("$.asn__in".into()));
    }
}
