use std::{
    io,
    path::Path as StdPath,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use axum::{
    Json as AxumJson, Router,
    body::Body,
    extract::{
        FromRequest, FromRequestParts, OptionalFromRequest, Path, Query, Request, State,
        rejection::JsonRejection,
    },
    http::{HeaderValue, Method, StatusCode, header, request::Parts},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use jiff::Timestamp;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use tracing::{error, warn};
use uuid::Uuid;

use crate::{
    artifact::{ARTIFACT_DIRECTORY, ArtifactFiles, MAX_ARTIFACT_BYTES, UploadError},
    model::{
        AgentRequest, Artifact, ClaimNextRequest, CreateTask, EmptyRequest, Label, PatchTask,
        ResultRequest, SetLabelDescription, Status, TaskView,
    },
    store::{Store, StoreError},
};

const AGENT_DOCS: &str = include_str!("../docs/agent.md");
const BOARD_UI: &str = include_str!("ui/index.html");

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<Store>>,
    artifact_files: Arc<ArtifactFiles>,
    expected_authorization: Option<String>,
}

impl AppState {
    pub fn new(store: Store, token: Option<String>) -> Self {
        Self::with_artifact_dir(store, token, ARTIFACT_DIRECTORY)
            .expect("default artifact directory opens")
    }

    pub fn with_artifact_dir(
        store: Store,
        token: Option<String>,
        artifact_dir: impl AsRef<StdPath>,
    ) -> io::Result<Self> {
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            artifact_files: Arc::new(ArtifactFiles::open(artifact_dir.as_ref())?),
            expected_authorization: token.map(|token| format!("Bearer {token}")),
        })
    }

    pub fn collect_expired_artifacts(&self, cutoff: Timestamp) -> Result<(usize, usize), String> {
        let expired = lock(self)
            .take_expired_artifacts(cutoff)
            .map_err(|error| format!("failed to select expired artifacts: {error:?}"))?;
        for artifact in &expired {
            if let Err(error) = self.artifact_files.remove(artifact.id) {
                warn!(artifact = %artifact.id, %error, "failed to remove expired artifact file");
            }
        }
        let tracked = lock(self)
            .artifact_ids()
            .map_err(|error| format!("failed to list artifact records: {error:?}"))?;
        let orphans = self
            .artifact_files
            .remove_untracked(&tracked, Duration::from_secs(60 * 60))
            .map_err(|error| format!("failed to remove orphan artifact files: {error}"))?;
        Ok((expired.len(), orphans))
    }
}

pub struct Json<T>(pub T);

impl<S, T> FromRequest<S> for Json<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        <AxumJson<T> as FromRequest<S>>::from_request(request, state)
            .await
            .map(|AxumJson(value)| Self(value))
            .map_err(ApiError::from_json_rejection)
    }
}

impl<S, T> OptionalFromRequest<S> for Json<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Option<Self>, Self::Rejection> {
        Option::<AxumJson<T>>::from_request(request, state)
            .await
            .map(|value| value.map(|AxumJson(value)| Self(value)))
            .map_err(ApiError::from_json_rejection)
    }
}

struct TaskId(Uuid);

impl<S> FromRequestParts<S> for TaskId
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(value) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::not_found("task not found"))?;
        Uuid::parse_str(&value)
            .map(Self)
            .map_err(|_| ApiError::not_found("task not found"))
    }
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn payload_too_large() -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!("artifact must be at most {} bytes", MAX_ARTIFACT_BYTES),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        let message = message.into();
        error!(%message, "internal artifact error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "internal error".to_owned(),
        }
    }

    fn from_upload_error(error: UploadError) -> Self {
        match error {
            UploadError::TooLarge => Self::payload_too_large(),
            UploadError::Body(error) => {
                Self::bad_request(format!("failed to read artifact body: {error}"))
            }
            UploadError::Io(error) => Self::internal(format!("failed to store artifact: {error}")),
        }
    }

    fn from_artifact_store_error(error: StoreError) -> Self {
        match error {
            StoreError::NotFound => Self::not_found("artifact not found"),
            error => error.into(),
        }
    }

    fn from_json_rejection(rejection: JsonRejection) -> Self {
        let status = match rejection.status() {
            StatusCode::UNPROCESSABLE_ENTITY => StatusCode::BAD_REQUEST,
            status => status,
        };
        Self {
            status,
            message: rejection.body_text(),
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::NotFound => Self::not_found("task not found"),
            StoreError::Internal(message) => {
                error!(%message, "internal store error");
                Self {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: "internal error".to_owned(),
                }
            }
            StoreError::Invalid(message) => Self::bad_request(message),
            StoreError::Conflict(message) => Self {
                status: StatusCode::CONFLICT,
                message,
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, AxumJson(json!({ "error": self.message }))).into_response()
    }
}

#[derive(Serialize)]
struct Index {
    name: &'static str,
    version: &'static str,
    endpoints: Vec<Endpoint>,
}

#[derive(Serialize)]
struct Endpoint {
    method: &'static str,
    path: &'static str,
    description: &'static str,
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

#[derive(Serialize)]
struct TaskList {
    tasks: Vec<TaskView>,
}

#[derive(Serialize)]
struct LabelList {
    labels: Vec<Label>,
}

#[derive(Serialize)]
struct ArtifactList {
    artifacts: Vec<Artifact>,
}

struct UploadArtifact {
    filename: String,
    agent: String,
}

impl UploadArtifact {
    fn parse(parameters: Vec<(String, String)>) -> Result<Self, ApiError> {
        let mut filename = None;
        let mut agent = None;
        for (name, value) in parameters {
            let destination = match name.as_str() {
                "filename" => &mut filename,
                "agent" => &mut agent,
                _ => {
                    return Err(ApiError::bad_request(format!(
                        "unknown query field: {name}"
                    )));
                }
            };
            if destination.replace(value).is_some() {
                return Err(ApiError::bad_request(format!(
                    "duplicate query field: {name}"
                )));
            }
        }
        Ok(Self {
            filename: filename
                .ok_or_else(|| ApiError::bad_request("missing query field: filename"))?,
            agent: agent.ok_or_else(|| ApiError::bad_request("missing query field: agent"))?,
        })
    }
}

#[derive(Deserialize)]
struct ArtifactPath {
    id: String,
    artifact_id: String,
}

struct TaskArtifactIds {
    task_id: Uuid,
    artifact_id: Uuid,
}

impl<S> FromRequestParts<S> for TaskArtifactIds
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(path) = Path::<ArtifactPath>::from_request_parts(parts, state)
            .await
            .map_err(|_| ApiError::not_found("artifact not found"))?;
        let task_id =
            Uuid::parse_str(&path.id).map_err(|_| ApiError::not_found("artifact not found"))?;
        let artifact_id = Uuid::parse_str(&path.artifact_id)
            .map_err(|_| ApiError::not_found("artifact not found"))?;
        Ok(Self {
            task_id,
            artifact_id,
        })
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(health))
        .route("/docs.md", get(agent_docs))
        .route("/ui", get(board_ui))
        .route("/tasks", post(create_task).get(list_tasks))
        .route("/tasks/claim", post(claim_next))
        .route(
            "/tasks/{id}",
            get(get_task).patch(patch_task).delete(delete_task),
        )
        .route("/tasks/{id}/claim", post(claim_task))
        .route("/tasks/{id}/release", post(release_task))
        .route("/tasks/{id}/complete", post(complete_task))
        .route("/tasks/{id}/fail", post(fail_task))
        .route("/tasks/{id}/cancel", post(cancel_task))
        .route("/tasks/{id}/retry", post(retry_task))
        .route(
            "/tasks/{id}/artifacts",
            post(upload_artifact).get(list_artifacts),
        )
        .route(
            "/tasks/{id}/artifacts/{artifact_id}",
            get(download_artifact).delete(delete_artifact),
        )
        .route("/labels", get(list_labels))
        .route("/labels/{name}", put(set_label_description))
        .fallback(fallback)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

async fn authorize(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if request.method() == Method::GET
        && matches!(request.uri().path(), "/healthz" | "/docs.md" | "/ui")
    {
        return Ok(next.run(request).await);
    }
    let Some(expected) = &state.expected_authorization else {
        return Ok(next.run(request).await);
    };
    if request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        != Some(expected.as_str())
    {
        return Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "unauthorized".to_owned(),
        });
    }
    Ok(next.run(request).await)
}

async fn index() -> AxumJson<Index> {
    AxumJson(Index {
        name: "orcis",
        version: env!("CARGO_PKG_VERSION"),
        endpoints: vec![
            endpoint("GET", "/", "Discover the API endpoints."),
            endpoint("GET", "/healthz", "Check service health."),
            endpoint(
                "GET",
                "/docs.md",
                "Read the agent documentation as Markdown.",
            ),
            endpoint("GET", "/ui", "View the board read-only in a browser."),
            endpoint("POST", "/tasks", "Create a task."),
            endpoint("GET", "/tasks", "List and filter tasks."),
            endpoint("GET", "/tasks/{id}", "Get a task."),
            endpoint("PATCH", "/tasks/{id}", "Update a task."),
            endpoint("DELETE", "/tasks/{id}", "Delete a task."),
            endpoint(
                "POST",
                "/tasks/claim",
                "Claim the best matching ready task.",
            ),
            endpoint("POST", "/tasks/{id}/claim", "Claim a ready task."),
            endpoint("POST", "/tasks/{id}/release", "Release a claimed task."),
            endpoint("POST", "/tasks/{id}/complete", "Complete a claimed task."),
            endpoint("POST", "/tasks/{id}/fail", "Fail a claimed task."),
            endpoint("POST", "/tasks/{id}/cancel", "Cancel a task."),
            endpoint(
                "POST",
                "/tasks/{id}/retry",
                "Retry a failed or cancelled task.",
            ),
            endpoint(
                "POST",
                "/tasks/{id}/artifacts",
                "Upload an artifact for a claimed task.",
            ),
            endpoint("GET", "/tasks/{id}/artifacts", "List a task's artifacts."),
            endpoint(
                "GET",
                "/tasks/{id}/artifacts/{artifact_id}",
                "Download an artifact.",
            ),
            endpoint(
                "DELETE",
                "/tasks/{id}/artifacts/{artifact_id}",
                "Delete an artifact.",
            ),
            endpoint(
                "GET",
                "/labels",
                "List labels in use on open tasks, with descriptions and counts.",
            ),
            endpoint(
                "PUT",
                "/labels/{name}",
                "Set a label's description on every open task that carries it.",
            ),
        ],
    })
}

fn endpoint(method: &'static str, path: &'static str, description: &'static str) -> Endpoint {
    Endpoint {
        method,
        path,
        description,
    }
}

async fn health() -> AxumJson<Health> {
    AxumJson(Health { status: "ok" })
}

async fn agent_docs() -> impl IntoResponse {
    (
        [("content-type", "text/markdown; charset=utf-8")],
        AGENT_DOCS,
    )
}

async fn board_ui() -> impl IntoResponse {
    (
        [
            ("content-type", "text/html; charset=utf-8"),
            ("cache-control", "no-cache"),
        ],
        BOARD_UI,
    )
}

async fn create_task(
    State(state): State<AppState>,
    Json(request): Json<CreateTask>,
) -> Result<impl IntoResponse, ApiError> {
    let task = lock(&state).create(request)?;
    Ok((StatusCode::CREATED, AxumJson(task)))
}

async fn list_tasks(
    State(state): State<AppState>,
    Query(parameters): Query<Vec<(String, String)>>,
) -> Result<AxumJson<TaskList>, ApiError> {
    let filters = Filters::parse(parameters)?;
    let tasks = lock(&state)
        .list()?
        .into_iter()
        .filter(|task| filters.matches(task))
        .collect();
    Ok(AxumJson(TaskList { tasks }))
}

async fn get_task(
    State(state): State<AppState>,
    TaskId(id): TaskId,
) -> Result<AxumJson<TaskView>, ApiError> {
    Ok(AxumJson(lock(&state).get(id)?))
}

async fn patch_task(
    State(state): State<AppState>,
    TaskId(id): TaskId,
    Json(request): Json<PatchTask>,
) -> Result<AxumJson<TaskView>, ApiError> {
    Ok(AxumJson(lock(&state).patch(id, request)?))
}

async fn delete_task(
    State(state): State<AppState>,
    TaskId(id): TaskId,
) -> Result<StatusCode, ApiError> {
    lock(&state).delete(id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn claim_task(
    State(state): State<AppState>,
    TaskId(id): TaskId,
    Json(request): Json<AgentRequest>,
) -> Result<AxumJson<TaskView>, ApiError> {
    Ok(AxumJson(lock(&state).claim(id, request.agent)?))
}

async fn claim_next(
    State(state): State<AppState>,
    Json(request): Json<ClaimNextRequest>,
) -> Result<Response, ApiError> {
    match lock(&state).claim_next(request.agent, &request.capabilities)? {
        Some(task) => Ok(AxumJson(task).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

async fn release_task(
    State(state): State<AppState>,
    TaskId(id): TaskId,
    Json(request): Json<AgentRequest>,
) -> Result<AxumJson<TaskView>, ApiError> {
    Ok(AxumJson(lock(&state).release(id, &request.agent)?))
}

async fn complete_task(
    State(state): State<AppState>,
    TaskId(id): TaskId,
    Json(request): Json<ResultRequest>,
) -> Result<AxumJson<TaskView>, ApiError> {
    Ok(AxumJson(lock(&state).complete(
        id,
        &request.agent,
        request.result,
    )?))
}

async fn fail_task(
    State(state): State<AppState>,
    TaskId(id): TaskId,
    Json(request): Json<ResultRequest>,
) -> Result<AxumJson<TaskView>, ApiError> {
    Ok(AxumJson(lock(&state).fail(
        id,
        &request.agent,
        request.result,
    )?))
}

async fn cancel_task(
    State(state): State<AppState>,
    TaskId(id): TaskId,
    _body: Option<Json<EmptyRequest>>,
) -> Result<AxumJson<TaskView>, ApiError> {
    Ok(AxumJson(lock(&state).cancel(id)?))
}

async fn retry_task(
    State(state): State<AppState>,
    TaskId(id): TaskId,
    _body: Option<Json<EmptyRequest>>,
) -> Result<AxumJson<TaskView>, ApiError> {
    Ok(AxumJson(lock(&state).retry(id)?))
}

async fn upload_artifact(
    State(state): State<AppState>,
    TaskId(task_id): TaskId,
    Query(parameters): Query<Vec<(String, String)>>,
    request: Request,
) -> Result<impl IntoResponse, ApiError> {
    let upload = UploadArtifact::parse(parameters)?;
    if let Some(value) = request.headers().get(header::CONTENT_LENGTH) {
        let size = value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| ApiError::bad_request("invalid content length"))?;
        if size > MAX_ARTIFACT_BYTES as u64 {
            return Err(ApiError::payload_too_large());
        }
    }
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| ApiError::bad_request("invalid artifact content type"))
        })
        .transpose()?
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    lock(&state).validate_artifact_upload(
        task_id,
        &upload.agent,
        &upload.filename,
        &content_type,
    )?;

    let id = Uuid::new_v4();
    let size_bytes = state
        .artifact_files
        .write_body(id, request.into_body())
        .await
        .map_err(ApiError::from_upload_error)?;
    let artifact = lock(&state).add_artifact(
        &upload.agent,
        Artifact {
            id,
            task_id,
            filename: upload.filename,
            content_type,
            size_bytes,
            created_at: Timestamp::now(),
        },
    );
    match artifact {
        Ok(artifact) => Ok((StatusCode::CREATED, AxumJson(artifact))),
        Err(error) => {
            if let Err(remove_error) = state.artifact_files.remove_async(id).await {
                warn!(artifact = %id, error = %remove_error, "failed to remove rejected artifact upload");
            }
            Err(error.into())
        }
    }
}

async fn list_artifacts(
    State(state): State<AppState>,
    TaskId(task_id): TaskId,
) -> Result<AxumJson<ArtifactList>, ApiError> {
    Ok(AxumJson(ArtifactList {
        artifacts: lock(&state).artifacts(task_id)?,
    }))
}

async fn download_artifact(
    State(state): State<AppState>,
    ids: TaskArtifactIds,
) -> Result<Response, ApiError> {
    let artifact = lock(&state)
        .artifact(ids.task_id, ids.artifact_id)
        .map_err(ApiError::from_artifact_store_error)?;
    let body = state
        .artifact_files
        .body(artifact.id, artifact.size_bytes)
        .await
        .map_err(|error| ApiError::internal(format!("failed to read artifact file: {error}")))?;
    let mut response = Body::new(body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&artifact.content_type)
            .map_err(|_| ApiError::internal("invalid stored artifact content type"))?,
    );
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&artifact.size_bytes.to_string())
            .expect("artifact size is a valid header value"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&content_disposition(&artifact.filename))
            .expect("sanitized artifact filename is a valid header value"),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

async fn delete_artifact(
    State(state): State<AppState>,
    ids: TaskArtifactIds,
) -> Result<StatusCode, ApiError> {
    let artifact = lock(&state)
        .delete_artifact(ids.task_id, ids.artifact_id)
        .map_err(ApiError::from_artifact_store_error)?;
    if let Err(error) = state.artifact_files.remove_async(artifact.id).await {
        warn!(artifact = %artifact.id, %error, "failed to remove deleted artifact file");
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn list_labels(State(state): State<AppState>) -> Result<AxumJson<LabelList>, ApiError> {
    Ok(AxumJson(LabelList {
        labels: lock(&state).labels()?,
    }))
}

async fn set_label_description(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(request): Json<SetLabelDescription>,
) -> Result<AxumJson<Label>, ApiError> {
    let name = name.trim();
    match lock(&state).set_label_description(name, request.description) {
        Ok(label) => Ok(AxumJson(label)),
        Err(StoreError::NotFound) => Err(ApiError::not_found("label not found")),
        Err(error) => Err(error.into()),
    }
}

async fn fallback() -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        message: "not found".to_owned(),
    }
}

async fn method_not_allowed() -> ApiError {
    ApiError {
        status: StatusCode::METHOD_NOT_ALLOWED,
        message: "method not allowed".to_owned(),
    }
}

fn lock(state: &AppState) -> MutexGuard<'_, Store> {
    state
        .store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn content_disposition(filename: &str) -> String {
    let filename: String = filename
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, ' ' | '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let filename = if filename.trim().is_empty() {
        "artifact"
    } else {
        &filename
    };
    format!("attachment; filename=\"{filename}\"")
}

#[derive(Default)]
struct Filters {
    statuses: Vec<Status>,
    ready: Option<bool>,
    requires: Vec<String>,
    claimed_by: Option<String>,
}

impl Filters {
    fn parse(parameters: Vec<(String, String)>) -> Result<Self, ApiError> {
        let mut filters = Self::default();
        for (name, value) in parameters {
            match name.as_str() {
                "status" => filters.statuses.push(parse_status(&value)?),
                "ready" => {
                    filters.ready = Some(
                        value
                            .parse()
                            .map_err(|_| ApiError::bad_request("ready must be true or false"))?,
                    )
                }
                "requires" => filters.requires.push(value),
                "claimed_by" => filters.claimed_by = Some(value),
                _ => {
                    return Err(ApiError::bad_request(format!(
                        "unknown query field: {name}"
                    )));
                }
            }
        }
        Ok(filters)
    }

    fn matches(&self, task: &TaskView) -> bool {
        (self.statuses.is_empty() || self.statuses.contains(&task.task.status))
            && self.ready.is_none_or(|ready| task.ready == ready)
            && self.requires.iter().all(|requirement| {
                task.task
                    .requires
                    .iter()
                    .any(|task_requirement| task_requirement.name == *requirement)
            })
            && self
                .claimed_by
                .as_ref()
                .is_none_or(|agent| task.task.claimed_by.as_ref() == Some(agent))
    }
}

fn parse_status(value: &str) -> Result<Status, ApiError> {
    match value {
        "todo" => Ok(Status::Todo),
        "in_progress" => Ok(Status::InProgress),
        "done" => Ok(Status::Done),
        "failed" => Ok(Status::Failed),
        "cancelled" => Ok(Status::Cancelled),
        _ => Err(ApiError::bad_request(format!(
            "invalid status filter: {value}"
        ))),
    }
}
