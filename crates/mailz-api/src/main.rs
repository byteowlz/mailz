use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, patch, post},
};
use chrono::{DateTime, Utc};
use clap::{Args, Parser};
use futures_util::SinkExt;
use log::info;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use utoipa::{IntoParams, OpenApi, ToSchema};

use mailz_core::{
    AppConfig, AppPaths, CreateAgent, CreateFileReservation, FileReservation,
    FileReservationResult, GcSummary, MessageView, SendMessage, Storage, load_admin_key,
    save_admin_key,
};

fn main() {
    if let Err(err) = try_main() {
        let _ = writeln!(io::stderr(), "{err:?}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn try_main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();
    let paths = AppPaths::discover(cli.common.config)?;
    let config = AppConfig::load(&paths, false)?;
    paths.ensure_directories()?;
    let storage = Storage::open(&paths.database_file())?;
    let admin_key = resolve_admin_key(&paths, &config)?;
    let rate_limit = config.api.rate_limit_per_minute;

    let state = AppState {
        config: Arc::new(config),
        storage: Arc::new(Mutex::new(storage)),
        admin_key,
        rate_limiter: Arc::new(RateLimiter::new(rate_limit)),
    };
    let auth_layer = middleware::from_fn_with_state(state.clone(), auth_middleware);
    spawn_gc_task(state.clone());

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(root))
        .route("/health", get(health))
        .route("/config", get(get_config))
        .route("/openapi.json", get(openapi))
        .route("/projects", get(list_projects).post(create_project))
        .route("/projects/{id}", get(get_project).delete(delete_project))
        .route(
            "/projects/{id}/agents",
            get(list_agents).post(register_agent),
        )
        .route("/agents/{id}", get(get_agent))
        .route("/agents/{id}/inbox", get(get_inbox))
        .route("/messages", post(send_message))
        .route("/messages/{id}", get(get_message))
        .route("/messages/{id}/read", patch(mark_read))
        .route("/messages/{id}/ack", patch(ack_message))
        .route("/messages/search", get(search_messages))
        .route("/agents/{id}/keys", post(issue_api_key))
        .route("/ws/{agent_id}", get(ws_handler))
        .route(
            "/reservations",
            get(list_reservations).post(create_reservation),
        )
        .route(
            "/reservations/{id}",
            get(get_reservation).delete(delete_reservation),
        )
        .route("/reservations/check", post(check_reservations))
        .layer(auth_layer)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], cli.common.port));
    info!("Starting API server on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

#[derive(Debug, Parser)]
#[command(author, version, about = "HTTP API server for mailz")]
struct Cli {
    #[command(flatten)]
    common: CommonOpts,
}

#[derive(Debug, Clone, Args)]
struct CommonOpts {
    /// Override the config file path
    #[arg(long, value_name = "PATH")]
    config: Option<std::path::PathBuf>,

    /// Port to listen on
    #[arg(short, long, default_value = "3000")]
    port: u16,
}

#[derive(Clone)]
struct AppState {
    config: Arc<AppConfig>,
    storage: Arc<Mutex<Storage>>,
    admin_key: String,
    rate_limiter: Arc<RateLimiter>,
}

#[derive(Clone, Copy, Debug)]
struct AuthContext {
    agent_id: Option<i64>,
    is_admin: bool,
}

struct RateLimiter {
    limit_per_minute: u64,
    buckets: Mutex<HashMap<i64, VecDeque<Instant>>>,
}

impl RateLimiter {
    fn new(limit_per_minute: u64) -> Self {
        Self {
            limit_per_minute,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn check(&self, agent_id: i64) -> bool {
        if self.limit_per_minute == 0 {
            return true;
        }
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let bucket = buckets.entry(agent_id).or_default();
        while bucket
            .front()
            .map(|t| now.duration_since(*t) > window)
            .unwrap_or(false)
        {
            bucket.pop_front();
        }
        if bucket.len() as u64 >= self.limit_per_minute {
            return false;
        }
        bucket.push_back(now);
        true
    }
}

#[derive(Serialize, ToSchema)]
struct RootResponse {
    name: &'static str,
    version: &'static str,
}

#[derive(Serialize, ToSchema)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize, ToSchema)]
struct ErrorResponse {
    error: String,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct ProjectCreateRequest {
    path: String,
    name: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct AgentRegisterRequest {
    name: Option<String>,
    program: Option<String>,
    model: Option<String>,
    task_description: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct SendMessageRequest {
    project_id: i64,
    sender_name: String,
    to: Vec<String>,
    cc: Option<Vec<String>>,
    bcc: Option<Vec<String>>,
    subject: String,
    body: String,
    importance: Option<String>,
    ack_required: Option<bool>,
    thread_id: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct ReservationCreateRequest {
    project_id: i64,
    agent_name: String,
    paths: Vec<String>,
    ttl_seconds: Option<u64>,
    exclusive: Option<bool>,
    reason: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct ReservationCheckRequest {
    project_id: i64,
    paths: Vec<String>,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct ReservationResponse {
    id: i64,
    project_id: i64,
    agent_id: i64,
    path_pattern: String,
    exclusive: bool,
    reason: Option<String>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    released_at: Option<DateTime<Utc>>,
}

impl From<FileReservation> for ReservationResponse {
    fn from(value: FileReservation) -> Self {
        Self {
            id: value.id,
            project_id: value.project_id,
            agent_id: value.agent_id,
            path_pattern: value.path_pattern,
            exclusive: value.exclusive,
            reason: value.reason,
            created_at: value.created_at,
            expires_at: value.expires_at,
            released_at: value.released_at,
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
struct ReservationResultResponse {
    granted: Vec<ReservationResponse>,
    conflicts: Vec<ReservationResponse>,
}

impl From<FileReservationResult> for ReservationResultResponse {
    fn from(value: FileReservationResult) -> Self {
        Self {
            granted: value
                .granted
                .into_iter()
                .map(ReservationResponse::from)
                .collect(),
            conflicts: value
                .conflicts
                .into_iter()
                .map(ReservationResponse::from)
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
struct ProjectResponse {
    id: i64,
    slug: String,
    path: String,
    created_at: DateTime<Utc>,
}

impl From<mailz_core::Project> for ProjectResponse {
    fn from(value: mailz_core::Project) -> Self {
        Self {
            id: value.id,
            slug: value.slug,
            path: value.path,
            created_at: value.created_at,
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
struct AgentResponse {
    id: i64,
    project_id: i64,
    name: String,
    program: String,
    model: String,
    task_description: Option<String>,
    created_at: DateTime<Utc>,
    last_active_at: DateTime<Utc>,
}

impl From<mailz_core::Agent> for AgentResponse {
    fn from(value: mailz_core::Agent) -> Self {
        Self {
            id: value.id,
            project_id: value.project_id,
            name: value.name,
            program: value.program,
            model: value.model,
            task_description: value.task_description,
            created_at: value.created_at,
            last_active_at: value.last_active_at,
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
struct ApiKeyResponse {
    api_key: String,
    id: i64,
    agent_id: i64,
    key_prefix: String,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, Deserialize, ToSchema)]
struct MessageResponse {
    id: i64,
    sender: String,
    recipients: Vec<String>,
    subject: String,
    body: String,
    importance: String,
    ack_required: bool,
    thread_id: Option<String>,
    created_at: DateTime<Utc>,
    read_at: Option<DateTime<Utc>>,
    ack_at: Option<DateTime<Utc>>,
}

impl From<MessageView> for MessageResponse {
    fn from(value: MessageView) -> Self {
        Self {
            id: value.id,
            sender: value.sender,
            recipients: value.recipients,
            subject: value.subject,
            body: value.body,
            importance: value.importance.as_str().to_string(),
            ack_required: value.ack_required,
            thread_id: value.thread_id,
            created_at: value.created_at,
            read_at: value.read_at,
            ack_at: value.ack_at,
        }
    }
}

#[derive(Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
struct InboxQuery {
    limit: Option<usize>,
    offset: Option<usize>,
    unread: Option<bool>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
}

#[derive(Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
struct MessageQuery {
    agent_id: Option<i64>,
}

#[derive(Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
struct MessageActionQuery {
    agent_id: i64,
}

#[derive(Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
struct SearchQuery {
    q: String,
    project_id: i64,
    agent_id: Option<i64>,
    unread: Option<bool>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
struct ReservationListQuery {
    project_id: i64,
    agent_id: Option<i64>,
    file: Option<String>,
}

#[derive(OpenApi)]
#[openapi(
    paths(
        root,
        health,
        get_config,
        list_projects,
        create_project,
        get_project,
        delete_project,
        register_agent,
        list_agents,
        get_agent,
        issue_api_key,
        send_message,
        get_inbox,
        get_message,
        mark_read,
        ack_message,
        search_messages,
        create_reservation,
        list_reservations,
        get_reservation,
        delete_reservation,
        check_reservations
    ),
    components(schemas(
        RootResponse,
        HealthResponse,
        ErrorResponse,
        ProjectCreateRequest,
        ProjectResponse,
        AgentRegisterRequest,
        AgentResponse,
        ApiKeyResponse,
        SendMessageRequest,
        MessageResponse,
        ReservationCreateRequest,
        ReservationCheckRequest,
        ReservationResponse,
        ReservationResultResponse,
        InboxQuery,
        MessageQuery,
        MessageActionQuery,
        SearchQuery,
        ReservationListQuery
    ))
)]
struct ApiDoc;

fn resolve_admin_key(paths: &AppPaths, config: &AppConfig) -> Result<String> {
    if let Some(key) = config.api.admin_key.clone() {
        return Ok(key);
    }
    if let Some(key) = load_admin_key(paths)? {
        return Ok(key);
    }

    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let key = format!("adm_{}", hex::encode(bytes));
    save_admin_key(paths, &key)?;
    info!(
        "generated admin key at {}",
        paths.admin_key_file().display()
    );
    Ok(key)
}

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

#[utoipa::path(
    get,
    path = "/",
    responses(
        (status = 200, description = "Root", body = RootResponse)
    )
)]
async fn root() -> Json<RootResponse> {
    Json(RootResponse {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Health", body = HealthResponse)
    )
)]
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[utoipa::path(
    get,
    path = "/config",
    responses(
        (status = 200, description = "Config")
    )
)]
async fn get_config(State(state): State<AppState>) -> Result<Json<AppConfig>, StatusCode> {
    Ok(Json((*state.config).clone()))
}

#[utoipa::path(
    get,
    path = "/projects",
    responses(
        (status = 200, description = "List projects", body = [ProjectResponse])
    )
)]
async fn list_projects(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<ProjectResponse>>> {
    ensure_admin(&auth)?;
    let projects = with_storage(state, move |storage| storage.list_projects()).await?;
    Ok(Json(
        projects.into_iter().map(ProjectResponse::from).collect(),
    ))
}

#[utoipa::path(
    post,
    path = "/projects",
    request_body = ProjectCreateRequest,
    responses(
        (status = 200, description = "Create project", body = ProjectResponse),
        (status = 409, description = "Project conflict", body = ErrorResponse)
    )
)]
async fn create_project(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Json(payload): Json<ProjectCreateRequest>,
) -> ApiResult<Json<ProjectResponse>> {
    ensure_admin(&auth)?;
    let project = with_storage(state, move |storage| {
        storage.create_project(&payload.path, payload.name.as_deref())
    })
    .await
    .map_err(|err| err.with_status(StatusCode::CONFLICT))?;
    Ok(Json(ProjectResponse::from(project)))
}

#[utoipa::path(
    get,
    path = "/projects/{id}",
    params(("id" = i64, Path, description = "Project id")),
    responses(
        (status = 200, description = "Project details", body = ProjectResponse),
        (status = 404, description = "Project not found", body = ErrorResponse)
    )
)]
async fn get_project(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<ProjectResponse>> {
    ensure_admin(&auth)?;
    let project = with_storage(state, move |storage| storage.get_project_by_id(id)).await?;
    let Some(project) = project else {
        return Err(ApiError::not_found("project not found"));
    };
    Ok(Json(ProjectResponse::from(project)))
}

#[utoipa::path(
    delete,
    path = "/projects/{id}",
    params(("id" = i64, Path, description = "Project id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Project not found", body = ErrorResponse)
    )
)]
async fn delete_project(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    ensure_admin(&auth)?;
    let deleted = with_storage(state, move |storage| storage.delete_project_by_id(id)).await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("project not found"))
    }
}

#[utoipa::path(
    post,
    path = "/projects/{id}/agents",
    params(("id" = i64, Path, description = "Project id")),
    request_body = AgentRegisterRequest,
    responses(
        (status = 200, description = "Agent registered", body = AgentResponse),
        (status = 404, description = "Project not found", body = ErrorResponse)
    )
)]
async fn register_agent(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<AgentRegisterRequest>,
) -> ApiResult<Json<AgentResponse>> {
    ensure_admin(&auth)?;
    let agent = with_storage(state, move |storage| {
        let project = storage
            .get_project_by_id(id)?
            .ok_or_else(|| anyhow::anyhow!("project not found"))?;
        storage.register_agent(
            project.id,
            &CreateAgent {
                name: payload.name,
                program: payload.program.unwrap_or_else(|| "mailz-api".to_string()),
                model: payload.model.unwrap_or_else(|| "unknown".to_string()),
                task_description: payload.task_description,
            },
        )
    })
    .await
    .map_err(|err| err.with_status(StatusCode::NOT_FOUND))?;

    Ok(Json(AgentResponse::from(agent)))
}

#[utoipa::path(
    get,
    path = "/projects/{id}/agents",
    params(("id" = i64, Path, description = "Project id")),
    responses(
        (status = 200, description = "List agents", body = [AgentResponse]),
        (status = 404, description = "Project not found", body = ErrorResponse)
    )
)]
async fn list_agents(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<AgentResponse>>> {
    ensure_admin(&auth)?;
    let agents = with_storage(state, move |storage| {
        storage
            .get_project_by_id(id)?
            .ok_or_else(|| anyhow::anyhow!("project not found"))?;
        storage.list_agents(id)
    })
    .await
    .map_err(|err| err.with_status(StatusCode::NOT_FOUND))?;

    Ok(Json(agents.into_iter().map(AgentResponse::from).collect()))
}

#[utoipa::path(
    get,
    path = "/agents/{id}",
    params(("id" = i64, Path, description = "Agent id")),
    responses(
        (status = 200, description = "Agent details", body = AgentResponse),
        (status = 404, description = "Agent not found", body = ErrorResponse)
    )
)]
async fn get_agent(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<AgentResponse>> {
    ensure_admin(&auth)?;
    let agent = with_storage(state, move |storage| storage.get_agent_by_id(id)).await?;
    let Some(agent) = agent else {
        return Err(ApiError::not_found("agent not found"));
    };
    Ok(Json(AgentResponse::from(agent)))
}

#[utoipa::path(
    post,
    path = "/agents/{id}/keys",
    params(("id" = i64, Path, description = "Agent id")),
    responses(
        (status = 200, description = "API key issued", body = ApiKeyResponse),
        (status = 404, description = "Agent not found", body = ErrorResponse)
    )
)]
async fn issue_api_key(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<ApiKeyResponse>> {
    ensure_admin(&auth)?;
    let issued = with_storage(state, move |storage| storage.issue_api_key(id))
        .await
        .map_err(|err| err.with_status(StatusCode::NOT_FOUND))?;
    Ok(Json(ApiKeyResponse {
        api_key: issued.api_key,
        id: issued.record.id,
        agent_id: issued.record.agent_id,
        key_prefix: issued.record.key_prefix,
        created_at: issued.record.created_at,
        last_used_at: issued.record.last_used_at,
    }))
}

#[utoipa::path(
    post,
    path = "/messages",
    request_body = SendMessageRequest,
    responses(
        (status = 200, description = "Message sent", body = MessageResponse),
        (status = 404, description = "Missing sender or recipients", body = ErrorResponse)
    )
)]
async fn send_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Json(payload): Json<SendMessageRequest>,
) -> ApiResult<Json<MessageResponse>> {
    if !auth.is_admin {
        let agent = require_agent(&state, auth).await?;
        if agent.project_id != payload.project_id {
            return Err(ApiError::forbidden("project mismatch"));
        }
        if agent.name != payload.sender_name {
            return Err(ApiError::forbidden("sender mismatch"));
        }
    }
    let importance = match payload.importance.as_deref() {
        Some(value) => Some(value.parse().map_err(|err| ApiError {
            status: StatusCode::BAD_REQUEST,
            message: err,
            body: None,
        })?),
        None => None,
    };
    let message = with_storage(state.clone(), move |storage| {
        storage.send_message(
            payload.project_id,
            &SendMessage {
                sender_name: payload.sender_name,
                to: payload.to,
                cc: payload.cc,
                bcc: payload.bcc,
                subject: payload.subject,
                body: payload.body,
                importance,
                ack_required: payload.ack_required,
                thread_id: payload.thread_id,
            },
        )
    })
    .await
    .map_err(|err| err.with_status(StatusCode::NOT_FOUND))?;

    let message = with_storage(state, move |storage| {
        storage.get_message_view_unscoped(message.id)
    })
    .await?
    .ok_or_else(|| ApiError::not_found("message not found"))?;

    Ok(Json(MessageResponse::from(message)))
}

#[utoipa::path(
    get,
    path = "/agents/{id}/inbox",
    params(("id" = i64, Path, description = "Agent id"), InboxQuery),
    responses(
        (status = 200, description = "Inbox", body = [MessageResponse]),
        (status = 404, description = "Agent not found", body = ErrorResponse)
    )
)]
async fn get_inbox(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<InboxQuery>,
) -> ApiResult<Json<Vec<MessageResponse>>> {
    ensure_agent_match(&auth, id)?;
    let limit = query.limit.unwrap_or(50);
    let offset = query.offset.unwrap_or(0);
    let unread = query.unread.unwrap_or(false);

    let messages = with_storage(state, move |storage| {
        let agent = storage
            .get_agent_by_id(id)?
            .ok_or_else(|| anyhow::anyhow!("agent not found"))?;
        storage.fetch_inbox_for_agent(
            agent.project_id,
            agent.id,
            limit,
            offset,
            unread,
            query.start,
            query.end,
        )
    })
    .await
    .map_err(|err| err.with_status(StatusCode::NOT_FOUND))?;

    Ok(Json(
        messages.into_iter().map(MessageResponse::from).collect(),
    ))
}

#[utoipa::path(
    get,
    path = "/messages/{id}",
    params(("id" = i64, Path, description = "Message id"), MessageQuery),
    responses(
        (status = 200, description = "Message", body = MessageResponse),
        (status = 404, description = "Message not found", body = ErrorResponse)
    )
)]
async fn get_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<MessageQuery>,
) -> ApiResult<Json<MessageResponse>> {
    let agent_id = match query.agent_id {
        Some(agent_id) => {
            ensure_agent_match(&auth, agent_id)?;
            Some(agent_id)
        }
        None => {
            ensure_admin(&auth)?;
            None
        }
    };
    let message = with_storage(state, move |storage| {
        if let Some(agent_id) = agent_id {
            let agent = storage
                .get_agent_by_id(agent_id)?
                .ok_or_else(|| anyhow::anyhow!("agent not found"))?;
            storage.get_message_view(agent.project_id, agent.id, id)
        } else {
            storage.get_message_view_unscoped(id)
        }
    })
    .await
    .map_err(|err| err.with_status(StatusCode::NOT_FOUND))?;

    let Some(message) = message else {
        return Err(ApiError::not_found("message not found"));
    };
    Ok(Json(MessageResponse::from(message)))
}

#[utoipa::path(
    patch,
    path = "/messages/{id}/read",
    params(("id" = i64, Path, description = "Message id"), MessageActionQuery),
    responses(
        (status = 200, description = "Marked read", body = MessageResponse),
        (status = 404, description = "Message not found", body = ErrorResponse)
    )
)]
async fn mark_read(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<MessageActionQuery>,
) -> ApiResult<Json<MessageResponse>> {
    ensure_agent_match(&auth, query.agent_id)?;
    let message = with_storage(state, move |storage| {
        let agent = storage
            .get_agent_by_id(query.agent_id)?
            .ok_or_else(|| anyhow::anyhow!("agent not found"))?;
        storage.mark_read(agent.id, id)?;
        storage.get_message_view(agent.project_id, agent.id, id)
    })
    .await
    .map_err(|err| err.with_status(StatusCode::NOT_FOUND))?;

    let Some(message) = message else {
        return Err(ApiError::not_found("message not found"));
    };
    Ok(Json(MessageResponse::from(message)))
}

#[utoipa::path(
    patch,
    path = "/messages/{id}/ack",
    params(("id" = i64, Path, description = "Message id"), MessageActionQuery),
    responses(
        (status = 200, description = "Acknowledged", body = MessageResponse),
        (status = 404, description = "Message not found", body = ErrorResponse)
    )
)]
async fn ack_message(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<MessageActionQuery>,
) -> ApiResult<Json<MessageResponse>> {
    ensure_agent_match(&auth, query.agent_id)?;
    let message = with_storage(state, move |storage| {
        let agent = storage
            .get_agent_by_id(query.agent_id)?
            .ok_or_else(|| anyhow::anyhow!("agent not found"))?;
        storage.acknowledge(agent.id, id)?;
        storage.get_message_view(agent.project_id, agent.id, id)
    })
    .await
    .map_err(|err| err.with_status(StatusCode::NOT_FOUND))?;

    let Some(message) = message else {
        return Err(ApiError::not_found("message not found"));
    };
    Ok(Json(MessageResponse::from(message)))
}

#[utoipa::path(
    get,
    path = "/messages/search",
    params(SearchQuery),
    responses(
        (status = 200, description = "Search results", body = [MessageResponse]),
        (status = 400, description = "Invalid parameters", body = ErrorResponse)
    )
)]
async fn search_messages(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> ApiResult<Json<Vec<MessageResponse>>> {
    let agent_id = match query.agent_id {
        Some(agent_id) => {
            ensure_agent_match(&auth, agent_id)?;
            Some(agent_id)
        }
        None => {
            ensure_admin(&auth)?;
            None
        }
    };
    let limit = query.limit.unwrap_or(50);
    let offset = query.offset.unwrap_or(0);
    let unread = query.unread.unwrap_or(false);

    let messages = with_storage(state, move |storage| {
        storage.search_messages_with_filters(
            query.project_id,
            &query.q,
            limit,
            offset,
            agent_id,
            unread,
            query.start,
            query.end,
        )
    })
    .await
    .map_err(|err| err.with_status(StatusCode::BAD_REQUEST))?;

    Ok(Json(
        messages.into_iter().map(MessageResponse::from).collect(),
    ))
}

#[utoipa::path(
    post,
    path = "/reservations",
    request_body = ReservationCreateRequest,
    responses(
        (status = 200, description = "Reservation created", body = ReservationResultResponse),
        (status = 409, description = "Conflicts found", body = ReservationResultResponse)
    )
)]
async fn create_reservation(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Json(payload): Json<ReservationCreateRequest>,
) -> ApiResult<Json<ReservationResultResponse>> {
    if !auth.is_admin {
        let agent = require_agent(&state, auth).await?;
        if agent.project_id != payload.project_id {
            return Err(ApiError::forbidden("project mismatch"));
        }
        if agent.name != payload.agent_name {
            return Err(ApiError::forbidden("agent mismatch"));
        }
    }
    let result = with_storage(state, move |storage| {
        storage.create_file_reservations(
            payload.project_id,
            &CreateFileReservation {
                agent_name: payload.agent_name,
                paths: payload.paths,
                ttl_seconds: payload.ttl_seconds,
                exclusive: payload.exclusive,
                reason: payload.reason,
            },
        )
    })
    .await?;

    let response = ReservationResultResponse::from(result);
    if !response.conflicts.is_empty() {
        return Err(ApiError::with_body(StatusCode::CONFLICT, response));
    }

    Ok(Json(response))
}

#[utoipa::path(
    get,
    path = "/reservations",
    params(ReservationListQuery),
    responses(
        (status = 200, description = "Reservations", body = [ReservationResponse])
    )
)]
async fn list_reservations(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Query(query): Query<ReservationListQuery>,
) -> ApiResult<Json<Vec<ReservationResponse>>> {
    if !auth.is_admin {
        let agent = require_agent(&state, auth).await?;
        if agent.project_id != query.project_id {
            return Err(ApiError::forbidden("project mismatch"));
        }
    }
    let reservations = with_storage(state, move |storage| {
        storage.list_active_reservations(query.project_id, query.agent_id, query.file.as_deref())
    })
    .await?;

    Ok(Json(
        reservations
            .into_iter()
            .map(ReservationResponse::from)
            .collect(),
    ))
}

#[utoipa::path(
    get,
    path = "/reservations/{id}",
    params(("id" = i64, Path, description = "Reservation id")),
    responses(
        (status = 200, description = "Reservation details", body = ReservationResponse),
        (status = 404, description = "Reservation not found", body = ErrorResponse)
    )
)]
async fn get_reservation(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<ReservationResponse>> {
    ensure_admin(&auth)?;
    let reservation = with_storage(state, move |storage| storage.get_reservation_by_id(id)).await?;
    let Some(reservation) = reservation else {
        return Err(ApiError::not_found("reservation not found"));
    };
    Ok(Json(ReservationResponse::from(reservation)))
}

#[utoipa::path(
    delete,
    path = "/reservations/{id}",
    params(("id" = i64, Path, description = "Reservation id")),
    responses(
        (status = 204, description = "Released"),
        (status = 404, description = "Reservation not found", body = ErrorResponse)
    )
)]
async fn delete_reservation(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    ensure_admin(&auth)?;
    let released = with_storage(state, move |storage| {
        storage.release_reservation_by_id_any(id)
    })
    .await?;
    if released {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("reservation not found"))
    }
}

#[utoipa::path(
    post,
    path = "/reservations/check",
    request_body = ReservationCheckRequest,
    responses(
        (status = 200, description = "Conflicts", body = [ReservationResponse])
    )
)]
async fn check_reservations(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Json(payload): Json<ReservationCheckRequest>,
) -> ApiResult<Json<Vec<ReservationResponse>>> {
    if !auth.is_admin {
        let agent = require_agent(&state, auth).await?;
        if agent.project_id != payload.project_id {
            return Err(ApiError::forbidden("project mismatch"));
        }
    }
    let conflicts = with_storage(state, move |storage| {
        storage.check_reservations(payload.project_id, &payload.paths)
    })
    .await?;

    Ok(Json(
        conflicts
            .into_iter()
            .map(ReservationResponse::from)
            .collect(),
    ))
}

async fn ws_handler(
    Extension(auth): Extension<AuthContext>,
    State(state): State<AppState>,
    Path(agent_id): Path<i64>,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    if let Err(err) = ensure_agent_match(&auth, agent_id) {
        return err.into_response();
    }
    ws.on_upgrade(move |socket| handle_ws(socket, state, agent_id))
        .into_response()
}

#[derive(Serialize)]
struct WsEvent {
    kind: String,
    timestamp: DateTime<Utc>,
    data: serde_json::Value,
}

async fn handle_ws(mut socket: WebSocket, state: AppState, agent_id: i64) {
    let agent = match with_storage_any(state.clone(), move |storage| {
        storage
            .get_agent_by_id(agent_id)?
            .ok_or_else(|| anyhow::anyhow!("agent not found"))
    })
    .await
    {
        Ok(agent) => agent,
        Err(_) => {
            let _ = socket.close().await;
            return;
        }
    };

    let poll_interval = Duration::from_secs(state.config.watch.poll_interval_seconds.max(1));
    let mut ticker = tokio::time::interval(poll_interval);
    let mut last_seen = Utc::now();
    let mut conflict_paths = std::collections::HashSet::new();
    let mut expiring_reservations = std::collections::HashSet::new();

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Ok(events) = poll_ws_events(&state, &agent, &mut last_seen, &mut conflict_paths, &mut expiring_reservations).await {
                    for event in events {
                        if socket.send(Message::Text(event.into())).await.is_err() {
                            return;
                        }
                    }
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => return,
                }
            }
        }
    }
}

async fn poll_ws_events(
    state: &AppState,
    agent: &mailz_core::Agent,
    last_seen: &mut DateTime<Utc>,
    conflict_paths: &mut std::collections::HashSet<String>,
    expiring_reservations: &mut std::collections::HashSet<i64>,
) -> Result<Vec<String>> {
    let agent_id = agent.id;
    let project_id = agent.project_id;
    let since = *last_seen;
    let poll_result = with_storage_any(state.clone(), move |storage| {
        let messages =
            storage.fetch_inbox_for_agent(project_id, agent_id, 50, 0, false, Some(since), None)?;
        let reservations = storage.list_active_reservations(project_id, None, None)?;
        Ok((messages, reservations))
    })
    .await?;

    let (messages, reservations) = poll_result;
    let mut payloads = Vec::new();

    let mut max_seen = *last_seen;
    for message in messages {
        if message.created_at > max_seen {
            max_seen = message.created_at;
        }
        let event = WsEvent {
            kind: "message".to_string(),
            timestamp: Utc::now(),
            data: serde_json::json!({
                "id": message.id,
                "sender": message.sender,
                "subject": message.subject,
                "created_at": message.created_at,
            }),
        };
        payloads.push(serde_json::to_string(&event)?);
    }
    if max_seen > *last_seen {
        *last_seen = max_seen + chrono::Duration::milliseconds(1);
    }

    let mut path_map: HashMap<String, Vec<&FileReservation>> = HashMap::new();
    for reservation in &reservations {
        path_map
            .entry(reservation.path_pattern.clone())
            .or_default()
            .push(reservation);
    }

    let current_conflicts: std::collections::HashSet<String> = path_map
        .iter()
        .filter_map(|(path, items)| {
            if items.len() > 1 && items.iter().any(|res| res.agent_id == agent_id) {
                Some(path.clone())
            } else {
                None
            }
        })
        .collect();

    for path in current_conflicts.difference(conflict_paths) {
        let event = WsEvent {
            kind: "reservation_conflict".to_string(),
            timestamp: Utc::now(),
            data: serde_json::json!({
                "path": path,
                "conflicts": path_map.get(path).unwrap_or(&Vec::new()).iter().map(|res| {
                    serde_json::json!({
                        "id": res.id,
                        "agent_id": res.agent_id,
                        "exclusive": res.exclusive,
                        "expires_at": res.expires_at,
                    })
                }).collect::<Vec<_>>(),
            }),
        };
        payloads.push(serde_json::to_string(&event)?);
    }
    *conflict_paths = current_conflicts;

    let now = Utc::now();
    let current_expiring: std::collections::HashSet<i64> = reservations
        .iter()
        .filter_map(|reservation| {
            if reservation.agent_id != agent_id {
                return None;
            }
            let remaining = reservation.expires_at - now;
            if remaining.num_minutes() <= 5 && remaining.num_minutes() >= 0 {
                Some(reservation.id)
            } else {
                None
            }
        })
        .collect();

    for id in current_expiring.difference(expiring_reservations) {
        if let Some(reservation) = reservations.iter().find(|res| res.id == *id) {
            let event = WsEvent {
                kind: "reservation_expiring".to_string(),
                timestamp: Utc::now(),
                data: serde_json::json!({
                    "id": reservation.id,
                    "path": reservation.path_pattern,
                    "expires_at": reservation.expires_at,
                }),
            };
            payloads.push(serde_json::to_string(&event)?);
        }
    }
    *expiring_reservations = current_expiring;

    Ok(payloads)
}

struct ApiError {
    status: StatusCode,
    message: String,
    body: Option<ReservationResultResponse>,
}

impl ApiError {
    fn not_found(message: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.to_string(),
            body: None,
        }
    }

    fn forbidden(message: &str) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.to_string(),
            body: None,
        }
    }

    fn with_body(status: StatusCode, body: ReservationResultResponse) -> Self {
        Self {
            status,
            message: String::new(),
            body: Some(body),
        }
    }

    fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        if let Some(body) = self.body {
            return (self.status, Json(body)).into_response();
        }
        let payload = ErrorResponse {
            error: self.message,
        };
        (self.status, Json(payload)).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn ensure_admin(auth: &AuthContext) -> ApiResult<()> {
    if auth.is_admin {
        Ok(())
    } else {
        Err(ApiError::forbidden("admin required"))
    }
}

fn ensure_agent_match(auth: &AuthContext, agent_id: i64) -> ApiResult<()> {
    if auth.is_admin || auth.agent_id == Some(agent_id) {
        Ok(())
    } else {
        Err(ApiError::forbidden("agent mismatch"))
    }
}

async fn require_agent(state: &AppState, auth: AuthContext) -> ApiResult<mailz_core::Agent> {
    let agent_id = auth
        .agent_id
        .ok_or_else(|| ApiError::forbidden("agent key required"))?;
    with_storage(state.clone(), move |storage| {
        storage
            .get_agent_by_id(agent_id)?
            .ok_or_else(|| anyhow::anyhow!("agent not found"))
    })
    .await
    .map_err(|err| err.with_status(StatusCode::NOT_FOUND))
}

async fn with_storage_any<T, F>(state: AppState, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Storage) -> Result<T> + Send + 'static,
{
    let storage = state.storage.clone();
    tokio::task::spawn_blocking(move || {
        let guard = storage
            .lock()
            .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
        f(&guard)
    })
    .await
    .map_err(|err| anyhow::anyhow!(err.to_string()))?
}

async fn with_storage<T, F>(state: AppState, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Storage) -> Result<T> + Send + 'static,
{
    let storage = state.storage.clone();
    tokio::task::spawn_blocking(move || {
        let guard = storage.lock().map_err(|_| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "storage lock poisoned".to_string(),
            body: None,
        })?;
        f(&guard).map_err(|err| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: err.to_string(),
            body: None,
        })
    })
    .await
    .map_err(|err| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: err.to_string(),
        body: None,
    })?
}

async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Result<axum::response::Response, (StatusCode, Json<ErrorResponse>)> {
    let key = extract_api_key(req.headers()).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "missing X-API-Key".to_string(),
            }),
        )
    })?;

    if key == state.admin_key {
        req.extensions_mut().insert(AuthContext {
            agent_id: None,
            is_admin: true,
        });
        return Ok(next.run(req).await);
    }

    let agent_id = with_storage_any(state.clone(), move |storage| storage.verify_api_key(&key))
        .await
        .map_err(|err| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: err.to_string(),
                }),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(ErrorResponse {
                    error: "invalid API key".to_string(),
                }),
            )
        })?;

    if !state.rate_limiter.check(agent_id) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse {
                error: "rate limit exceeded".to_string(),
            }),
        ));
    }

    req.extensions_mut().insert(AuthContext {
        agent_id: Some(agent_id),
        is_admin: false,
    });
    Ok(next.run(req).await)
}

fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())?
        .trim()
        .to_string();
    if value.is_empty() { None } else { Some(value) }
}

fn spawn_gc_task(state: AppState) {
    let retention_days = state.config.maintenance.message_retention_days;
    let interval_seconds = state.config.maintenance.gc_interval_seconds;
    if interval_seconds == 0 {
        return;
    }
    let storage = state.storage.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(interval_seconds));
        loop {
            ticker.tick().await;
            match run_gc(storage.clone(), retention_days).await {
                Ok(summary) => {
                    if summary.expired_reservations > 0 || summary.deleted_messages > 0 {
                        info!(
                            "gc: expired {} reservations, deleted {} messages",
                            summary.expired_reservations, summary.deleted_messages
                        );
                    }
                }
                Err(err) => {
                    log::warn!("gc failed: {err}");
                }
            }
        }
    });
}

async fn run_gc(storage: Arc<Mutex<Storage>>, retention_days: u64) -> Result<GcSummary> {
    tokio::task::spawn_blocking(move || {
        let guard = storage
            .lock()
            .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
        guard.run_gc(retention_days)
    })
    .await
    .map_err(|err| anyhow::anyhow!(err.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_app(rate_limit: u64) -> Router {
        let storage = Storage::open_memory().unwrap();
        let mut config = AppConfig::default();
        config.api.admin_key = Some("test-admin".to_string());
        config.api.rate_limit_per_minute = rate_limit;
        let state = AppState {
            config: Arc::new(config),
            storage: Arc::new(Mutex::new(storage)),
            admin_key: "test-admin".to_string(),
            rate_limiter: Arc::new(RateLimiter::new(rate_limit)),
        };
        let auth_layer = middleware::from_fn_with_state(state.clone(), auth_middleware);

        Router::new()
            .route("/projects", post(create_project).get(list_projects))
            .route("/projects/{id}/agents", post(register_agent))
            .route("/agents/{id}/keys", post(issue_api_key))
            .route("/agents/{id}/inbox", get(get_inbox))
            .route("/messages", post(send_message))
            .route("/messages/{id}/read", patch(mark_read))
            .layer(auth_layer)
            .with_state(state)
    }

    #[tokio::test]
    async fn create_project_and_send_message() {
        let app = test_app(100);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/projects")
                    .header("x-api-key", "test-admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"path":"/tmp/demo","name":"demo"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let project: ProjectResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(project.slug, "demo");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/projects/1/agents")
                    .header("x-api-key", "test-admin")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"AgentA","program":"test","model":"gpt"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let agent_a: AgentResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(agent_a.name, "AgentA");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/projects/1/agents")
                    .header("x-api-key", "test-admin")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"AgentB","program":"test","model":"gpt"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let agent_b: AgentResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(agent_b.name, "AgentB");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/messages")
                    .header("x-api-key", "test-admin")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"project_id":1,"sender_name":"AgentA","to":["AgentB"],"subject":"Hi","body":"Hello"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let message: MessageResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(message.subject, "Hi");
        assert_eq!(message.recipients, vec![agent_b.name]);

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/agents/2/inbox")
                    .header("x-api-key", "test-admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let inbox: Vec<MessageResponse> = serde_json::from_slice(&body).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].sender, agent_a.name);
    }

    #[tokio::test]
    async fn missing_api_key_is_rejected() {
        let app = test_app(100);
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/projects")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rate_limit_applies_to_agent_keys() {
        let app = test_app(1);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/projects")
                    .header("x-api-key", "test-admin")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"path":"/tmp/demo","name":"demo"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/projects/1/agents")
                    .header("x-api-key", "test-admin")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"AgentA","program":"test","model":"gpt"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/agents/1/keys")
                    .header("x-api-key", "test-admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let issued: ApiKeyResponse = serde_json::from_slice(&body).unwrap();

        let api_key = issued.api_key;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/agents/1/inbox")
                    .header("x-api-key", api_key.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/agents/1/inbox")
                    .header("x-api-key", api_key)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn poll_ws_events_emits_expected_kinds() {
        let storage = Storage::open_memory().unwrap();
        let project = storage.create_project("/tmp/demo", Some("demo")).unwrap();
        let agent_a = storage
            .register_agent(
                project.id,
                &CreateAgent {
                    name: Some("AgentA".to_string()),
                    program: "test".to_string(),
                    model: "gpt".to_string(),
                    task_description: None,
                },
            )
            .unwrap();
        let agent_b = storage
            .register_agent(
                project.id,
                &CreateAgent {
                    name: Some("AgentB".to_string()),
                    program: "test".to_string(),
                    model: "gpt".to_string(),
                    task_description: None,
                },
            )
            .unwrap();
        storage
            .send_message(
                project.id,
                &SendMessage {
                    sender_name: agent_b.name.clone(),
                    to: vec![agent_a.name.clone()],
                    cc: None,
                    bcc: None,
                    subject: "Hello".to_string(),
                    body: "World".to_string(),
                    importance: None,
                    ack_required: None,
                    thread_id: None,
                },
            )
            .unwrap();
        storage
            .create_file_reservations(
                project.id,
                &CreateFileReservation {
                    agent_name: agent_a.name.clone(),
                    paths: vec!["src/lib.rs".to_string()],
                    ttl_seconds: Some(60),
                    exclusive: Some(true),
                    reason: None,
                },
            )
            .unwrap();
        storage
            .create_file_reservations(
                project.id,
                &CreateFileReservation {
                    agent_name: agent_b.name.clone(),
                    paths: vec!["src/lib.rs".to_string()],
                    ttl_seconds: Some(600),
                    exclusive: Some(true),
                    reason: None,
                },
            )
            .unwrap();

        let state = AppState {
            config: Arc::new(AppConfig::default()),
            storage: Arc::new(Mutex::new(storage)),
            admin_key: "test-admin".to_string(),
            rate_limiter: Arc::new(RateLimiter::new(0)),
        };

        let mut last_seen = Utc::now() - chrono::Duration::minutes(10);
        let mut conflict_paths = std::collections::HashSet::new();
        let mut expiring_reservations = std::collections::HashSet::new();
        let events = poll_ws_events(
            &state,
            &agent_a,
            &mut last_seen,
            &mut conflict_paths,
            &mut expiring_reservations,
        )
        .await
        .unwrap();

        let mut kinds = Vec::new();
        for raw in events {
            let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
            kinds.push(value["kind"].as_str().unwrap().to_string());
        }
        assert!(kinds.contains(&"message".to_string()));
        assert!(kinds.contains(&"reservation_conflict".to_string()));
        assert!(kinds.contains(&"reservation_expiring".to_string()));
    }
}
