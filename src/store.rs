use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use jiff::Timestamp;
use rusqlite::{
    Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params, types::Type,
};
use serde_json::Value;
use uuid::Uuid;

use crate::model::{
    Artifact, CreateTask, Label, Patch, PatchTask, Requirement, RequirementInput, Status, Task,
    TaskView,
};

const MIGRATIONS: &[&str] = &[
    r#"
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
"#,
    r#"
CREATE TABLE task_requirements (
  task_id     TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  name        TEXT NOT NULL,
  description TEXT,
  position    INTEGER NOT NULL,
  PRIMARY KEY (task_id, name)
);
CREATE INDEX task_requirements_name ON task_requirements(name);
INSERT INTO task_requirements (task_id, name, description, position)
  SELECT tasks.id, je.value, NULL, je.key FROM tasks, json_each(tasks.requires) AS je;
ALTER TABLE tasks DROP COLUMN requires;
"#,
    r#"
ALTER TABLE tasks ADD COLUMN closed_at TEXT;
UPDATE tasks
SET closed_at = updated_at
WHERE status IN ('done', 'cancelled');
CREATE INDEX tasks_closed_at ON tasks(closed_at) WHERE closed_at IS NOT NULL;
CREATE TABLE artifacts (
  id           TEXT PRIMARY KEY,
  task_id      TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
  filename     TEXT NOT NULL,
  content_type TEXT NOT NULL,
  size_bytes   INTEGER NOT NULL CHECK (size_bytes >= 0),
  created_at   TEXT NOT NULL
);
CREATE INDEX artifacts_task_id ON artifacts(task_id, created_at, id);
"#,
];

const LABELS_QUERY: &str = "SELECT tr.name,
       (
         SELECT tr2.description
         FROM task_requirements tr2
         JOIN tasks t2 ON t2.id = tr2.task_id
         WHERE tr2.name = tr.name
           AND tr2.description IS NOT NULL
           AND t2.status IN ('todo', 'in_progress', 'failed')
         ORDER BY t2.updated_at DESC, t2.id DESC
         LIMIT 1
       ) AS description,
       COUNT(*) AS open_tasks,
       SUM(
         CASE WHEN t.status = 'todo' AND NOT EXISTS (
           SELECT 1
           FROM task_dependencies td
           JOIN tasks d ON d.id = td.depends_on
           WHERE td.task_id = t.id AND d.status <> 'done'
         ) THEN 1 ELSE 0 END
       ) AS ready_tasks
FROM task_requirements tr
JOIN tasks t ON t.id = tr.task_id
WHERE t.status IN ('todo', 'in_progress', 'failed')
  AND (?1 IS NULL OR tr.name = ?1)
GROUP BY tr.name
ORDER BY tr.name";

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
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
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
        let requires = validate_requirements(request.requires)?;
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
        replace_requirements(&tx, task.id, &task.requires)?;
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

    pub fn labels(&mut self) -> Result<Vec<Label>, StoreError> {
        let tx = self.conn.transaction()?;
        let labels = read_labels(&tx, None)?;
        tx.commit()?;
        Ok(labels)
    }

    pub fn set_label_description(
        &mut self,
        name: &str,
        description: Option<String>,
    ) -> Result<Label, StoreError> {
        let name = name.trim();
        validate_label_name(name)?;
        let description = validate_requirement_description(description)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE tasks
             SET updated_at = ?3, version = version + 1
             WHERE status IN ('todo', 'in_progress', 'failed')
               AND EXISTS (
                 SELECT 1 FROM task_requirements tr
                 WHERE tr.task_id = tasks.id
                   AND tr.name = ?1
                   AND tr.description IS NOT ?2
               )",
            params![name, description, format!("{:.9}", Timestamp::now())],
        )?;
        tx.execute(
            "UPDATE task_requirements
             SET description = ?2
             WHERE name = ?1
               AND description IS NOT ?2
               AND task_id IN (
                 SELECT id FROM tasks
                 WHERE status IN ('todo', 'in_progress', 'failed')
               )",
            params![name, description],
        )?;
        let label = read_labels(&tx, Some(name))?
            .into_iter()
            .next()
            .ok_or(StoreError::NotFound)?;
        tx.commit()?;
        Ok(label)
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
            task.requires = validate_requirements(requires)?;
            replace_requirements(&tx, id, &task.requires)?;
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

    pub fn add_artifact(
        &mut self,
        agent: &str,
        mut artifact: Artifact,
    ) -> Result<Artifact, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_artifact_upload(
            &tx,
            artifact.task_id,
            agent,
            &artifact.filename,
            &artifact.content_type,
        )?;
        artifact.filename = artifact.filename.trim().to_owned();
        tx.execute(
            "INSERT INTO artifacts (
               id, task_id, filename, content_type, size_bytes, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                artifact.id.to_string(),
                artifact.task_id.to_string(),
                artifact.filename,
                artifact.content_type,
                sql_integer(artifact.size_bytes, "artifact size")?,
                format!("{:.9}", artifact.created_at),
            ],
        )?;
        tx.commit()?;
        Ok(artifact)
    }

    pub fn validate_artifact_upload(
        &self,
        task_id: Uuid,
        agent: &str,
        filename: &str,
        content_type: &str,
    ) -> Result<(), StoreError> {
        validate_artifact_upload(&self.conn, task_id, agent, filename, content_type)
    }

    pub fn artifacts(&self, task_id: Uuid) -> Result<Vec<Artifact>, StoreError> {
        read_task(&self.conn, task_id)?;
        let mut statement = self.conn.prepare(
            "SELECT id, task_id, filename, content_type, size_bytes, created_at
             FROM artifacts
             WHERE task_id = ?1
             ORDER BY created_at, id",
        )?;
        statement
            .query_map([task_id.to_string()], decode_artifact)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn artifact(&self, task_id: Uuid, id: Uuid) -> Result<Artifact, StoreError> {
        self.conn
            .query_row(
                "SELECT id, task_id, filename, content_type, size_bytes, created_at
                 FROM artifacts
                 WHERE task_id = ?1 AND id = ?2",
                params![task_id.to_string(), id.to_string()],
                decode_artifact,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    pub fn delete_artifact(&mut self, task_id: Uuid, id: Uuid) -> Result<Artifact, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let artifact = tx
            .query_row(
                "SELECT id, task_id, filename, content_type, size_bytes, created_at
                 FROM artifacts
                 WHERE task_id = ?1 AND id = ?2",
                params![task_id.to_string(), id.to_string()],
                decode_artifact,
            )
            .optional()?
            .ok_or(StoreError::NotFound)?;
        tx.execute("DELETE FROM artifacts WHERE id = ?1", [id.to_string()])?;
        tx.commit()?;
        Ok(artifact)
    }

    pub fn take_expired_artifacts(
        &mut self,
        cutoff: Timestamp,
    ) -> Result<Vec<Artifact>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cutoff = format!("{cutoff:.9}");
        let artifacts = {
            let mut statement = tx.prepare(
                "SELECT a.id, a.task_id, a.filename, a.content_type, a.size_bytes, a.created_at
                 FROM artifacts a
                 JOIN tasks t ON t.id = a.task_id
                 WHERE t.status IN ('done', 'cancelled')
                   AND t.closed_at <= ?1
                 ORDER BY a.id",
            )?;
            statement
                .query_map([&cutoff], decode_artifact)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute(
            "DELETE FROM artifacts
             WHERE id IN (
               SELECT a.id
               FROM artifacts a
               JOIN tasks t ON t.id = a.task_id
               WHERE t.status IN ('done', 'cancelled')
                 AND t.closed_at <= ?1
             )",
            [&cutoff],
        )?;
        tx.commit()?;
        Ok(artifacts)
    }

    pub fn artifact_ids(&self) -> Result<HashSet<Uuid>, StoreError> {
        let mut statement = self.conn.prepare("SELECT id FROM artifacts")?;
        statement
            .query_map([], |row| parse_uuid(0, &row.get::<_, String>(0)?))?
            .collect::<rusqlite::Result<HashSet<_>>>()
            .map_err(Into::into)
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
        let capabilities = encode_json(&capabilities)?;
        let candidate = tx
            .query_row(
                "SELECT id
                 FROM tasks
                 WHERE status = 'todo'
                   AND NOT EXISTS (
                     SELECT 1
                     FROM task_dependencies td
                     JOIN tasks d ON d.id = td.depends_on
                     WHERE td.task_id = tasks.id AND d.status <> 'done'
                   )
                   AND NOT EXISTS (
                     SELECT 1
                     FROM task_requirements tr
                     WHERE tr.task_id = tasks.id
                       AND NOT EXISTS (
                         SELECT 1 FROM json_each(?1) capability
                         WHERE capability.value = tr.name
                       )
                   )
                 ORDER BY priority DESC, created_at ASC, id ASC
                 LIMIT 1",
                [capabilities],
                |row| parse_uuid(0, &row.get::<_, String>(0)?),
            )
            .optional()?;

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
        set_closed_at(&tx, task.id, Some(task.updated_at))?;
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
        set_closed_at(&tx, task.id, None)?;
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
        if matches!(action, OwnedAction::Complete) {
            set_closed_at(&tx, task.id, Some(task.updated_at))?;
        }
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
    let metadata = encode_json(&task.metadata)?;
    let result = task.result.as_ref().map(encode_json).transpose()?;
    let version = sql_integer(task.version, "task version")?;
    conn.execute(
        "INSERT INTO tasks (
           id, title, description, status, priority, metadata, claimed_by, result,
           created_at, updated_at, version
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            task.id.to_string(),
            task.title,
            task.description,
            status_text(task.status),
            task.priority,
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
    let metadata = encode_json(&task.metadata)?;
    let result = task.result.as_ref().map(encode_json).transpose()?;
    conn.execute(
        "UPDATE tasks
         SET title = ?2, description = ?3, status = ?4, priority = ?5,
             metadata = ?6, claimed_by = ?7, result = ?8, updated_at = ?9,
             version = version + 1
         WHERE id = ?1",
        params![
            task.id.to_string(),
            task.title,
            task.description,
            status_text(task.status),
            task.priority,
            metadata,
            task.claimed_by,
            result,
            format!("{:.9}", task.updated_at),
        ],
    )?;
    Ok(())
}

fn set_closed_at(
    conn: &Connection,
    id: Uuid,
    closed_at: Option<Timestamp>,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE tasks SET closed_at = ?2 WHERE id = ?1",
        params![
            id.to_string(),
            closed_at.map(|timestamp| format!("{timestamp:.9}"))
        ],
    )?;
    Ok(())
}

fn replace_requirements(
    conn: &Connection,
    task_id: Uuid,
    requirements: &[Requirement],
) -> Result<(), StoreError> {
    conn.execute(
        "DELETE FROM task_requirements WHERE task_id = ?1",
        [task_id.to_string()],
    )?;
    let mut statement = conn.prepare(
        "INSERT INTO task_requirements (task_id, name, description, position)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (position, requirement) in requirements.iter().enumerate() {
        statement.execute(params![
            task_id.to_string(),
            requirement.name,
            requirement.description,
            sql_integer(position, "requirement position")?,
        ])?;
    }
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
            "SELECT id, title, description, status, priority, metadata, claimed_by,
                    result, created_at, updated_at, version
             FROM tasks WHERE id = ?1",
            [id.to_string()],
            decode_task,
        )
        .optional()?
        .ok_or(StoreError::NotFound)?;
    task.requires = requirement_rows(conn, id)?;
    task.depends_on = dependency_ids(conn, id)?;
    Ok(task)
}

fn decode_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    let id_text: String = row.get(0)?;
    let status: String = row.get(3)?;
    let metadata: String = row.get(5)?;
    let result: Option<String> = row.get(7)?;
    let created_at: String = row.get(8)?;
    let updated_at: String = row.get(9)?;
    let version: i64 = row.get(10)?;
    Ok(Task {
        id: parse_uuid(0, &id_text)?,
        title: row.get(1)?,
        description: row.get(2)?,
        status: parse_status(3, &status)?,
        priority: row.get(4)?,
        requires: Vec::new(),
        depends_on: Vec::new(),
        metadata: decode_json(5, &metadata)?,
        claimed_by: row.get(6)?,
        result: result
            .as_deref()
            .map(|value| decode_json(7, value))
            .transpose()?,
        created_at: parse_timestamp(8, &created_at)?,
        updated_at: parse_timestamp(9, &updated_at)?,
        version: version
            .try_into()
            .map_err(|error| conversion_error(10, Type::Integer, error))?,
    })
}

fn decode_artifact(row: &Row<'_>) -> rusqlite::Result<Artifact> {
    let id: String = row.get(0)?;
    let task_id: String = row.get(1)?;
    let size_bytes: i64 = row.get(4)?;
    let created_at: String = row.get(5)?;
    Ok(Artifact {
        id: parse_uuid(0, &id)?,
        task_id: parse_uuid(1, &task_id)?,
        filename: row.get(2)?,
        content_type: row.get(3)?,
        size_bytes: size_bytes
            .try_into()
            .map_err(|error| conversion_error(4, Type::Integer, error))?,
        created_at: parse_timestamp(5, &created_at)?,
    })
}

fn list_views(conn: &Connection) -> rusqlite::Result<Vec<TaskView>> {
    let mut statement = conn.prepare(
        "SELECT id, title, description, status, priority, metadata, claimed_by,
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
        "SELECT task_id, name, description
         FROM task_requirements
         ORDER BY task_id, position",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let task_id = parse_uuid(0, &row.get::<_, String>(0)?)?;
        if let Some(&index) = positions.get(&task_id) {
            views[index].task.requires.push(Requirement {
                name: row.get(1)?,
                description: row.get(2)?,
            });
        }
    }
    drop(rows);
    drop(statement);

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

fn requirement_rows(conn: &Connection, id: Uuid) -> rusqlite::Result<Vec<Requirement>> {
    let mut statement = conn.prepare(
        "SELECT name, description
         FROM task_requirements
         WHERE task_id = ?1
         ORDER BY position",
    )?;
    statement
        .query_map([id.to_string()], |row| {
            Ok(Requirement {
                name: row.get(0)?,
                description: row.get(1)?,
            })
        })?
        .collect()
}

fn read_labels(conn: &Connection, name: Option<&str>) -> rusqlite::Result<Vec<Label>> {
    let mut statement = conn.prepare(LABELS_QUERY)?;
    statement
        .query_map([name], |row| {
            let open_tasks: i64 = row.get(2)?;
            let ready_tasks: i64 = row.get(3)?;
            Ok(Label {
                name: row.get(0)?,
                description: row.get(1)?,
                open_tasks: open_tasks
                    .try_into()
                    .map_err(|error| conversion_error(2, Type::Integer, error))?,
                ready_tasks: ready_tasks
                    .try_into()
                    .map_err(|error| conversion_error(3, Type::Integer, error))?,
            })
        })?
        .collect()
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

fn validate_artifact_filename(filename: &str) -> Result<(), StoreError> {
    if filename.is_empty() {
        return Err(StoreError::Invalid(
            "artifact filename must not be empty".to_owned(),
        ));
    }
    if filename.chars().count() > 255 {
        return Err(StoreError::Invalid(
            "artifact filename must be at most 255 characters".to_owned(),
        ));
    }
    if filename.chars().any(char::is_control) {
        return Err(StoreError::Invalid(
            "artifact filename must not contain control characters".to_owned(),
        ));
    }
    Ok(())
}

fn validate_artifact_owner(
    conn: &Connection,
    task_id: Uuid,
    agent: &str,
) -> Result<(), StoreError> {
    let task = read_task(conn, task_id)?;
    if task.status != Status::InProgress {
        return Err(StoreError::Conflict(format!(
            "cannot add an artifact to a task in status {}",
            status_text(task.status)
        )));
    }
    if task.claimed_by.as_deref() != Some(agent) {
        return Err(StoreError::Conflict(format!(
            "task is not claimed by agent {agent}"
        )));
    }
    Ok(())
}

fn validate_artifact_upload(
    conn: &Connection,
    task_id: Uuid,
    agent: &str,
    filename: &str,
    content_type: &str,
) -> Result<(), StoreError> {
    validate_artifact_owner(conn, task_id, agent)?;
    validate_artifact_filename(filename.trim())?;
    validate_content_type(content_type)
}

fn validate_content_type(content_type: &str) -> Result<(), StoreError> {
    if content_type.is_empty() || content_type.len() > 255 {
        return Err(StoreError::Invalid(
            "artifact content type must contain 1 through 255 bytes".to_owned(),
        ));
    }
    Ok(())
}

fn validate_requirements(inputs: Vec<RequirementInput>) -> Result<Vec<Requirement>, StoreError> {
    let mut requirements: Vec<Requirement> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();
    for input in inputs {
        let (name, description) = match input {
            RequirementInput::Name(name) => (name, None),
            RequirementInput::Full(requirement) => (requirement.name, requirement.description),
        };
        let name = name.trim().to_owned();
        validate_requirement_name(&name)?;
        let description = validate_requirement_description(description)?;
        if let Some(&position) = positions.get(&name) {
            if requirements[position].description.is_none() && description.is_some() {
                requirements[position].description = description;
            }
        } else {
            positions.insert(name.clone(), requirements.len());
            requirements.push(Requirement { name, description });
        }
    }
    Ok(requirements)
}

fn validate_requirement_name(name: &str) -> Result<(), StoreError> {
    validate_name(name, "requirement")
}

fn validate_label_name(name: &str) -> Result<(), StoreError> {
    validate_name(name, "label")
}

fn validate_name(name: &str, kind: &str) -> Result<(), StoreError> {
    if name.trim().is_empty() {
        return Err(StoreError::Invalid(format!(
            "invalid {kind} name: must not be empty"
        )));
    }
    if name.chars().count() > 100 {
        return Err(StoreError::Invalid(format!(
            "invalid {kind} name: must be at most 100 characters"
        )));
    }
    Ok(())
}

fn validate_requirement_description(
    description: Option<String>,
) -> Result<Option<String>, StoreError> {
    let description = description.map(|value| value.trim().to_owned());
    let description = description.filter(|value| !value.is_empty());
    if description
        .as_ref()
        .is_some_and(|value| value.chars().count() > 1000)
    {
        return Err(StoreError::Invalid(
            "requirement description must be at most 1000 characters".to_owned(),
        ));
    }
    Ok(description)
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

    fn requirement(name: &str, description: Option<&str>) -> RequirementInput {
        RequirementInput::Full(Requirement {
            name: name.to_owned(),
            description: description.map(str::to_owned),
        })
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
    fn requirements_round_trip_and_deduplicate_descriptions() {
        let mut store = Store::in_memory();
        let mut task = request("requirements");
        task.requires = vec![
            RequirementInput::Name(" rust ".to_owned()),
            requirement("architecture", None),
            requirement("rust", Some("systems work")),
            requirement("architecture", Some("module boundaries")),
            requirement("rust", Some("ignored later description")),
        ];

        let created = store.create(task).unwrap();
        assert_eq!(
            created.task.requires,
            vec![
                Requirement {
                    name: "rust".to_owned(),
                    description: Some("systems work".to_owned()),
                },
                Requirement {
                    name: "architecture".to_owned(),
                    description: Some("module boundaries".to_owned()),
                },
            ]
        );
        assert_eq!(
            store.get(created.task.id).unwrap().task.requires,
            created.task.requires
        );
    }

    #[test]
    fn labels_include_only_open_tasks_with_counts_and_latest_description() {
        let mut store = Store::in_memory();
        let blocker = create(&mut store, "blocker");

        let mut old = request("old description");
        old.requires = vec![
            requirement("shared", Some("old")),
            requirement("old-only", Some("open")),
        ];
        old.depends_on = vec![blocker];
        let old_id = store
            .create_at(Uuid::new_v4(), old, Timestamp::constant(1_700_000_000, 0))
            .unwrap()
            .task
            .id;

        let mut new = request("new description");
        new.requires = vec![requirement("shared", Some("new"))];
        store
            .create_at(Uuid::new_v4(), new, Timestamp::constant(1_700_000_001, 0))
            .unwrap();

        let mut done = request("done");
        done.requires = vec![
            requirement("shared", Some("closed")),
            requirement("closed-only", Some("gone")),
        ];
        let done_id = store.create(done).unwrap().task.id;
        store.claim(done_id, "agent".to_owned()).unwrap();
        store.complete(done_id, "agent", None).unwrap();

        let mut cancelled = request("cancelled");
        cancelled.requires = vec![requirement("cancelled-only", None)];
        let cancelled_id = store.create(cancelled).unwrap().task.id;
        store.cancel(cancelled_id).unwrap();

        let labels = store.labels().unwrap();
        assert_eq!(
            labels,
            vec![
                Label {
                    name: "old-only".to_owned(),
                    description: Some("open".to_owned()),
                    open_tasks: 1,
                    ready_tasks: 0,
                },
                Label {
                    name: "shared".to_owned(),
                    description: Some("new".to_owned()),
                    open_tasks: 2,
                    ready_tasks: 1,
                },
            ]
        );

        store
            .patch(
                old_id,
                PatchTask {
                    requires: Patch::Present(vec![
                        requirement("shared", Some("patched")),
                        requirement("old-only", Some("open")),
                    ]),
                    ..PatchTask::default()
                },
            )
            .unwrap();
        let shared = store
            .labels()
            .unwrap()
            .into_iter()
            .find(|label| label.name == "shared")
            .unwrap();
        assert_eq!(shared.description.as_deref(), Some("patched"));
    }

    #[test]
    fn setting_label_description_updates_open_tasks_only() {
        let mut store = Store::in_memory();
        let mut first = request("first");
        first.requires = vec![requirement("design", None)];
        let first = store.create(first).unwrap().task;
        let mut second = request("second");
        second.requires = vec![requirement("design", Some("before"))];
        let second = store.create(second).unwrap().task;
        let mut closed = request("closed");
        closed.requires = vec![requirement("design", Some("closed description"))];
        let closed = store.create(closed).unwrap().task;
        store.claim(closed.id, "agent".to_owned()).unwrap();
        let closed = store.complete(closed.id, "agent", None).unwrap().task;

        let label = store
            .set_label_description("design", Some("  shared meaning  ".to_owned()))
            .unwrap();
        assert_eq!(label.description.as_deref(), Some("shared meaning"));
        assert_eq!(label.open_tasks, 2);
        for task in [&first, &second] {
            let updated = store.get(task.id).unwrap().task;
            assert_eq!(updated.version, task.version + 1);
            assert_eq!(
                updated.requires[0].description.as_deref(),
                Some("shared meaning")
            );
        }
        store
            .set_label_description("design", Some("shared meaning".to_owned()))
            .unwrap();
        for task in [&first, &second] {
            let updated = store.get(task.id).unwrap().task;
            assert_eq!(updated.version, task.version + 1);
        }
        let closed_after = store.get(closed.id).unwrap().task;
        assert_eq!(closed_after.version, closed.version);
        assert_eq!(
            closed_after.requires[0].description.as_deref(),
            Some("closed description")
        );
        assert!(matches!(
            store.set_label_description("unknown", None),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn claim_next_matches_requirement_names_only() {
        let mut store = Store::in_memory();
        let mut task = request("described");
        task.requires = vec![requirement("architecture", Some("module boundaries"))];
        let id = store.create(task).unwrap().task.id;
        let claimed = store
            .claim_next("agent".to_owned(), &["architecture".to_owned()])
            .unwrap()
            .unwrap();
        assert_eq!(claimed.task.id, id);
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
        frontend.requires = vec![RequirementInput::Name("frontend".to_owned())];
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
    fn artifacts_require_ownership_and_expire_only_with_closed_tasks() {
        let mut store = Store::in_memory();
        let done = create(&mut store, "done");
        let failed = create(&mut store, "failed");
        let cancelled = create(&mut store, "cancelled");

        assert!(matches!(
            store.add_artifact(
                "agent",
                Artifact {
                    id: Uuid::new_v4(),
                    task_id: done,
                    filename: "before.txt".to_owned(),
                    content_type: "text/plain".to_owned(),
                    size_bytes: 1,
                    created_at: Timestamp::now(),
                },
            ),
            Err(StoreError::Conflict(_))
        ));

        let mut artifacts = Vec::new();
        for task_id in [done, failed, cancelled] {
            store.claim(task_id, "agent".to_owned()).unwrap();
            let artifact = store
                .add_artifact(
                    "agent",
                    Artifact {
                        id: Uuid::new_v4(),
                        task_id,
                        filename: format!("{task_id}.bin"),
                        content_type: "application/octet-stream".to_owned(),
                        size_bytes: 42,
                        created_at: Timestamp::now(),
                    },
                )
                .unwrap();
            artifacts.push(artifact);
        }
        store.complete(done, "agent", None).unwrap();
        store.fail(failed, "agent", None).unwrap();
        store.cancel(cancelled).unwrap();

        let cutoff = Timestamp::from_second(Timestamp::now().as_second() + 1).unwrap();
        let expired = store.take_expired_artifacts(cutoff).unwrap();
        assert_eq!(
            expired
                .iter()
                .map(|artifact| artifact.task_id)
                .collect::<HashSet<_>>(),
            HashSet::from([done, cancelled])
        );
        assert!(store.artifacts(done).unwrap().is_empty());
        assert_eq!(store.artifacts(failed).unwrap(), vec![artifacts[1].clone()]);
        assert!(store.artifacts(cancelled).unwrap().is_empty());
    }

    #[test]
    fn artifact_retention_uses_the_closure_time() {
        let mut store = Store::in_memory();
        let task_id = create(&mut store, "closed");
        store.claim(task_id, "agent".to_owned()).unwrap();
        let artifact = Artifact {
            id: Uuid::new_v4(),
            task_id,
            filename: "result.bin".to_owned(),
            content_type: "application/octet-stream".to_owned(),
            size_bytes: 1,
            created_at: Timestamp::now(),
        };
        store.add_artifact("agent", artifact.clone()).unwrap();
        store.complete(task_id, "agent", None).unwrap();
        let closed_at: String = store
            .conn
            .query_row(
                "SELECT closed_at FROM tasks WHERE id = ?1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();

        store.patch(task_id, PatchTask::default()).unwrap();
        let closed_after_patch: String = store
            .conn
            .query_row(
                "SELECT closed_at FROM tasks WHERE id = ?1",
                [task_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(closed_after_patch, closed_at);
        assert_eq!(
            store
                .take_expired_artifacts(closed_at.parse().unwrap())
                .unwrap(),
            vec![artifact]
        );
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
        assert_eq!(version, 3);
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
        assert_eq!(version, 3);
    }

    #[test]
    fn migrates_version_one_requirement_json_into_rows() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("version-one.db");
        let id = Uuid::new_v4();
        let timestamp = "2026-09-13T12:00:00.000000000Z";
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute(
                "INSERT INTO tasks (
                   id, title, description, status, priority, requires, metadata, claimed_by,
                   result, created_at, updated_at, version
                 ) VALUES (?1, 'migrated', '', 'todo', 0, '[\"rust\",\"api\"]', '{}',
                           NULL, NULL, ?2, ?2, 1)",
                params![id.to_string(), timestamp],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        let store = Store::open(path.to_str().unwrap()).unwrap();
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 3);
        let rows = requirement_rows(&store.conn, id).unwrap();
        assert_eq!(
            rows,
            vec![
                Requirement {
                    name: "rust".to_owned(),
                    description: None,
                },
                Requirement {
                    name: "api".to_owned(),
                    description: None,
                },
            ]
        );
        assert_eq!(store.get(id).unwrap().task.requires, rows);
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
                       id, title, description, status, priority, metadata, claimed_by,
                       result, created_at, updated_at, version
                     ) VALUES (?1, ?2, '', 'todo', 0, '{}', NULL, NULL, ?3, ?3, 1)",
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
