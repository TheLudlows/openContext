use crate::{
    db,
    error::{AppError, Result},
    service::Service,
    types::*,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub fn router(service: Service) -> Router {
    Router::new()
        .route(
            "/health/live",
            get(|| async { Json(json!({"status":"ok"})) }),
        )
        .route("/health/ready", get(ready))
        .route("/v1/memories", post(memory))
        .route("/v1/captures", post(capture))
        .route("/v1/knowledge", post(knowledge))
        .route("/v1/search", post(search))
        .route("/v1/resolve", post(resolve))
        .route("/v1/candidates", get(candidates))
        .route("/v1/candidates/{id}", get(candidate))
        .route("/v1/candidates/{id}/review", post(review))
        .route("/v1/assets/{id}", get(asset).delete(delete_asset))
        .route("/v1/assets/{id}/restore", post(restore))
        .route("/v1/events/{id}", axum::routing::delete(delete_event))
        .route("/v1/jobs/{id}", get(job))
        .route("/v1/jobs/{id}/{action}", post(job_action))
        .route("/v1/files", post(upload))
        .route("/v1/files/{id}", get(file).delete(delete_file))
        .layer(DefaultBodyLimit::max(crate::parsing::MAX_FILE))
        .with_state(service)
}
fn token(h: &HeaderMap) -> Result<&str> {
    h.get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(AppError::Unauthorized)
}
fn key(h: &HeaderMap) -> Result<&str> {
    h.get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Invalid("Idempotency-Key is required for mutations".into()))
}
async fn auth(s: &Service, h: &HeaderMap) -> Result<AuthContext> {
    s.auth(token(h)?).await
}
async fn ready(State(s): State<Service>) -> impl IntoResponse {
    let result = sqlx::query("SELECT 1 FROM oc.jobs LIMIT 0")
        .execute(&s.pool)
        .await;
    if result.is_ok() {
        (StatusCode::OK, Json(json!({"status":"ready"})))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status":"unavailable"})),
        )
    }
}
async fn memory(
    State(s): State<Service>,
    h: HeaderMap,
    Json(i): Json<MemoryInput>,
) -> Result<Json<Value>> {
    Ok(Json(s.memory(&auth(&s, &h).await?, key(&h)?, i).await?))
}
async fn capture(
    State(s): State<Service>,
    h: HeaderMap,
    Json(i): Json<CaptureInput>,
) -> Result<Json<Value>> {
    Ok(Json(s.capture(&auth(&s, &h).await?, key(&h)?, i).await?))
}
async fn knowledge(
    State(s): State<Service>,
    h: HeaderMap,
    Json(i): Json<KnowledgeInput>,
) -> Result<Json<Value>> {
    Ok(Json(s.knowledge(&auth(&s, &h).await?, key(&h)?, i).await?))
}
async fn search(
    State(s): State<Service>,
    h: HeaderMap,
    Json(i): Json<SearchInput>,
) -> Result<Json<Value>> {
    Ok(Json(s.search(&auth(&s, &h).await?, i).await?))
}
async fn resolve(
    State(s): State<Service>,
    h: HeaderMap,
    Json(i): Json<ResolveInput>,
) -> Result<Json<Value>> {
    Ok(Json(s.resolve(&auth(&s, &h).await?, i).await?))
}
async fn candidates(State(s): State<Service>, h: HeaderMap) -> Result<Json<Value>> {
    Ok(Json(s.candidates(&auth(&s, &h).await?).await?))
}
async fn candidate(
    State(s): State<Service>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    Ok(Json(s.candidate_get(&auth(&s, &h).await?, id).await?))
}
async fn review(
    State(s): State<Service>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Json(i): Json<ReviewInput>,
) -> Result<Json<Value>> {
    Ok(Json(s.review(&auth(&s, &h).await?, key(&h)?, id, i).await?))
}
#[derive(Deserialize)]
struct Version {
    version: Option<i32>,
}
async fn asset(
    State(s): State<Service>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(v): Query<Version>,
) -> Result<Json<Value>> {
    Ok(Json(s.get(&auth(&s, &h).await?, id, v.version).await?))
}
async fn restore(
    State(s): State<Service>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Json(i): Json<RestoreInput>,
) -> Result<Json<Value>> {
    Ok(Json(
        s.restore(&auth(&s, &h).await?, key(&h)?, id, i).await?,
    ))
}
async fn delete_asset(
    State(s): State<Service>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    Ok(Json(
        s.delete(&auth(&s, &h).await?, key(&h)?, id, "asset")
            .await?,
    ))
}
async fn delete_event(
    State(s): State<Service>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    Ok(Json(
        s.delete(&auth(&s, &h).await?, key(&h)?, id, "event")
            .await?,
    ))
}
async fn delete_file(
    State(s): State<Service>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    Ok(Json(
        s.delete(&auth(&s, &h).await?, key(&h)?, id, "file").await?,
    ))
}
async fn job(State(s): State<Service>, h: HeaderMap, Path(id): Path<Uuid>) -> Result<Json<Value>> {
    Ok(Json(s.job(&auth(&s, &h).await?, id).await?))
}
async fn job_action(
    State(s): State<Service>,
    h: HeaderMap,
    Path((id, action)): Path<(Uuid, String)>,
) -> Result<Json<Value>> {
    Ok(Json(
        s.job_action(&auth(&s, &h).await?, key(&h)?, id, &action)
            .await?,
    ))
}
#[derive(Deserialize)]
struct Upload {
    name: String,
    format: String,
}
async fn upload(
    State(s): State<Service>,
    h: HeaderMap,
    Query(q): Query<Upload>,
    body: Bytes,
) -> Result<Json<Value>> {
    Ok(Json(
        s.upload(&auth(&s, &h).await?, key(&h)?, &q.name, &q.format, &body)
            .await?,
    ))
}
async fn file(
    State(s): State<Service>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse> {
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
            (axum::http::header::CONTENT_DISPOSITION, "attachment"),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        s.file(&auth(&s, &h).await?, id).await?,
    ))
}

pub async fn serve(s: Service, bind: &str) -> anyhow::Result<()> {
    db::check_runtime(&s.pool).await?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(address = bind, "HTTP API listening");
    axum::serve(listener, router(s))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
