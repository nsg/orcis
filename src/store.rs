use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
use rusqlite::{
    Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params, types::Type,
};
use serde_json::Value;
use uuid::Uuid;

use crate::model::{CreateTask, Patch, PatchTask, Status, Task, TaskView};

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE tasks (
  id          TEXT PRIMARY KEY,
  title       TEXT NOT NULL,
  description TEXT NOT NULL,
  status      TEXT NOT NULL,
  priority    INTEGER NOT NULL,
  requires    TEXT NOT NULL,
  metadata    TEXT NOT NULL,
  claimed_by  TEXT,
  result      TEXT,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  version     INTEGER NOT NULL
);
CREATE TABLE task_dependencies (
  task_id    TEXT NOT NULL REFERENCES tasks(id) ON DELETE RESTRICT,
  depends_on TEXT NOT NULL REFERENCES tasks(id) ON DELETE RESTRICT,
  position   INTEGER NOT NULL,
  PRIMARY KEY (task_id, depends_on)
);
CREATE INDEX task_dependencies_depends_on ON task_dependencies(depends_on);
CREATE INDEX tasks_status_priority ON tasks(status, priority DESC, created_at, id);
"#];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    NotFound,
    Invalid(String),
    Conflict(String),
    Internal(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

#[derive(Clone, Copy)]
enum OwnedAction {
    Release,
    Complete,
    Fail,
}

impl Store {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.execute_batch(
            "PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    pub fn in_memory() -> Self {
        Self::open(":memory:").expect("in-memory SQLite database opens")
    }

    pub fn create(&mut self, request: CreateTask) -> Result<TaskView, StoreError> {
        self.create_at(Uuid::new_v4(), request, Timestamp::now())
    }

    fn create_at(
        &mut self,
        id: Uuid,
        request: CreateTask,
        now: Timestamp,
    ) -> Result<TaskView, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_title(&request.title)?;
        let requires = deduplicate(request.requires);
        let depends_on = deduplicate(request.depends_on);
        validate_dependencies(&tx, id, &depends_on)?;
        validate_cycle(&tx, id, &depends_on)?;

        let task = Task {
            id,
            title: request.title,
            description: request.description,
            status: Status::Todo,
            priority: request.priority,
            requires,
            depends_on,
            metadata: request.metadata,
            claimed_by: None,
            result: None,
            created_at: now,
            updated_at: now,
            version: 1,
        };
        insert_task(&tx, &task)?;
        replace_dependencies(&tx, task.id, &task.depends_on)?;
        let view = view(&tx, task)?;
        tx.commit()?;
        Ok(view)
    }

    pub fn get(&self, id: Uuid) -> Result<TaskView, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let task = read_task(&tx, id)?;
        let view = view(&tx, task)?;
        tx.commit()?;
        Ok(view)
    }

    pub fn list(&self) -> Result<Vec<TaskView>, StoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let views = list_views(&tx)?;
        tx.commit()?;
        Ok(views)
    }

    pub fn patch(&mut self, id: Uuid, patch: PatchTask) -> Result<TaskView, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut task = read_task(&tx, id)?;
        let PatchTask {
            title,
            description,
            priority,
            requires,
            depends_on,
            metadata,
        } = patch;

        if let Patch::Present(title) = &title {
            validate_title(title)?;
        }
        if let Patch::Present(dependencies) = depends_on {
            let dependencies = deduplicate(dependencies);
            tx.execute(
                "DELETE FROM task_dependencies WHERE task_id = ?1",
                [id.to_string()],
            )?;
            validate_dependencies(&tx, id, &dependencies)?;
            validate_cycle(&tx, id, &dependencies)?;
            replace_dependencies(&tx, id, &dependencies)?;
            task.depends_on = dependencies;
        }
        if let Patch::Present(title) = title {
            task.title = title;
        }
        if let Patch::Present(description) = description {
            task.description = description;
        }
        if let Patch::Present(priority) = priority {
            task.priority = priority;
        }
        if let Patch::Present(requires) = requires {
            task.requires = deduplicate(requires);
        }
        if let Patch::Present(metadata) = metadata {
            task.metadata = metadata;
        }
        touch(&mut task);
        update_task(&tx, &task)?;
        let view = view(&tx, task)?;
        tx.commit()?;
        Ok(view)
    }

    pub fn delete(&mut self, id: Uuid) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        read_task(&tx, id)?;
        let dependents = dependent_ids(&tx, id)?;
        if !dependents.is_empty() {
            return Err(StoreError::Conflict(format!(
                "task has dependents: {}",
                join_ids(&dependents)
            )));
        }
        tx.execute(
            "DELETE FROM task_dependencies WHERE task_id = ?1",
            [id.to_string()],
        )?;
        tx.execute("DELETE FROM tasks WHERE id = ?1", [id.to_string()])?;
        tx.commit()?;
        Ok(())
    }

    pub fn claim(&mut self, id: Uuid, agent: String) -> Result<TaskView, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let view = claim_in_transaction(&tx, id, agent)?;
        tx.commit()?;
        Ok(view)
    }

    pub fn claim_next(
        &mut self,
        agent: String,
        capabilities: &[String],
    ) -> Result<Option<TaskView>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let capabilities: HashSet<&str> = capabilities.iter().map(String::as_str).collect();
        let candidate = {
            let mut statement = tx.prepare(
                "SELECT id, requires
                 FROM tasks
                 WHERE status = 'todo'
                   AND NOT EXISTS (
                     SELECT 1
                     FROM task_dependencies td
                     JOIN tasks d ON d.id = td.depends_on
                     WHERE td.task_id = tasks.id AND d.status <> 'done'
                   )
                 ORDER BY priority DESC, created_at ASC, id ASC",
            )?;
            let mut rows = statement.query([])?;
            let mut candidate = None;
            while let Some(row) = rows.next()? {
                let id_text: String = row.get(0)?;
                let requires_text: String = row.get(1)?;
                let requires: Vec<String> = decode_json(1, &requires_text)?;
                if requires
                    .iter()
                    .all(|requirement| capabilities.contains(requirement.as_str()))
                {
                    candidate = Some(parse_uuid(0, &id_text)?);
                    break;
                }
            }
            candidate
        };

        let Some(id) = candidate else {
            tx.commit()?;
            return Ok(None);
        };
        let view = claim_in_transaction(&tx, id, agent)?;
        tx.commit()?;
        Ok(Some(view))
    }

    pub fn release(&mut self, id: Uuid, agent: &str) -> Result<TaskView, StoreError> {
        self.transition_owned(id, agent, OwnedAction::Release, None)
    }

    pub fn complete(
        &mut self,
        id: Uuid,
        agent: &str,
        result: Option<Value>,
    ) -> Result<TaskView, StoreError> {
        self.transition_owned(id, agent, OwnedAction::Complete, result)
    }

    pub fn fail(
        &mut self,
        id: Uuid,
        agent: &str,
        result: Option<Value>,
    ) -> Result<TaskView, StoreError> {
        self.transition_owned(id, agent, OwnedAction::Fail, result)
    }

    pub fn cancel(&mut self, id: Uuid) -> Result<TaskView, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut task = read_task(&tx, id)?;
        if !matches!(
            task.status,
            Status::Todo | Status::InProgress | Status::Failed
        ) {
            return Err(action_conflict("cancel", task.status));
        }
        task.status = Status::Cancelled;
        task.claimed_by = None;
        touch(&mut task);
        update_task(&tx, &task)?;
        let view = view(&tx, task)?;
        tx.commit()?;
        Ok(view)
    }

    pub fn retry(&mut self, id: Uuid) -> Result<TaskView, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut task = read_task(&tx, id)?;
        if !matches!(task.status, Status::Failed | Status::Cancelled) {
            return Err(action_conflict("retry", task.status));
        }
        task.status = Status::Todo;
        task.claimed_by = None;
        task.result = None;
        touch(&mut task);
        update_task(&tx, &task)?;
        let view = view(&tx, task)?;
        tx.commit()?;
        Ok(view)
    }

    fn transition_owned(
        &mut self,
        id: Uuid,
        agent: &str,
        action: OwnedAction,
        result: Option<Value>,
    ) -> Result<TaskView, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut task = read_task(&tx, id)?;
        if task.status != Status::InProgress {
            return Err(action_conflict(action.label(), task.status));
        }
        if task.claimed_by.as_deref() != Some(agent) {
            return Err(StoreError::Conflict(format!(
                "task is not claimed by agent {agent}"
            )));
        }
        task.status = action.status();
        if matches!(action, OwnedAction::Release) {
            task.claimed_by = None;
        } else {
            task.result = result;
        }
        touch(&mut task);
        update_task(&tx, &task)?;
        let view = view(&tx, task)?;
        tx.commit()?;
        Ok(view)
    }
}

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for (index, migration) in MIGRATIONS.iter().enumerate() {
        let version = i64::try_from(index + 1).expect("migration count fits in SQLite integer");
        if version <= current {
            continue;
        }
        tx.execute_batch(migration)?;
        tx.pragma_update(None, "user_version", version)?;
    }
    tx.commit()?;
    Ok(())
}

fn insert_task(conn: &Connection, task: &Task) -> Result<(), StoreError> {
    let requires = encode_json(&task.requires)?;
    let metadata = encode_json(&task.metadata)?;
    let result = task.result.as_ref().map(encode_json).transpose()?;
    let version = sql_integer(task.version, "task version")?;
    conn.execute(
        "INSERT INTO tasks (
           id, title, description, status, priority, requires, metadata, claimed_by, result,
           created_at, updated_at, version
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            task.id.to_string(),
            task.title,
            task.description,
            status_text(task.status),
            task.priority,
            requires,
            metadata,
            task.claimed_by,
            result,
            format!("{:.9}", task.created_at),
            format!("{:.9}", task.updated_at),
            version,
        ],
    )?;
    Ok(())
}

fn update_task(conn: &Connection, task: &Task) -> Result<(), StoreError> {
    let requires = encode_json(&task.requires)?;
    let metadata = encode_json(&task.metadata)?;
    let result = task.result.as_ref().map(encode_json).transpose()?;
    conn.execute(
        "UPDATE tasks
         SET title = ?2, description = ?3, status = ?4, priority = ?5, requires = ?6,
             metadata = ?7, claimed_by = ?8, result = ?9, updated_at = ?10,
             version = version + 1
         WHERE id = ?1",
        params![
            task.id.to_string(),
            task.title,
            task.description,
            status_text(task.status),
            task.priority,
            requires,
            metadata,
            task.claimed_by,
            result,
            format!("{:.9}", task.updated_at),
        ],
    )?;
    Ok(())
}

fn replace_dependencies(
    conn: &Connection,
    task_id: Uuid,
    dependencies: &[Uuid],
) -> Result<(), StoreError> {
    let mut statement = conn.prepare(
        "INSERT INTO task_dependencies (task_id, depends_on, position) VALUES (?1, ?2, ?3)",
    )?;
    for (position, dependency) in dependencies.iter().enumerate() {
        let position = sql_integer(position, "dependency position")?;
        statement.execute(params![
            task_id.to_string(),
            dependency.to_string(),
            position,
        ])?;
    }
    Ok(())
}

fn read_task(conn: &Connection, id: Uuid) -> Result<Task, StoreError> {
    let mut task = conn
        .query_row(
            "SELECT id, title, description, status, priority, requires, metadata, claimed_by,
                    result, created_at, updated_at, version
             FROM tasks WHERE id = ?1",
            [id.to_string()],
            decode_task,
        )
        .optional()?
        .ok_or(StoreError::NotFound)?;
    task.depends_on = dependency_ids(conn, id)?;
    Ok(task)
}

fn decode_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    let id_text: String = row.get(0)?;
    let status: String = row.get(3)?;
    let requires: String = row.get(5)?;
    let metadata: String = row.get(6)?;
    let result: Option<String> = row.get(8)?;
    let created_at: String = row.get(9)?;
    let updated_at: String = row.get(10)?;
    let version: i64 = row.get(11)?;
    Ok(Task {
        id: parse_uuid(0, &id_text)?,
        title: row.get(1)?,
        description: row.get(2)?,
        status: parse_status(3, &status)?,
        priority: row.get(4)?,
        requires: decode_json(5, &requires)?,
        depends_on: Vec::new(),
        metadata: decode_json(6, &metadata)?,
        claimed_by: row.get(7)?,
        result: result
            .as_deref()
            .map(|value| decode_json(8, value))
            .transpose()?,
        created_at: parse_timestamp(9, &created_at)?,
        updated_at: parse_timestamp(10, &updated_at)?,
        version: version
            .try_into()
            .map_err(|error| conversion_error(11, Type::Integer, error))?,
    })
}

fn list_views(conn: &Connection) -> rusqlite::Result<Vec<TaskView>> {
    let mut statement = conn.prepare(
        "SELECT id, title, description, status, priority, requires, metadata, claimed_by,
                result, created_at, updated_at, version
         FROM tasks
         ORDER BY priority DESC, created_at ASC, id ASC",
    )?;
    let tasks = statement
        .query_map([], decode_task)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);

    let mut views: Vec<_> = tasks
        .into_iter()
        .map(|task| TaskView {
            task,
            blocked_by: Vec::new(),
            dependents: Vec::new(),
            ready: false,
        })
        .collect();
    let positions: HashMap<_, _> = views
        .iter()
        .enumerate()
        .map(|(index, view)| (view.task.id, index))
        .collect();

    let mut statement = conn.prepare(
        "SELECT td.task_id, td.depends_on, d.status
         FROM task_dependencies td
         JOIN tasks d ON d.id = td.depends_on
         ORDER BY td.task_id, td.position",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let task_id = parse_uuid(0, &row.get::<_, String>(0)?)?;
        let depends_on = parse_uuid(1, &row.get::<_, String>(1)?)?;
        let dependency_status: String = row.get(2)?;
        if let Some(&index) = positions.get(&task_id) {
            views[index].task.depends_on.push(depends_on);
            if dependency_status != "done" {
                views[index].blocked_by.push(depends_on);
            }
        }
        if let Some(&index) = positions.get(&depends_on) {
            views[index].dependents.push(task_id);
        }
    }
    for view in &mut views {
        view.ready = view.task.status == Status::Todo && view.blocked_by.is_empty();
    }
    Ok(views)
}

fn view(conn: &Connection, task: Task) -> Result<TaskView, StoreError> {
    let blocked_by = blocked_ids(conn, task.id)?;
    let dependents = dependent_ids(conn, task.id)?;
    let ready = task.status == Status::Todo && blocked_by.is_empty();
    Ok(TaskView {
        task,
        blocked_by,
        dependents,
        ready,
    })
}

fn dependency_ids(conn: &Connection, id: Uuid) -> rusqlite::Result<Vec<Uuid>> {
    query_ids(
        conn,
        "SELECT depends_on FROM task_dependencies WHERE task_id = ?1 ORDER BY position",
        id,
    )
}

fn blocked_ids(conn: &Connection, id: Uuid) -> rusqlite::Result<Vec<Uuid>> {
    query_ids(
        conn,
        "SELECT td.depends_on
         FROM task_dependencies td
         JOIN tasks d ON d.id = td.depends_on
         WHERE td.task_id = ?1 AND d.status <> 'done'
         ORDER BY td.position",
        id,
    )
}

fn dependent_ids(conn: &Connection, id: Uuid) -> rusqlite::Result<Vec<Uuid>> {
    query_ids(
        conn,
        "SELECT task_id FROM task_dependencies WHERE depends_on = ?1 ORDER BY task_id",
        id,
    )
}

fn query_ids(conn: &Connection, sql: &str, id: Uuid) -> rusqlite::Result<Vec<Uuid>> {
    let mut statement = conn.prepare(sql)?;
    statement
        .query_map([id.to_string()], |row| {
            parse_uuid(0, &row.get::<_, String>(0)?)
        })?
        .collect()
}

fn validate_dependencies(
    conn: &Connection,
    id: Uuid,
    dependencies: &[Uuid],
) -> Result<(), StoreError> {
    if dependencies.contains(&id) {
        return Err(StoreError::Invalid(format!(
            "task cannot depend on itself: {id}"
        )));
    }
    for dependency in dependencies {
        let exists = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id = ?1)",
            [dependency.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            return Err(StoreError::Invalid(format!(
                "dependency does not exist: {dependency}"
            )));
        }
    }
    Ok(())
}

fn validate_cycle(conn: &Connection, id: Uuid, dependencies: &[Uuid]) -> Result<(), StoreError> {
    for dependency in dependencies {
        let cycle = conn
            .query_row(
                "WITH RECURSIVE reach(id) AS (
                   SELECT ?1
                   UNION
                   SELECT td.depends_on
                   FROM task_dependencies td
                   JOIN reach ON td.task_id = reach.id
                 )
                 SELECT 1 FROM reach WHERE id = ?2 LIMIT 1",
                params![dependency.to_string(), id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some();
        if cycle {
            return Err(StoreError::Conflict(format!(
                "dependency cycle involving: {id}, {dependency}"
            )));
        }
    }
    Ok(())
}

fn claim_in_transaction(
    tx: &Transaction<'_>,
    id: Uuid,
    agent: String,
) -> Result<TaskView, StoreError> {
    let mut task = read_task(tx, id)?;
    if task.status != Status::Todo {
        return Err(action_conflict("claim", task.status));
    }
    let blocked_by = blocked_ids(tx, id)?;
    if !blocked_by.is_empty() {
        return Err(StoreError::Conflict(format!(
            "task is blocked by: {}",
            join_ids(&blocked_by)
        )));
    }
    task.status = Status::InProgress;
    task.claimed_by = Some(agent);
    touch(&mut task);
    update_task(tx, &task)?;
    view(tx, task)
}

impl OwnedAction {
    fn label(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Complete => "complete",
            Self::Fail => "fail",
        }
    }

    fn status(self) -> Status {
        match self {
            Self::Release => Status::Todo,
            Self::Complete => Status::Done,
            Self::Fail => Status::Failed,
        }
    }
}

fn encode_json(value: &impl serde::Serialize) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|error| StoreError::Internal(error.to_string()))
}

fn sql_integer(value: impl TryInto<i64>, description: &str) -> Result<i64, StoreError> {
    value
        .try_into()
        .map_err(|_| StoreError::Internal(format!("{description} exceeds SQLite integer range")))
}

fn decode_json<T: serde::de::DeserializeOwned>(index: usize, value: &str) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|error| conversion_error(index, Type::Text, error))
}

fn parse_uuid(index: usize, value: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(value).map_err(|error| conversion_error(index, Type::Text, error))
}

fn parse_timestamp(index: usize, value: &str) -> rusqlite::Result<Timestamp> {
    value
        .parse()
        .map_err(|error| conversion_error(index, Type::Text, error))
}

fn parse_status(index: usize, value: &str) -> rusqlite::Result<Status> {
    match value {
        "todo" => Ok(Status::Todo),
        "in_progress" => Ok(Status::InProgress),
        "done" => Ok(Status::Done),
        "failed" => Ok(Status::Failed),
        "cancelled" => Ok(Status::Cancelled),
        _ => Err(conversion_error(
            index,
            Type::Text,
            format!("invalid task status: {value}"),
        )),
    }
}

fn conversion_error(
    index: usize,
    value_type: Type,
    error: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, value_type, error.into())
}

fn status_text(status: Status) -> &'static str {
    match status {
        Status::Todo => "todo",
        Status::InProgress => "in_progress",
        Status::Done => "done",
        Status::Failed => "failed",
        Status::Cancelled => "cancelled",
    }
}

fn validate_title(title: &str) -> Result<(), StoreError> {
    if title.trim().is_empty() {
        return Err(StoreError::Invalid("title must not be empty".to_owned()));
    }
    if title.chars().count() > 500 {
        return Err(StoreError::Invalid(
            "title must be at most 500 characters".to_owned(),
        ));
    }
    Ok(())
}

fn deduplicate<T: Eq + std::hash::Hash + Clone>(values: Vec<T>) -> Vec<T> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn touch(task: &mut Task) {
    task.updated_at = Timestamp::now();
    task.version += 1;
}

fn action_conflict(action: &str, status: Status) -> StoreError {
    StoreError::Conflict(format!(
        "cannot {action} a task in status {}",
        status_text(status)
    ))
}

fn join_ids(ids: &[Uuid]) -> String {
    ids.iter()
        .map(Uuid::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::model::CreateTask;
    use serde_json::json;
    use tempfile::tempdir;

    fn request(title: &str) -> CreateTask {
        CreateTask {
            title: title.to_owned(),
            description: String::new(),
            priority: 0,
            requires: Vec::new(),
            depends_on: Vec::new(),
            metadata: json!({}),
        }
    }

    fn create(store: &mut Store, title: &str) -> Uuid {
        store.create(request(title)).unwrap().task.id
    }

    #[test]
    fn create_get_and_dependency_validation() {
        let mut store = Store::in_memory();
        let id = create(&mut store, "task");
        assert_eq!(store.get(id).unwrap().task.title, "task");

        let mut missing = request("missing");
        let missing_id = Uuid::new_v4();
        missing.depends_on.push(missing_id);
        assert!(matches!(
            store.create(missing),
            Err(StoreError::Invalid(message)) if message == format!("dependency does not exist: {missing_id}")
        ));

        let self_id = Uuid::new_v4();
        let mut self_dependency = request("self");
        self_dependency.depends_on.push(self_id);
        assert!(matches!(
            store.create_at(self_id, self_dependency, Timestamp::now()),
            Err(StoreError::Invalid(message)) if message.contains(&self_id.to_string())
        ));
    }

    #[test]
    fn detects_short_and_long_cycles() {
        let mut store = Store::in_memory();
        let b = create(&mut store, "B");
        let mut a_request = request("A");
        a_request.depends_on.push(b);
        let a = store.create(a_request).unwrap().task.id;
        let patch = PatchTask {
            depends_on: Patch::Present(vec![a]),
            ..PatchTask::default()
        };
        assert!(matches!(
            store.patch(b, patch),
            Err(StoreError::Conflict(message)) if message.contains("dependency cycle")
        ));

        let mut store = Store::in_memory();
        let c = create(&mut store, "C");
        let mut b_request = request("B");
        b_request.depends_on.push(c);
        let b = store.create(b_request).unwrap().task.id;
        let mut a_request = request("A");
        a_request.depends_on.push(b);
        let a = store.create(a_request).unwrap().task.id;
        let patch = PatchTask {
            depends_on: Patch::Present(vec![a]),
            ..PatchTask::default()
        };
        assert!(matches!(
            store.patch(c, patch),
            Err(StoreError::Conflict(message)) if message.contains("dependency cycle")
        ));
    }

    #[test]
    fn dependency_completion_changes_readiness() {
        let mut store = Store::in_memory();
        let a = create(&mut store, "A");
        let mut b_request = request("B");
        b_request.depends_on.push(a);
        let b = store.create(b_request).unwrap().task.id;
        assert!(!store.get(b).unwrap().ready);
        assert_eq!(store.get(b).unwrap().blocked_by, vec![a]);

        store.claim(a, "agent".to_owned()).unwrap();
        store
            .complete(a, "agent", Some(json!({ "ok": true })))
            .unwrap();
        assert!(store.get(b).unwrap().ready);
    }

    #[test]
    fn claim_selection_obeys_order_capabilities_and_readiness() {
        let mut store = Store::in_memory();
        let blocker = create(&mut store, "blocker");

        let mut blocked_request = request("blocked high");
        blocked_request.priority = 100;
        blocked_request.depends_on = vec![blocker];
        store.create(blocked_request).unwrap();

        let mut frontend = request("frontend");
        frontend.priority = 50;
        frontend.requires = vec!["frontend".to_owned()];
        store.create(frontend).unwrap();

        let mut lower = request("lower");
        lower.priority = 2;
        let lower_id = store.create(lower).unwrap().task.id;

        let common_time = Timestamp::constant(1_700_000_000, 0);
        let mut oldest = request("oldest");
        oldest.priority = 3;
        let oldest_id = Uuid::new_v4();
        store.create_at(oldest_id, oldest, common_time).unwrap();
        let mut newest = request("newest");
        newest.priority = 3;
        let newest_id = Uuid::new_v4();
        store.create_at(newest_id, newest, common_time).unwrap();
        let expected_tie_winner = oldest_id.min(newest_id);

        let claimed = store.claim_next("none".to_owned(), &[]).unwrap().unwrap();
        assert_eq!(claimed.task.id, expected_tie_winner);

        let claimed = store
            .claim_next("frontend-agent".to_owned(), &["frontend".to_owned()])
            .unwrap()
            .unwrap();
        assert_eq!(claimed.task.title, "frontend");

        let claimed = store.claim_next("none-2".to_owned(), &[]).unwrap().unwrap();
        let other_tie = if expected_tie_winner == oldest_id {
            newest_id
        } else {
            oldest_id
        };
        assert_eq!(claimed.task.id, other_tie);
        assert_eq!(store.get(lower_id).unwrap().task.status, Status::Todo);
        assert_eq!(claimed.task.status, Status::InProgress);
    }

    #[test]
    fn timestamp_text_order_matches_chronological_order() {
        let mut store = Store::in_memory();
        let whole_second: Timestamp = "2026-09-11T17:37:35Z".parse().unwrap();
        let half_second: Timestamp = "2026-09-11T17:37:35.5Z".parse().unwrap();
        let whole_second_id = Uuid::new_v4();
        let half_second_id = Uuid::new_v4();

        store
            .create_at(whole_second_id, request("whole second"), whole_second)
            .unwrap();
        store
            .create_at(half_second_id, request("half second"), half_second)
            .unwrap();

        let tasks = store.list().unwrap();
        assert_eq!(tasks[0].task.id, whole_second_id);
        assert_eq!(tasks[1].task.id, half_second_id);

        let claimed = store.claim_next("agent".to_owned(), &[]).unwrap().unwrap();
        assert_eq!(claimed.task.id, whole_second_id);
    }

    #[test]
    fn every_allowed_transition_and_forbidden_transitions() {
        let mut store = Store::in_memory();
        let released = create(&mut store, "released");
        store.claim(released, "a".to_owned()).unwrap();
        let task = store.release(released, "a").unwrap();
        assert_eq!(task.task.status, Status::Todo);
        assert_eq!(task.task.claimed_by, None);

        let completed = create(&mut store, "completed");
        store.claim(completed, "a".to_owned()).unwrap();
        let task = store.complete(completed, "a", Some(json!(42))).unwrap();
        assert_eq!(task.task.status, Status::Done);
        assert_eq!(task.task.result, Some(json!(42)));

        let failed = create(&mut store, "failed");
        store.claim(failed, "a".to_owned()).unwrap();
        let task = store.fail(failed, "a", Some(json!("why"))).unwrap();
        assert_eq!(task.task.status, Status::Failed);
        let task = store.retry(failed).unwrap();
        assert_eq!(task.task.status, Status::Todo);
        assert_eq!(task.task.result, None);

        let cancelled_todo = create(&mut store, "cancel todo");
        assert_eq!(
            store.cancel(cancelled_todo).unwrap().task.status,
            Status::Cancelled
        );
        assert_eq!(
            store.retry(cancelled_todo).unwrap().task.status,
            Status::Todo
        );

        let cancelled_progress = create(&mut store, "cancel progress");
        store.claim(cancelled_progress, "a".to_owned()).unwrap();
        let task = store.cancel(cancelled_progress).unwrap();
        assert_eq!(task.task.status, Status::Cancelled);
        assert_eq!(task.task.claimed_by, None);

        let cancelled_failed = create(&mut store, "cancel failed");
        store.claim(cancelled_failed, "a".to_owned()).unwrap();
        store.fail(cancelled_failed, "a", None).unwrap();
        assert_eq!(
            store.cancel(cancelled_failed).unwrap().task.status,
            Status::Cancelled
        );

        let todo = create(&mut store, "forbidden todo");
        assert!(matches!(
            store.complete(todo, "a", None),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(
            store.release(todo, "a"),
            Err(StoreError::Conflict(_))
        ));
        assert!(matches!(store.retry(todo), Err(StoreError::Conflict(_))));
        assert!(matches!(
            store.cancel(completed),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn ownership_is_enforced() {
        let mut store = Store::in_memory();
        for action in ["release", "complete", "fail"] {
            let id = create(&mut store, action);
            store.claim(id, "owner".to_owned()).unwrap();
            let result = match action {
                "release" => store.release(id, "other"),
                "complete" => store.complete(id, "other", None),
                "fail" => store.fail(id, "other", None),
                _ => unreachable!(),
            };
            assert!(matches!(result, Err(StoreError::Conflict(_))));
        }
    }

    #[test]
    fn delete_rejects_dependents_then_removes_unreferenced_task() {
        let mut store = Store::in_memory();
        let parent = create(&mut store, "parent");
        let mut child = request("child");
        child.depends_on.push(parent);
        let child = store.create(child).unwrap().task.id;
        assert!(matches!(
            store.delete(parent),
            Err(StoreError::Conflict(message)) if message.contains(&child.to_string())
        ));
        store.delete(child).unwrap();
        store.delete(parent).unwrap();
        assert!(matches!(store.get(parent), Err(StoreError::NotFound)));
    }

    #[test]
    fn reopen_round_trip_preserves_tasks_and_dependencies() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("orcis.db");
        let path = path.to_str().unwrap();
        let mut store = Store::open(path).unwrap();
        let a = create(&mut store, "A");
        let mut b_request = request("B");
        b_request.priority = 9;
        b_request.depends_on.push(a);
        let created_b = store.create(b_request).unwrap();
        let b = created_b.task.id;
        let claimed_a = store.claim(a, "agent".to_owned()).unwrap();
        drop(store);

        let reopened = Store::open(path).unwrap();
        let version: i64 = reopened
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let tasks = reopened.list().unwrap();
        assert_eq!(tasks.len(), 2);
        let reopened_a = tasks.iter().find(|task| task.task.id == a).unwrap();
        assert_eq!(reopened_a.task, claimed_a.task);
        let reopened_b = tasks.iter().find(|task| task.task.id == b).unwrap();
        assert_eq!(reopened_b.task, created_b.task);
        assert_eq!(reopened_b.blocked_by, vec![a]);
        assert!(!reopened_b.ready);
    }

    #[test]
    fn migrations_are_idempotent() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("orcis.db");
        let path = path.to_str().unwrap();
        drop(Store::open(path).unwrap());
        let reopened = Store::open(path).unwrap();
        let version: i64 = reopened
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }

    #[test]
    fn deep_cycle_detection_handles_fifty_thousand_tasks() {
        let mut store = Store::in_memory();
        let now = Timestamp::constant(1_700_000_000, 0).to_string();
        let ids: Vec<_> = (1..=50_000).map(Uuid::from_u128).collect();
        let tx = store
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO tasks (
                       id, title, description, status, priority, requires, metadata, claimed_by,
                       result, created_at, updated_at, version
                     ) VALUES (?1, ?2, '', 'todo', 0, '[]', '{}', NULL, NULL, ?3, ?3, 1)",
                )
                .unwrap();
            for (index, id) in ids.iter().enumerate() {
                insert
                    .execute(params![id.to_string(), format!("task {index}"), now])
                    .unwrap();
            }
        }
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO task_dependencies (task_id, depends_on, position)
                     VALUES (?1, ?2, 0)",
                )
                .unwrap();
            for pair in ids.windows(2) {
                insert
                    .execute(params![pair[1].to_string(), pair[0].to_string()])
                    .unwrap();
            }
        }
        tx.commit().unwrap();

        let patch = PatchTask {
            depends_on: Patch::Present(vec![*ids.last().unwrap()]),
            ..PatchTask::default()
        };
        assert!(matches!(
            store.patch(ids[0], patch),
            Err(StoreError::Conflict(message)) if message.contains("dependency cycle")
        ));
    }

    #[test]
    fn open_fails_when_parent_is_a_regular_file() {
        let directory = tempdir().unwrap();
        let regular_file = directory.path().join("file");
        fs::write(&regular_file, b"not a directory").unwrap();
        let path = regular_file.join("orcis.db");
        assert!(Store::open(path.to_str().unwrap()).is_err());
    }
}
