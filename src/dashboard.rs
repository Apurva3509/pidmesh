use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ORIGIN, REFERRER_POLICY,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::task;
use uuid::Uuid;

use crate::VERSION;
use crate::store::MeshStore;

const DASHBOARD_HTML: &str = include_str!("../dashboard/index.html");
const DASHBOARD_CSS: &str = include_str!("../dashboard/dashboard.css");
const DASHBOARD_JS: &str = include_str!("../dashboard/dashboard.js");
const MAX_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
struct DashboardState {
    agent_id: Arc<str>,
    origin: Arc<str>,
    store: MeshStore,
    token: Arc<str>,
    workspace: Arc<PathBuf>,
}

#[derive(Debug)]
enum ApiError {
    BadRequest(String),
    Forbidden,
    Internal,
    Unauthorized,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::Forbidden => (StatusCode::FORBIDDEN, "request origin rejected".to_owned()),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "dashboard operation failed".to_owned(),
            ),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "authorization required".to_owned(),
            ),
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

#[derive(Deserialize)]
struct SnapshotQuery {
    limit: Option<u32>,
}

#[derive(Deserialize)]
struct RecallQuery {
    limit: Option<u32>,
    q: String,
}

#[derive(Deserialize)]
struct RememberRequest {
    importance: Option<f64>,
    key: Option<String>,
    kind: Option<String>,
    text: String,
}

#[derive(Deserialize)]
struct SendRequest {
    correlation_id: Option<String>,
    recipient: Option<String>,
    text: String,
}

#[derive(Deserialize)]
struct ClaimRequest {
    detail: Option<String>,
    lease_seconds: Option<u64>,
    task: String,
}

#[derive(Deserialize)]
struct ResourceRequest {
    detail: Option<String>,
    lease_seconds: Option<u64>,
    resources: Vec<String>,
    task: Option<String>,
}

pub async fn serve(store: MeshStore, workspace: PathBuf, port: u16) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .context("failed to bind the local dashboard")?;
    let address = listener.local_addr()?;
    let agent_id = format!(
        "pidmesh-ui-{}-{}",
        std::process::id(),
        &Uuid::new_v4().simple().to_string()[..8]
    );
    let registration_store = store.clone();
    let registration_workspace = workspace.clone();
    let registration_agent = agent_id.clone();
    task::spawn_blocking(move || {
        registration_store.register_agent(
            "pidmesh-ui",
            std::process::id(),
            Some(&registration_workspace),
            "pidmesh-ui",
            &["dashboard".to_owned()],
            Some(&registration_agent),
        )
    })
    .await
    .context("dashboard registration task failed")??;

    let token = Uuid::new_v4().simple().to_string();
    let origin = format!("http://{address}");
    let state = DashboardState {
        agent_id: Arc::from(agent_id.clone()),
        origin: Arc::from(origin.clone()),
        store: store.clone(),
        token: Arc::from(token.clone()),
        workspace: Arc::new(workspace),
    };
    let app = router(state.clone());
    println!(
        "{}",
        json!({
            "agent_id": agent_id,
            "dashboard": format!("{origin}/#token={token}"),
            "listening": address.to_string(),
            "workspace": state.workspace.as_ref()
        })
    );

    let heartbeat_store = store.clone();
    let heartbeat_agent = agent_id.clone();
    let heartbeat = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            let pulse_store = heartbeat_store.clone();
            let pulse_agent = heartbeat_agent.clone();
            if !matches!(
                task::spawn_blocking(move || pulse_store.heartbeat(&pulse_agent)).await,
                Ok(Ok(true))
            ) {
                break;
            }
        }
    });
    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    heartbeat.abort();
    let stop_store = store;
    task::spawn_blocking(move || stop_store.stop_agent(&agent_id))
        .await
        .context("dashboard cleanup task failed")??;
    server_result.context("dashboard server failed")
}

fn router(state: DashboardState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/favicon.ico", get(favicon))
        .route("/dashboard.css", get(styles))
        .route("/dashboard.js", get(script))
        .route("/healthz", get(health))
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/memories", get(recall).post(remember))
        .route("/api/v1/messages", post(send))
        .route("/api/v1/claims", post(claim))
        .route("/api/v1/claims/{task}", delete(release))
        .route(
            "/api/v1/resources",
            get(resources)
                .post(reserve_resources)
                .delete(release_resources),
        )
        .layer(DefaultBodyLimit::max(MAX_TEXT_BYTES))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn styles() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/css; charset=utf-8")], DASHBOARD_CSS)
}

async fn script() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/javascript; charset=utf-8")],
        DASHBOARD_JS,
    )
}

async fn health() -> Json<Value> {
    Json(json!({"service": "pidmesh-dashboard", "version": VERSION}))
}

async fn snapshot(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Query(query): Query<SnapshotQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=500).contains(&limit) {
        return Err(ApiError::BadRequest(
            "limit must be between 1 and 500".to_owned(),
        ));
    }
    let store = state.store.clone();
    let workspace = Arc::clone(&state.workspace);
    let mut value = blocking(move || store.dashboard_snapshot(&workspace, limit)).await?;
    value["dashboard_agent_id"] = json!(state.agent_id.as_ref());
    Ok(Json(value))
}

async fn recall(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Query(query): Query<RecallQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    validate_text("query", &query.q)?;
    let limit = query.limit.unwrap_or(25);
    if !(1..=100).contains(&limit) {
        return Err(ApiError::BadRequest(
            "limit must be between 1 and 100".to_owned(),
        ));
    }
    let store = state.store.clone();
    let agent_id = Arc::clone(&state.agent_id);
    let value = blocking(move || store.recall(&agent_id, &query.q, limit)).await?;
    Ok(Json(value))
}

async fn remember(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Json(request): Json<RememberRequest>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    validate_text("memory", &request.text)?;
    let kind = request.kind.unwrap_or_else(|| "note".to_owned());
    validate_label("kind", &kind)?;
    if let Some(key) = request.key.as_deref() {
        validate_label("key", key)?;
    }
    let importance = request.importance.unwrap_or(0.5);
    let store = state.store.clone();
    let agent_id = Arc::clone(&state.agent_id);
    let value = blocking(move || {
        store.remember(
            &agent_id,
            &request.text,
            &kind,
            request.key.as_deref(),
            importance,
        )
    })
    .await?;
    Ok(Json(value))
}

async fn send(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Json(request): Json<SendRequest>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    validate_text("message", &request.text)?;
    let recipient = request.recipient.unwrap_or_else(|| "*".to_owned());
    validate_label("recipient", &recipient)?;
    let store = state.store.clone();
    let agent_id = Arc::clone(&state.agent_id);
    let value = blocking(move || {
        store.send(
            &agent_id,
            &request.text,
            &recipient,
            request.correlation_id.as_deref(),
        )
    })
    .await?;
    Ok(Json(value))
}

async fn claim(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Json(request): Json<ClaimRequest>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    validate_label("task", &request.task)?;
    let lease_seconds = request.lease_seconds.unwrap_or(300);
    if !(1..=86_400).contains(&lease_seconds) {
        return Err(ApiError::BadRequest(
            "lease must be between 1 and 86400 seconds".to_owned(),
        ));
    }
    let store = state.store.clone();
    let agent_id = Arc::clone(&state.agent_id);
    let value = blocking(move || {
        store.claim(
            &agent_id,
            &request.task,
            lease_seconds,
            request.detail.as_deref(),
        )
    })
    .await?;
    Ok(Json(value))
}

async fn release(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Path(task_key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    validate_label("task", &task_key)?;
    let store = state.store.clone();
    let agent_id = Arc::clone(&state.agent_id);
    let released = blocking(move || store.release(&agent_id, &task_key)).await?;
    Ok(Json(json!({"released": released})))
}

async fn resources(
    State(state): State<DashboardState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    let store = state.store.clone();
    let agent_id = Arc::clone(&state.agent_id);
    let value = blocking(move || store.resources(&agent_id)).await?;
    Ok(Json(value))
}

async fn reserve_resources(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Json(request): Json<ResourceRequest>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    validate_resources(&request.resources)?;
    if let Some(task) = request.task.as_deref() {
        validate_label("task", task)?;
    }
    if let Some(detail) = request.detail.as_deref() {
        validate_text("detail", detail)?;
    }
    let lease_seconds = request.lease_seconds.unwrap_or(900);
    if !(1..=86_400).contains(&lease_seconds) {
        return Err(ApiError::BadRequest(
            "lease must be between 1 and 86400 seconds".to_owned(),
        ));
    }
    let store = state.store.clone();
    let agent_id = Arc::clone(&state.agent_id);
    let value = blocking(move || {
        store.reserve_resources(
            &agent_id,
            &request.resources,
            request.task.as_deref(),
            request.detail.as_deref(),
            lease_seconds,
        )
    })
    .await?;
    Ok(Json(value))
}

async fn release_resources(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Json(request): Json<ResourceRequest>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    validate_resources(&request.resources)?;
    let store = state.store.clone();
    let agent_id = Arc::clone(&state.agent_id);
    let released = blocking(move || store.release_resources(&agent_id, &request.resources)).await?;
    Ok(Json(json!({"released": released})))
}

fn authorize(state: &DashboardState, headers: &HeaderMap) -> Result<(), ApiError> {
    let expected = format!("Bearer {}", state.token);
    if headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        != Some(expected.as_str())
    {
        return Err(ApiError::Unauthorized);
    }
    if headers
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin != state.origin.as_ref())
    {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

fn validate_text(label: &str, value: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() || value.len() > MAX_TEXT_BYTES {
        Err(ApiError::BadRequest(format!(
            "{label} must contain between 1 and {MAX_TEXT_BYTES} bytes"
        )))
    } else {
        Ok(())
    }
}

fn validate_label(label: &str, value: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() || value.len() > 256 {
        Err(ApiError::BadRequest(format!(
            "{label} must contain between 1 and 256 bytes"
        )))
    } else {
        Ok(())
    }
}

fn validate_resources(resources: &[String]) -> Result<(), ApiError> {
    if resources.is_empty() || resources.len() > 64 {
        return Err(ApiError::BadRequest(
            "resources must contain between 1 and 64 entries".to_owned(),
        ));
    }
    for resource in resources {
        validate_label("resource", resource)?;
    }
    Ok(())
}

async fn blocking<T>(operation: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T, ApiError>
where
    T: Send + 'static,
{
    task::spawn_blocking(operation)
        .await
        .map_err(|_| ApiError::Internal)?
        .map_err(|_| ApiError::Internal)
}

async fn security_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

async fn shutdown_signal() {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install interrupt handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install termination handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {},
        () = terminate => {},
    }
}
