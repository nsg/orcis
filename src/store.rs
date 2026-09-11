use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashSet},
    fs,
    io::{self, ErrorKind},
    path::PathBuf,
};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::model::{CreateTask, Patch, PatchTask, Status, Task, TaskView};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    NotFound,
    Invalid(String),
    Conflict(String),
    Internal(String),
}

#[derive(Debug)]
pub struct Store {
    tasks: BTreeMap<Uuid, Task>,
    path: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum OwnedAction {
    Release,
    Complete,
    Fail,
}

#[derive(Deserialize, Serialize)]
struct PersistedBoard {
    tasks: BTreeMap<Uuid, Task>,
}

impl Store {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            tasks: BTreeMap::new(),
            path,
        }
    }

    pub fn load(path: Option<PathBuf>) -> io::Result<Self> {
        let Some(board_path) = path else {
            return Ok(Self::new(None));
        };

        let bytes = match fs::read(&board_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(Self::new(Some(board_path)));
            }
            Err(error) => return Err(error),
        };
        let board: PersistedBoard = serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
        let store = Self {
            tasks: board.tasks,
            path: Some(board_path),
        };
        store.validate_loaded_graph()?;
        Ok(store)
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let mut temporary = path.as_os_str().to_os_string();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        let bytes = serde_json::to_vec_pretty(&PersistedBoard {
            tasks: self.tasks.clone(),
        })
        .map_err(io::Error::other)?;
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, path)
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
        self.commit(move |store| {
            validate_title(&request.title)?;
            let depends_on = deduplicate(request.depends_on);
            store.validate_dependencies(id, &depends_on)?;
            store.validate_cycle(id, &depends_on)?;
            let task = Task {
                id,
                title: request.title,
                description: request.description,
                status: Status::Todo,
                priority: request.priority,
                requires: deduplicate(request.requires),
                depends_on,
                metadata: request.metadata,
                claimed_by: None,
                result: None,
                created_at: now,
                updated_at: now,
                version: 1,
            };
            store.tasks.insert(id, task);
            store.get(id)
        })
    }

    pub fn get(&self, id: Uuid) -> Result<TaskView, StoreError> {
        let task = self.tasks.get(&id).ok_or(StoreError::NotFound)?;
        Ok(self.view(task))
    }

    pub fn list(&self) -> Vec<TaskView> {
        let mut tasks: Vec<_> = self.tasks.values().map(|task| self.view(task)).collect();
        tasks.sort_by(|left, right| {
            right
                .task
                .priority
                .cmp(&left.task.priority)
                .then_with(|| left.task.created_at.cmp(&right.task.created_at))
                .then_with(|| left.task.id.cmp(&right.task.id))
        });
        tasks
    }

    pub fn patch(&mut self, id: Uuid, patch: PatchTask) -> Result<TaskView, StoreError> {
        self.commit(move |store| {
            if !store.tasks.contains_key(&id) {
                return Err(StoreError::NotFound);
            }
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
            let requires = map_patch(requires, deduplicate);
            let depends_on = map_patch(depends_on, deduplicate);
            if let Patch::Present(dependencies) = &depends_on {
                store.validate_dependencies(id, dependencies)?;
                store.validate_cycle(id, dependencies)?;
            }

            let task = store.tasks.get_mut(&id).expect("existence checked above");
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
                task.requires = requires;
            }
            if let Patch::Present(depends_on) = depends_on {
                task.depends_on = depends_on;
            }
            if let Patch::Present(metadata) = metadata {
                task.metadata = metadata;
            }
            touch(task);
            store.get(id)
        })
    }

    pub fn delete(&mut self, id: Uuid) -> Result<(), StoreError> {
        self.commit(|store| {
            if !store.tasks.contains_key(&id) {
                return Err(StoreError::NotFound);
            }
            let dependents = store.dependent_ids(id);
            if !dependents.is_empty() {
                return Err(StoreError::Conflict(format!(
                    "task has dependents: {}",
                    join_ids(&dependents)
                )));
            }
            store.tasks.remove(&id);
            Ok(())
        })
    }

    pub fn claim(&mut self, id: Uuid, agent: String) -> Result<TaskView, StoreError> {
        self.commit(move |store| store.claim_uncommitted(id, agent))
    }

    fn claim_uncommitted(&mut self, id: Uuid, agent: String) -> Result<TaskView, StoreError> {
        let blocked_by = self.blocked_ids(id)?;
        let task = self.tasks.get_mut(&id).ok_or(StoreError::NotFound)?;
        if task.status != Status::Todo {
            return Err(action_conflict("claim", task.status));
        }
        if !blocked_by.is_empty() {
            return Err(StoreError::Conflict(format!(
                "task is blocked by: {}",
                join_ids(&blocked_by)
            )));
        }
        task.status = Status::InProgress;
        task.claimed_by = Some(agent);
        touch(task);
        self.get(id)
    }

    pub fn claim_next(
        &mut self,
        agent: String,
        capabilities: &[String],
    ) -> Result<Option<TaskView>, StoreError> {
        let capabilities: HashSet<&str> = capabilities.iter().map(String::as_str).collect();
        let id = self
            .tasks
            .values()
            .filter(|task| task.status == Status::Todo)
            .filter(|task| {
                task.requires
                    .iter()
                    .all(|requirement| capabilities.contains(requirement.as_str()))
            })
            .filter(|task| {
                task.depends_on.iter().all(|dependency| {
                    self.tasks
                        .get(dependency)
                        .is_some_and(|task| task.status == Status::Done)
                })
            })
            .min_by_key(|task| (Reverse(task.priority), task.created_at, task.id))
            .map(|task| task.id);
        match id {
            Some(id) => self
                .commit(move |store| store.claim_uncommitted(id, agent))
                .map(Some),
            None => Ok(None),
        }
    }

    pub fn release(&mut self, id: Uuid, agent: &str) -> Result<TaskView, StoreError> {
        self.commit(|store| store.transition_owned(id, agent, OwnedAction::Release, None))
    }

    pub fn complete(
        &mut self,
        id: Uuid,
        agent: &str,
        result: Option<Value>,
    ) -> Result<TaskView, StoreError> {
        self.commit(|store| store.transition_owned(id, agent, OwnedAction::Complete, result))
    }

    pub fn fail(
        &mut self,
        id: Uuid,
        agent: &str,
        result: Option<Value>,
    ) -> Result<TaskView, StoreError> {
        self.commit(|store| store.transition_owned(id, agent, OwnedAction::Fail, result))
    }

    pub fn cancel(&mut self, id: Uuid) -> Result<TaskView, StoreError> {
        self.commit(|store| {
            let task = store.tasks.get_mut(&id).ok_or(StoreError::NotFound)?;
            if !matches!(
                task.status,
                Status::Todo | Status::InProgress | Status::Failed
            ) {
                return Err(action_conflict("cancel", task.status));
            }
            task.status = Status::Cancelled;
            task.claimed_by = None;
            touch(task);
            store.get(id)
        })
    }

    pub fn retry(&mut self, id: Uuid) -> Result<TaskView, StoreError> {
        self.commit(|store| {
            let task = store.tasks.get_mut(&id).ok_or(StoreError::NotFound)?;
            if !matches!(task.status, Status::Failed | Status::Cancelled) {
                return Err(action_conflict("retry", task.status));
            }
            task.status = Status::Todo;
            task.claimed_by = None;
            task.result = None;
            touch(task);
            store.get(id)
        })
    }

    fn transition_owned(
        &mut self,
        id: Uuid,
        agent: &str,
        action: OwnedAction,
        result: Option<Value>,
    ) -> Result<TaskView, StoreError> {
        let task = self.tasks.get_mut(&id).ok_or(StoreError::NotFound)?;
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
        }
        if !matches!(action, OwnedAction::Release) {
            task.result = result;
        }
        touch(task);
        self.get(id)
    }

    fn view(&self, task: &Task) -> TaskView {
        let blocked_by = self.blocked_ids(task.id).unwrap_or_default();
        let ready = task.status == Status::Todo && blocked_by.is_empty();
        TaskView {
            task: task.clone(),
            blocked_by,
            dependents: self.dependent_ids(task.id),
            ready,
        }
    }

    fn blocked_ids(&self, id: Uuid) -> Result<Vec<Uuid>, StoreError> {
        let task = self.tasks.get(&id).ok_or(StoreError::NotFound)?;
        Ok(task
            .depends_on
            .iter()
            .copied()
            .filter(|dependency| {
                self.tasks
                    .get(dependency)
                    .is_none_or(|task| task.status != Status::Done)
            })
            .collect())
    }

    fn dependent_ids(&self, id: Uuid) -> Vec<Uuid> {
        self.tasks
            .values()
            .filter(|task| task.depends_on.contains(&id))
            .map(|task| task.id)
            .collect()
    }

    fn validate_dependencies(&self, id: Uuid, dependencies: &[Uuid]) -> Result<(), StoreError> {
        if dependencies.contains(&id) {
            return Err(StoreError::Invalid(format!(
                "task cannot depend on itself: {id}"
            )));
        }
        if let Some(missing) = dependencies
            .iter()
            .find(|dependency| !self.tasks.contains_key(dependency))
        {
            return Err(StoreError::Invalid(format!(
                "dependency does not exist: {missing}"
            )));
        }
        Ok(())
    }

    fn validate_cycle(&self, id: Uuid, dependencies: &[Uuid]) -> Result<(), StoreError> {
        for dependency in dependencies {
            let mut visited = HashSet::new();
            if self.reaches(*dependency, id, &mut visited) {
                return Err(StoreError::Conflict(format!(
                    "dependency cycle involving: {id}, {dependency}"
                )));
            }
        }
        Ok(())
    }

    fn reaches(&self, current: Uuid, target: Uuid, visited: &mut HashSet<Uuid>) -> bool {
        let mut stack = vec![current];
        while let Some(current) = stack.pop() {
            if current == target {
                return true;
            }
            if !visited.insert(current) {
                continue;
            }
            if let Some(task) = self.tasks.get(&current) {
                stack.extend(task.depends_on.iter().copied());
            }
        }
        false
    }

    fn persist(&self) -> Result<(), StoreError> {
        self.save()
            .map_err(|error| StoreError::Internal(format!("failed to persist board: {error}")))
    }

    fn commit<T>(
        &mut self,
        mutate: impl FnOnce(&mut Self) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let snapshot = self.path.as_ref().map(|_| self.tasks.clone());
        let result = match mutate(self) {
            Ok(result) => result,
            Err(error) => {
                if let Some(tasks) = snapshot {
                    self.tasks = tasks;
                }
                return Err(error);
            }
        };
        if let Err(error) = self.persist() {
            if let Some(tasks) = snapshot {
                self.tasks = tasks;
            }
            return Err(error);
        }
        Ok(result)
    }

    fn validate_loaded_graph(&self) -> io::Result<()> {
        for (&id, task) in &self.tasks {
            self.validate_dependencies(id, &task.depends_on)
                .and_then(|()| self.validate_cycle(id, &task.depends_on))
                .map_err(|error| {
                    io::Error::new(
                        ErrorKind::InvalidData,
                        format!("invalid task {id}: {}", store_error_message(error)),
                    )
                })?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn insert_raw(&mut self, task: Task) {
        self.tasks.insert(task.id, task);
    }

    #[cfg(test)]
    fn task_mut(&mut self, id: Uuid) -> &mut Task {
        self.tasks.get_mut(&id).expect("test task exists")
    }
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

fn store_error_message(error: StoreError) -> String {
    match error {
        StoreError::NotFound => "task not found".to_owned(),
        StoreError::Invalid(message)
        | StoreError::Conflict(message)
        | StoreError::Internal(message) => message,
    }
}

fn map_patch<T, U>(patch: Patch<T>, map: impl FnOnce(T) -> U) -> Patch<U> {
    match patch {
        Patch::Missing => Patch::Missing,
        Patch::Present(value) => Patch::Present(map(value)),
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
    let status = serde_json::to_value(status)
        .expect("status serialization is infallible")
        .as_str()
        .expect("status serializes as a string")
        .to_owned();
    StoreError::Conflict(format!("cannot {action} a task in status {status}"))
}

fn join_ids(ids: &[Uuid]) -> String {
    ids.iter()
        .map(Uuid::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
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
        let mut store = Store::new(None);
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
        let mut store = Store::new(None);
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

        let mut store = Store::new(None);
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
        let mut store = Store::new(None);
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
        let mut store = Store::new(None);
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

        let mut oldest = request("oldest");
        oldest.priority = 3;
        let oldest_id = store.create(oldest).unwrap().task.id;
        let mut newest = request("newest");
        newest.priority = 3;
        let newest_id = store.create(newest).unwrap().task.id;
        let common_time = Timestamp::constant(1_700_000_000, 0);
        store.task_mut(oldest_id).created_at = common_time;
        store.task_mut(newest_id).created_at = common_time;
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
    fn every_allowed_transition_and_forbidden_transitions() {
        let mut store = Store::new(None);
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
        let mut store = Store::new(None);
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
        let mut store = Store::new(None);
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
    fn persistence_round_trip_and_missing_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("nested").join("board.json");
        let mut store = Store::load(Some(path.clone())).unwrap();
        assert!(store.list().is_empty());
        create(&mut store, "persisted");
        store.save().unwrap();

        let loaded = Store::load(Some(path)).unwrap();
        assert_eq!(loaded.list()[0].task, store.list()[0].task);
    }

    #[test]
    fn deep_cycle_detection_is_iterative() {
        let mut store = Store::new(None);
        let now = Timestamp::constant(1_700_000_000, 0);
        let ids: Vec<_> = (1..=50_000).map(Uuid::from_u128).collect();
        for (index, &id) in ids.iter().enumerate() {
            store.insert_raw(Task {
                id,
                title: format!("task {index}"),
                description: String::new(),
                status: Status::Todo,
                priority: 0,
                requires: Vec::new(),
                depends_on: index
                    .checked_sub(1)
                    .map_or_else(Vec::new, |previous| vec![ids[previous]]),
                metadata: json!({}),
                claimed_by: None,
                result: None,
                created_at: now,
                updated_at: now,
                version: 1,
            });
        }

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
    fn persistence_failure_rolls_back_mutation() {
        let directory = tempdir().unwrap();
        let regular_file = directory.path().join("file");
        fs::write(&regular_file, b"not a directory").unwrap();
        let mut store = Store::new(Some(regular_file.join("board.json")));

        assert!(matches!(
            store.create(request("not persisted")),
            Err(StoreError::Internal(_))
        ));
        assert!(store.list().is_empty());
    }

    #[test]
    fn load_rejects_dangling_dependency() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("board.json");
        let id = Uuid::new_v4();
        let missing = Uuid::new_v4();
        let board = json!({
            "tasks": {
                (id.to_string()): {
                    "id": id,
                    "title": "dangling",
                    "description": "",
                    "status": "todo",
                    "priority": 0,
                    "requires": [],
                    "depends_on": [missing],
                    "metadata": {},
                    "claimed_by": null,
                    "result": null,
                    "created_at": "2026-09-11T00:00:00Z",
                    "updated_at": "2026-09-11T00:00:00Z",
                    "version": 1
                }
            }
        });
        fs::write(&path, serde_json::to_vec(&board).unwrap()).unwrap();

        let error = Store::load(Some(path)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(error.to_string().contains(&id.to_string()));
        assert!(error.to_string().contains(&missing.to_string()));
    }
}
