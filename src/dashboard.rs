use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
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
use crate::ide::{ApproveRequest, IdeManager, LaunchRequest, TerminalAttachment};
use crate::store::MeshStore;

const DASHBOARD_HTML: &str = include_str!("../dashboard/index.html");
const DASHBOARD_CSS: &str = include_str!("../dashboard/dashboard.css");
const DASHBOARD_JS: &str = include_str!("../dashboard/dashboard.js");
const TERMINAL_CSS: &str = include_str!("../dashboard/vendor/terminal.css");
const TERMINAL_JS: &str = include_str!("../dashboard/vendor/terminal.js");
const MAX_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone)]
struct DashboardState {
    agent_id: Arc<str>,
    ide: IdeManager,
    origin: Arc<str>,
    store: MeshStore,
    token: Arc<str>,
    workspace: Arc<PathBuf>,
}

#[derive(Debug)]
enum ApiError {
    BadRequest(String),
    Conflict(String),
    Forbidden,
    Internal,
    Unauthorized,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::Conflict(message) => (StatusCode::CONFLICT, message),
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

#[derive(Deserialize)]
struct OutputQuery {
    after: Option<u64>,
}

#[derive(Deserialize)]
struct TerminalInputRequest {
    text: String,
}

#[derive(Deserialize)]
struct FileQuery {
    path: Option<String>,
    session: Option<String>,
}

#[derive(Deserialize)]
struct TicketQuery {
    ticket: String,
}

#[derive(Deserialize)]
struct ResizeMessage {
    cols: u16,
    rows: u16,
    #[serde(rename = "type")]
    kind: String,
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
    let ide = IdeManager::new(store.clone(), workspace.clone());
    let state = DashboardState {
        agent_id: Arc::from(agent_id.clone()),
        ide,
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
        .with_graceful_shutdown(shutdown_signal(state.ide.clone()))
        .await;
    heartbeat.abort();
    state.ide.stop_all();
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
        .route("/terminal.css", get(terminal_styles))
        .route("/terminal.js", get(terminal_script))
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
        .route("/api/v1/ide/providers", get(ide_providers))
        .route("/api/v1/ide/sessions", get(ide_sessions).post(ide_launch))
        .route("/api/v1/ide/sessions/{id}", delete(ide_stop))
        .route("/api/v1/ide/sessions/{id}/output", get(ide_output))
        .route("/api/v1/ide/sessions/{id}/input", post(ide_input))
        .route("/api/v1/ide/sessions/{id}/review", get(ide_review))
        .route("/api/v1/ide/sessions/{id}/approve", post(ide_approve))
        .route("/api/v1/ide/sessions/{id}/reject", post(ide_reject))
        .route(
            "/api/v1/ide/sessions/{id}/attach-ticket",
            post(ide_attach_ticket),
        )
        .route("/api/v1/ide/sessions/{id}/terminal", get(ide_terminal))
        .route("/api/v1/ide/files", get(ide_files))
        .route("/api/v1/ide/file", get(ide_file))
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

async fn terminal_styles() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/css; charset=utf-8")], TERMINAL_CSS)
}

async fn terminal_script() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/javascript; charset=utf-8")],
        TERMINAL_JS,
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

async fn ide_providers(
    State(state): State<DashboardState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    let ide = state.ide.clone();
    Ok(Json(ide_blocking(move || ide.providers()).await?))
}

async fn ide_sessions(
    State(state): State<DashboardState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    let ide = state.ide.clone();
    Ok(Json(ide_blocking(move || ide.sessions()).await?))
}

async fn ide_launch(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Json(request): Json<LaunchRequest>,
) -> Result<Json<Value>, ApiError> {
    authorize_control(&state, &headers)?;
    let ide = state.ide.clone();
    Ok(Json(ide_blocking(move || ide.launch(request)).await?))
}

async fn ide_stop(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    authorize_control(&state, &headers)?;
    let ide = state.ide.clone();
    Ok(Json(ide_blocking(move || ide.stop(&session_id)).await?))
}

async fn ide_output(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(query): Query<OutputQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    let ide = state.ide.clone();
    Ok(Json(
        ide_blocking(move || ide.output(&session_id, query.after.unwrap_or(0))).await?,
    ))
}

async fn ide_input(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(request): Json<TerminalInputRequest>,
) -> Result<Json<Value>, ApiError> {
    authorize_control(&state, &headers)?;
    let ide = state.ide.clone();
    ide_blocking(move || ide.input(&session_id, &request.text)).await?;
    Ok(Json(json!({"written": true})))
}

async fn ide_review(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    let ide = state.ide.clone();
    Ok(Json(ide_blocking(move || ide.review(&session_id)).await?))
}

async fn ide_approve(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(request): Json<ApproveRequest>,
) -> Result<Json<Value>, ApiError> {
    authorize_control(&state, &headers)?;
    let ide = state.ide.clone();
    Ok(Json(
        ide_blocking(move || ide.approve(&session_id, &request)).await?,
    ))
}

async fn ide_reject(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    authorize_control(&state, &headers)?;
    let ide = state.ide.clone();
    Ok(Json(ide_blocking(move || ide.reject(&session_id)).await?))
}

async fn ide_attach_ticket(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    authorize_control(&state, &headers)?;
    let ide = state.ide.clone();
    Ok(Json(
        ide_blocking(move || ide.issue_ticket(&session_id)).await?,
    ))
}

async fn ide_terminal(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(query): Query<TicketQuery>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    authorize_origin(&state, &headers)?;
    let ide = state.ide.clone();
    let attachment = ide_blocking(move || ide.attach(&session_id, &query.ticket)).await?;
    Ok(upgrade
        .on_upgrade(move |socket| terminal_socket(socket, state.ide, attachment))
        .into_response())
}

async fn ide_files(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Query(query): Query<FileQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    let ide = state.ide.clone();
    Ok(Json(
        ide_blocking(move || {
            ide.files(
                query.session.as_deref(),
                query.path.as_deref().unwrap_or("."),
            )
        })
        .await?,
    ))
}

async fn ide_file(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Query(query): Query<FileQuery>,
) -> Result<Json<Value>, ApiError> {
    authorize(&state, &headers)?;
    let path = query
        .path
        .context("file path is required")
        .map_err(|_| ApiError::BadRequest("file path is required".to_owned()))?;
    let ide = state.ide.clone();
    Ok(Json(
        ide_blocking(move || ide.file(query.session.as_deref(), &path)).await?,
    ))
}

async fn terminal_socket(
    mut socket: WebSocket,
    ide: IdeManager,
    mut attachment: TerminalAttachment,
) {
    let session_id = attachment.session["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    for chunk in attachment.backlog {
        if socket
            .send(Message::Binary(chunk.data.into()))
            .await
            .is_err()
        {
            return;
        }
    }
    if socket
        .send(Message::Text(
            json!({"type": "status", "session": attachment.session})
                .to_string()
                .into(),
        ))
        .await
        .is_err()
    {
        return;
    }
    let mut lifecycle = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break; };
                match message {
                    Message::Binary(bytes) => {
                        if bytes.len() > 64 * 1024 || ide.input(
                            &session_id,
                            &String::from_utf8_lossy(&bytes),
                        ).is_err() {
                            break;
                        }
                    }
                    Message::Text(text) => {
                        let Ok(message) = serde_json::from_str::<ResizeMessage>(&text) else { continue; };
                        if message.kind == "resize" {
                            let _ = ide.resize(
                                &session_id,
                                message.cols,
                                message.rows,
                            );
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(bytes) => {
                        if socket.send(Message::Pong(bytes)).await.is_err() { break; }
                    }
                    Message::Pong(_) => {}
                }
            }
            output = attachment.receiver.recv() => {
                match output {
                    Ok(chunk) => {
                        if socket.send(Message::Binary(chunk.data.into())).await.is_err() { break; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let _ = socket.send(Message::Text(json!({"type": "gap"}).to_string().into())).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = lifecycle.tick() => {
                if !ide.session_is_live(&session_id).unwrap_or(false) {
                    let _ = socket.send(Message::Text(json!({"type": "status", "state": "closed"}).to_string().into())).await;
                    break;
                }
            }
        }
    }
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

fn authorize_control(state: &DashboardState, headers: &HeaderMap) -> Result<(), ApiError> {
    authorize(state, headers)?;
    authorize_origin(state, headers)
}

fn authorize_origin(state: &DashboardState, headers: &HeaderMap) -> Result<(), ApiError> {
    if headers.get(ORIGIN).and_then(|value| value.to_str().ok()) != Some(state.origin.as_ref()) {
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

async fn ide_blocking<T>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T, ApiError>
where
    T: Send + 'static,
{
    task::spawn_blocking(operation)
        .await
        .map_err(|_| ApiError::Internal)?
        .map_err(|error| ApiError::Conflict(format!("{error:#}")))
}

async fn security_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

async fn shutdown_signal(ide: IdeManager) {
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
    let _ = task::spawn_blocking(move || ide.stop_all()).await;
}
