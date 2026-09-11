use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Todo,
    InProgress,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Task {
    pub id: Uuid,
    pub title: String,
    pub description: String,
    pub status: Status,
    pub priority: i64,
    pub requires: Vec<String>,
    pub depends_on: Vec<Uuid>,
    pub metadata: Value,
    pub claimed_by: Option<String>,
    pub result: Option<Value>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub version: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskView {
    #[serde(flatten)]
    pub task: Task,
    pub blocked_by: Vec<Uuid>,
    pub dependents: Vec<Uuid>,
    pub ready: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTask {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<Uuid>,
    #[serde(default = "empty_object")]
    pub metadata: Value,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchTask {
    #[serde(default)]
    pub title: Patch<String>,
    #[serde(default)]
    pub description: Patch<String>,
    #[serde(default)]
    pub priority: Patch<i64>,
    #[serde(default)]
    pub requires: Patch<Vec<String>>,
    #[serde(default)]
    pub depends_on: Patch<Vec<Uuid>>,
    #[serde(default)]
    pub metadata: Patch<Value>,
}

#[derive(Clone, Debug, Default)]
pub enum Patch<T> {
    #[default]
    Missing,
    Present(T),
}

impl<'de, T> Deserialize<'de> for Patch<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::Present)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRequest {
    pub agent: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultRequest {
    pub agent: String,
    pub result: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimNextRequest {
    pub agent: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyRequest {}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}
