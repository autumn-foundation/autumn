//! MCP Tasks extension (`io.modelcontextprotocol/tasks`).
//!
//! A task tool can return a task handle instead of a blocking result. The
//! client polls `tasks/get` until the task is terminal. See
//! <https://modelcontextprotocol.io/extensions/tasks/overview>.
//!
//! The store is in memory and per process. A load balancer must send
//! `tasks/*` requests to the replica that holds the task (use the `Mcp-Name`
//! header). Tasks do not survive a restart.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use tokio::task::AbortHandle;

/// The extension identifier.
pub(crate) const EXTENSION_ID: &str = "io.modelcontextprotocol/tasks";

/// The `_meta` key that holds the per-request client capabilities.
const CLIENT_CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";

/// JSON-RPC error code: the client did not declare a required capability.
pub(crate) const MISSING_CAPABILITY: i64 = -32021;

/// Settings for MCP task tools.
///
/// Install with [`AppBuilder::mcp_tasks`](crate::app::AppBuilder::mcp_tasks).
///
/// ```rust
/// use std::time::Duration;
/// use autumn_web::mcp::McpTaskConfig;
///
/// let config = McpTaskConfig::default()
///     .ttl(Duration::from_secs(600))
///     .poll_interval(Duration::from_secs(2))
///     .max_tasks(256);
/// # let _ = config;
/// ```
#[derive(Clone, Debug)]
pub struct McpTaskConfig {
    ttl: Duration,
    poll_interval: Duration,
    max_tasks: usize,
}

impl Default for McpTaskConfig {
    /// TTL 1 hour, poll interval 1 second, 1024 tasks.
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(3600),
            poll_interval: Duration::from_secs(1),
            max_tasks: 1024,
        }
    }
}

impl McpTaskConfig {
    /// Time from creation until the server deletes the task. A running task
    /// is stopped when its TTL ends.
    #[must_use]
    pub const fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Poll interval that the server tells the client to use.
    #[must_use]
    pub const fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Maximum number of tasks in the store, running or terminal. When the
    /// store is full, a task tool runs as a blocking call.
    #[must_use]
    pub const fn max_tasks(mut self, max: usize) -> Self {
        self.max_tasks = max;
        self
    }
}

/// Whether the request `params` declare the tasks extension in
/// `_meta["io.modelcontextprotocol/clientCapabilities"].extensions`.
///
/// The value must be an object, as the extension schema requires.
pub(crate) fn client_declares_tasks(params: &Value) -> bool {
    params
        .get("_meta")
        .and_then(|meta| meta.get(CLIENT_CAPABILITIES_META))
        .and_then(|caps| caps.get("extensions"))
        .and_then(|extensions| extensions.get(EXTENSION_ID))
        .is_some_and(Value::is_object)
}

/// The `data` of a `-32021` error for this extension.
pub(crate) fn missing_capability_data() -> Value {
    json!({ "requiredCapabilities": { "extensions": { EXTENSION_ID: {} } } })
}

/// A digest of the caller's credentials. Only the same caller can read,
/// update or cancel a task.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Owner([u8; 32]);

impl Owner {
    /// Hash the credential parts. Each part has a length prefix, so two
    /// different part lists cannot give the same input.
    pub(crate) fn new(parts: &[Option<&str>]) -> Self {
        let mut hasher = Sha256::new();
        for part in parts {
            match part {
                Some(value) => {
                    hasher.update([1]);
                    hasher.update((value.len() as u64).to_be_bytes());
                    hasher.update(value.as_bytes());
                }
                None => hasher.update([0]),
            }
        }
        Self(hasher.finalize().into())
    }

    fn matches(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

/// How a task ended, or `Working`.
#[derive(Debug)]
pub(crate) enum TaskState {
    /// The tool is running.
    Working,
    /// The tool returned. Holds the `CallToolResult`.
    Completed(Value),
    /// A JSON-RPC error stopped the tool. Holds the error object and a
    /// status message.
    Failed(Value, String),
    /// The client cancelled the task.
    Cancelled,
}

impl TaskState {
    const fn status(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Completed(_) => "completed",
            Self::Failed(..) => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

struct Entry {
    owner: Owner,
    state: TaskState,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    abort: Option<AbortHandle>,
}

/// The in-memory task store.
pub(crate) struct TaskStore {
    config: McpTaskConfig,
    tasks: Mutex<HashMap<String, Entry>>,
}

/// Format a timestamp as ISO 8601 with milliseconds.
fn iso(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Clamp a duration to whole milliseconds for the wire.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl TaskStore {
    pub(crate) fn new(config: McpTaskConfig) -> Self {
        Self {
            config,
            tasks: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // A panic while the lock is held cannot leave an entry half-written:
        // each change is one field assignment. So a poisoned lock is safe.
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Remove the tasks whose TTL ended, and stop their work.
    fn purge(&self, tasks: &mut HashMap<String, Entry>, now: DateTime<Utc>) {
        let ttl = chrono::Duration::from_std(self.config.ttl).unwrap_or(chrono::Duration::MAX);
        tasks.retain(|_, entry| {
            let alive = entry
                .created_at
                .checked_add_signed(ttl)
                .is_none_or(|expires| now < expires);
            if !alive && let Some(abort) = entry.abort.take() {
                abort.abort();
            }
            alive
        });
    }

    /// The task fields on the wire, with the status payload.
    fn view(&self, id: &str, entry: &Entry, result_type: &str) -> Value {
        let mut out = json!({
            "resultType": result_type,
            "taskId": id,
            "status": entry.state.status(),
            "createdAt": iso(entry.created_at),
            "lastUpdatedAt": iso(entry.updated_at),
            "ttlMs": millis(self.config.ttl),
            "pollIntervalMs": millis(self.config.poll_interval),
        });
        match &entry.state {
            TaskState::Working => {}
            TaskState::Completed(result) => out["result"] = result.clone(),
            TaskState::Failed(error, message) => {
                out["error"] = error.clone();
                out["statusMessage"] = json!(message);
            }
            TaskState::Cancelled => out["statusMessage"] = json!("The client cancelled the task."),
        }
        out
    }

    /// Add a `working` task. Returns the `CreateTaskResult`, or `None` when
    /// the store is full.
    pub(crate) fn create(&self, id: &str, owner: Owner, now: DateTime<Utc>) -> Option<Value> {
        let mut tasks = self.lock();
        self.purge(&mut tasks, now);
        if tasks.len() >= self.config.max_tasks {
            return None;
        }
        let entry = Entry {
            owner,
            state: TaskState::Working,
            created_at: now,
            updated_at: now,
            abort: None,
        };
        let created = self.view(id, &entry, "task");
        tasks.insert(id.to_owned(), entry);
        Some(created)
    }

    /// Keep the handle that stops the task's work.
    pub(crate) fn set_abort(&self, id: &str, abort: AbortHandle) {
        let mut tasks = self.lock();
        match tasks.get_mut(id) {
            Some(entry) if matches!(entry.state, TaskState::Working) => entry.abort = Some(abort),
            // The task ended or was purged before the handle arrived.
            Some(_) => {}
            None => abort.abort(),
        }
    }

    /// Move a `working` task to a terminal state. A terminal task does not
    /// change.
    pub(crate) fn finish(&self, id: &str, state: TaskState, now: DateTime<Utc>) {
        let mut tasks = self.lock();
        if let Some(entry) = tasks.get_mut(id)
            && matches!(entry.state, TaskState::Working)
        {
            entry.state = state;
            entry.updated_at = now;
            entry.abort = None;
        }
    }

    /// The `tasks/get` result, or `None` when no task with this ID belongs to
    /// `owner`.
    pub(crate) fn get(&self, id: &str, owner: &Owner, now: DateTime<Utc>) -> Option<Value> {
        let mut tasks = self.lock();
        self.purge(&mut tasks, now);
        let entry = tasks.get(id).filter(|entry| entry.owner.matches(owner))?;
        Some(self.view(id, entry, "complete"))
    }

    /// Cancel a `working` task and stop its work. Returns `false` when no
    /// task with this ID belongs to `owner`.
    pub(crate) fn cancel(&self, id: &str, owner: &Owner, now: DateTime<Utc>) -> bool {
        let mut tasks = self.lock();
        self.purge(&mut tasks, now);
        let Some(entry) = tasks.get_mut(id).filter(|entry| entry.owner.matches(owner)) else {
            return false;
        };
        if matches!(entry.state, TaskState::Working) {
            entry.state = TaskState::Cancelled;
            entry.updated_at = now;
            if let Some(abort) = entry.abort.take() {
                abort.abort();
            }
        }
        true
    }

    /// Whether a task with this ID belongs to `owner`.
    pub(crate) fn contains(&self, id: &str, owner: &Owner, now: DateTime<Utc>) -> bool {
        let mut tasks = self.lock();
        self.purge(&mut tasks, now);
        tasks
            .get(id)
            .is_some_and(|entry| entry.owner.matches(owner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap()
    }

    fn owner(name: &str) -> Owner {
        Owner::new(&[Some(name)])
    }

    fn store() -> TaskStore {
        TaskStore::new(
            McpTaskConfig::default()
                .ttl(Duration::from_secs(10))
                .max_tasks(2),
        )
    }

    #[test]
    fn create_returns_a_working_task_handle() {
        let created = store().create("t1", owner("a"), at(0)).unwrap();
        assert_eq!(created["resultType"], "task");
        assert_eq!(created["status"], "working");
        assert_eq!(created["ttlMs"], 10_000);
        assert_eq!(created["pollIntervalMs"], 1_000);
        assert_eq!(created["createdAt"], "2023-11-14T22:13:20.000Z");
    }

    #[test]
    fn get_requires_the_same_owner() {
        let store = store();
        store.create("t1", owner("a"), at(0));
        assert!(store.get("t1", &owner("a"), at(1)).is_some());
        assert!(store.get("t1", &owner("b"), at(1)).is_none());
        assert!(!store.cancel("t1", &owner("b"), at(1)));
        assert!(!store.contains("t1", &owner("b"), at(1)));
    }

    #[test]
    fn owner_parts_are_length_prefixed() {
        let ab = Owner::new(&[Some("a"), Some("b")]);
        let a_b = Owner::new(&[Some("ab"), None]);
        assert!(!ab.matches(&a_b));
    }

    #[test]
    fn terminal_state_does_not_change() {
        let store = store();
        store.create("t1", owner("a"), at(0));
        assert!(store.cancel("t1", &owner("a"), at(1)));
        store.finish("t1", TaskState::Completed(json!({"isError": false})), at(2));
        let task = store.get("t1", &owner("a"), at(3)).unwrap();
        assert_eq!(task["status"], "cancelled");
        assert!(task.get("result").is_none());
        assert_eq!(task["lastUpdatedAt"], "2023-11-14T22:13:21.000Z");
    }

    #[test]
    fn completed_and_failed_tasks_carry_their_payload() {
        let store = store();
        store.create("ok", owner("a"), at(0));
        store.finish("ok", TaskState::Completed(json!({"isError": false})), at(1));
        let ok = store.get("ok", &owner("a"), at(1)).unwrap();
        assert_eq!(ok["resultType"], "complete");
        assert_eq!(ok["result"], json!({"isError": false}));

        store.create("bad", owner("a"), at(0));
        let error = json!({"code": -32603, "message": "Internal error"});
        store.finish(
            "bad",
            TaskState::Failed(error.clone(), "boom".into()),
            at(1),
        );
        let bad = store.get("bad", &owner("a"), at(1)).unwrap();
        assert_eq!(bad["status"], "failed");
        assert_eq!(bad["error"], error);
        assert_eq!(bad["statusMessage"], "boom");
    }

    #[test]
    fn expired_tasks_are_purged() {
        let store = store();
        store.create("t1", owner("a"), at(0));
        assert!(store.get("t1", &owner("a"), at(9)).is_some());
        assert!(store.get("t1", &owner("a"), at(10)).is_none());
    }

    #[test]
    fn full_store_refuses_until_a_task_expires() {
        let store = store();
        assert!(store.create("t1", owner("a"), at(0)).is_some());
        assert!(store.create("t2", owner("a"), at(1)).is_some());
        assert!(store.create("t3", owner("a"), at(2)).is_none());
        assert!(store.create("t3", owner("a"), at(10)).is_some());
    }

    #[test]
    fn client_capability_must_be_an_object() {
        let declared =
            json!({"_meta": {CLIENT_CAPABILITIES_META: {"extensions": {EXTENSION_ID: {}}}}});
        assert!(client_declares_tasks(&declared));
        let malformed =
            json!({"_meta": {CLIENT_CAPABILITIES_META: {"extensions": {EXTENSION_ID: true}}}});
        assert!(!client_declares_tasks(&malformed));
        assert!(!client_declares_tasks(&json!({})));
        assert!(!client_declares_tasks(&Value::Null));
    }

    #[tokio::test]
    async fn cancel_and_expiry_abort_the_work() {
        let store = store();
        store.create("t1", owner("a"), at(0));
        let job = tokio::spawn(std::future::pending::<()>());
        store.set_abort("t1", job.abort_handle());
        assert!(store.cancel("t1", &owner("a"), at(1)));
        assert!(job.await.unwrap_err().is_cancelled());

        store.create("t2", owner("a"), at(0));
        let job = tokio::spawn(std::future::pending::<()>());
        store.set_abort("t2", job.abort_handle());
        assert!(!store.contains("t2", &owner("a"), at(20)));
        assert!(job.await.unwrap_err().is_cancelled());
    }
}
