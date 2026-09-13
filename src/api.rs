use std::sync::{Arc, Mutex, MutexGuard};

use axum::{
    Json as AxumJson, Router,
    extract::{
        FromRequest, FromRequestParts, OptionalFromRequest, Path, Query, Request, State,
        rejection::JsonRejection,
    },
    http::{Method, StatusCode, request::Parts},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use tracing::error;
use uuid::Uuid;

use crate::{
    model::{
        AgentRequest, ClaimNextRequest, CreateTask, EmptyRequest, Label, PatchTask, ResultRequest,
        SetLabelDescription, Status, TaskView,
    },
    store::{Store, StoreError},
};

const AGENT_DOCS: &str = include_str!("../docs/agent.md");

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<Store>>,
    expected_authorization: Option<String>,
}

impl AppState {
    pub fn new(store: Store, token: Option<String>) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            expected_authorization: token.map(|token| format!("Bearer {token}")),
        }
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

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(health))
        .route("/docs.md", get(agent_docs))
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
    if request.method() == Method::GET && matches!(request.uri().path(), "/healthz" | "/docs.md") {
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
