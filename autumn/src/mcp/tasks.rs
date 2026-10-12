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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use tokio::task::AbortHandle;

/// The extension identifier.
pub const EXTENSION_ID: &str = "io.modelcontextprotocol/tasks";

use super::modern::META_CLIENT_CAPABILITIES as CLIENT_CAPABILITIES_META;

/// JSON-RPC error code: the client did not declare a required capability.
pub const MISSING_CAPABILITY: i64 = -32021;

/// The largest integer a JSON number holds exactly (2^53 - 1). Wire values
/// above it lose precision in JavaScript clients.
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// The shortest TTL. A shorter TTL can delete a task before the client polls.
const MIN_TTL: Duration = Duration::from_secs(1);

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
///     .max_tasks(256)
///     .max_tasks_per_caller(16)
///     .max_stored_bytes(64 * 1024 * 1024);
/// # let _ = config;
/// ```
#[derive(Clone, Debug)]
pub struct McpTaskConfig {
    ttl: Duration,
    poll_interval: Duration,
    max_tasks: usize,
    max_tasks_per_caller: usize,
    max_stored_bytes: usize,
}

impl Default for McpTaskConfig {
    /// TTL 1 hour, poll interval 1 second, 1024 tasks, 64 tasks per client,
    /// 256 MiB of stored results.
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(3600),
            poll_interval: Duration::from_secs(1),
            max_tasks: 1024,
            max_tasks_per_caller: 64,
            max_stored_bytes: 256 * 1024 * 1024,
        }
    }
}

impl McpTaskConfig {
    /// Set the time from creation until the server deletes the task. When the
    /// TTL ends, the server stops a running task. The minimum is 1 second.
    #[must_use]
    pub const fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = if ttl.as_millis() < MIN_TTL.as_millis() {
            MIN_TTL
        } else {
            ttl
        };
        self
    }

    /// Set the poll interval that the server sends to the client.
    #[must_use]
    pub const fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Set the maximum number of tasks in the store, running or terminal. If
    /// the store is full, a task tool runs as a blocking call.
    #[must_use]
    pub const fn max_tasks(mut self, max: usize) -> Self {
        self.max_tasks = max;
        self
    }

    /// Set the maximum number of tasks for one client. The key is the client
    /// IP address that the trusted-proxy policy resolves, which the caller
    /// cannot change. If a client has this number of tasks, its next task
    /// tool call runs as a blocking call.
    #[must_use]
    pub const fn max_tasks_per_caller(mut self, max: usize) -> Self {
        self.max_tasks_per_caller = max;
        self
    }

    /// Set the maximum size of all stored results, in bytes of JSON. If a
    /// result does not fit, the task completes with a tool error.
    #[must_use]
    pub const fn max_stored_bytes(mut self, max: usize) -> Self {
        self.max_stored_bytes = max;
        self
    }
}

/// Whether the request `params` declare the tasks extension in
/// `_meta["io.modelcontextprotocol/clientCapabilities"].extensions`.
///
/// The value must be an object, as the extension schema requires.
pub fn client_declares_tasks(params: &Value) -> bool {
    params
        .get("_meta")
        .and_then(|meta| meta.get(CLIENT_CAPABILITIES_META))
        .and_then(|caps| caps.get("extensions"))
        .and_then(|extensions| extensions.get(EXTENSION_ID))
        .is_some_and(Value::is_object)
}

/// The `data` of a `-32021` error for this extension.
pub fn missing_capability_data() -> Value {
    json!({ "requiredCapabilities": { "extensions": { EXTENSION_ID: {} } } })
}

/// A digest of the caller's credentials. Only the same caller can read,
/// update or cancel a task.
#[derive(Clone, Copy, Debug)]
pub struct Owner([u8; 32]);

impl Owner {
    /// Hash the credential parts. Each part has a length prefix, so two
    /// different part lists cannot give the same input.
    pub fn new(parts: &[Option<&str>]) -> Self {
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
pub enum TaskState {
    /// The tool is running.
    Working,
    /// The tool returned. Holds the `CallToolResult`. An `Arc`, so that
    /// `tasks/get` copies it outside the lock.
    Completed(Arc<Value>),
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
    /// The quota key: the client IP address.
    quota: String,
    /// The size of a stored result, in bytes of JSON.
    bytes: usize,
    state: TaskState,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    abort: Option<AbortHandle>,
}

/// The in-memory task store.
pub struct TaskStore {
    config: McpTaskConfig,
    tasks: Mutex<HashMap<String, Entry>>,
    /// Limits how many task results buffer at the same time. Each permit is
    /// one full-size result, so the peak is near `max_stored_bytes`.
    buffers: tokio::sync::Semaphore,
}

/// Format a timestamp as ISO 8601 with milliseconds.
fn iso(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A duration in whole milliseconds for the wire, at most
/// [`MAX_SAFE_INTEGER`].
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).map_or(MAX_SAFE_INTEGER, |ms| ms.min(MAX_SAFE_INTEGER))
}

impl TaskStore {
    pub fn new(config: McpTaskConfig) -> Self {
        let permits = (config.max_stored_bytes / super::MAX_TOOL_RESPONSE_BYTES).max(1);
        Self {
            config,
            tasks: Mutex::new(HashMap::new()),
            buffers: tokio::sync::Semaphore::new(permits),
        }
    }

    /// Wait for a permit to buffer one task result. Keep it until the result
    /// is stored or discarded.
    pub async fn buffer_permit(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        // The semaphore is never closed, so this is always `Some`.
        self.buffers.acquire().await.ok()
    }

    /// The TTL. The job of a task stops when it ends.
    pub const fn ttl(&self) -> Duration {
        self.config.ttl
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // No code between the field assignments of a change can panic. So a
        // poisoned lock still holds consistent data.
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

    /// The task fields on the wire, with the status payload. A completed
    /// result is returned apart, so the caller can copy it after the lock is
    /// released. See [`with_result`].
    fn view(&self, id: &str, entry: &Entry, result_type: &str) -> (Value, Option<Arc<Value>>) {
        let mut out = json!({
            "resultType": result_type,
            "taskId": id,
            "status": entry.state.status(),
            "createdAt": iso(entry.created_at),
            "lastUpdatedAt": iso(entry.updated_at),
            "ttlMs": millis(self.config.ttl),
            "pollIntervalMs": millis(self.config.poll_interval),
        });
        let mut result = None;
        match &entry.state {
            TaskState::Working => {}
            TaskState::Completed(value) => result = Some(Arc::clone(value)),
            TaskState::Failed(error, message) => {
                out["error"] = error.clone();
                out["statusMessage"] = json!(message);
            }
            TaskState::Cancelled => out["statusMessage"] = json!("The client cancelled the task."),
        }
        (out, result)
    }

    /// Add a `working` task. Returns the `CreateTaskResult`. Returns `None`
    /// if the store is full or the `quota` key has its quota of tasks.
    pub fn create(&self, id: &str, owner: Owner, quota: &str, now: DateTime<Utc>) -> Option<Value> {
        let mut tasks = self.lock();
        self.purge(&mut tasks, now);
        if tasks.len() >= self.config.max_tasks {
            return None;
        }
        let held = tasks.values().filter(|e| e.quota == quota).count();
        if held >= self.config.max_tasks_per_caller {
            return None;
        }
        let entry = Entry {
            owner,
            quota: quota.to_owned(),
            bytes: 0,
            state: TaskState::Working,
            created_at: now,
            updated_at: now,
            abort: None,
        };
        let (created, _) = self.view(id, &entry, "task");
        tasks.insert(id.to_owned(), entry);
        drop(tasks);
        Some(created)
    }

    /// Keep the handle that stops the task's work.
    pub fn set_abort(&self, id: &str, abort: &AbortHandle) {
        let mut tasks = self.lock();
        let stop = match tasks.get_mut(id) {
            // No entry: the TTL ended before the handle came.
            None => true,
            Some(entry) => match entry.state {
                TaskState::Working => {
                    entry.abort = Some(abort.clone());
                    false
                }
                // The client cancelled before the handle came.
                TaskState::Cancelled => true,
                // The task ended. Its job can still write the audit outcome.
                TaskState::Completed(_) | TaskState::Failed(..) => false,
            },
        };
        drop(tasks);
        if stop {
            abort.abort();
        }
    }

    /// Move a `working` task to a terminal state. A terminal task does not
    /// change. A result that does not fit in `max_stored_bytes` becomes a
    /// tool error.
    pub fn finish(&self, id: &str, state: TaskState, now: DateTime<Utc>) {
        // Measure outside the lock: a result can be 10 MiB.
        let bytes = match &state {
            TaskState::Completed(result) => serde_json::to_vec(&**result).map_or(0, |v| v.len()),
            _ => 0,
        };
        let mut tasks = self.lock();
        let stored: usize = tasks.values().map(|e| e.bytes).sum();
        let fits = stored.saturating_add(bytes) <= self.config.max_stored_bytes;
        if let Some(entry) = tasks.get_mut(id)
            && matches!(entry.state, TaskState::Working)
        {
            if fits {
                entry.state = state;
                entry.bytes = bytes;
            } else {
                entry.state = TaskState::Completed(Arc::new(over_budget_result()));
            }
            entry.updated_at = now;
            entry.abort = None;
        }
        drop(tasks);
        if !fits {
            tracing::warn!(
                bytes,
                "MCP task result exceeds the task store byte budget; \
                 the task completes with a tool error"
            );
        }
    }

    /// The `tasks/get` result, or `None` when no task with this ID belongs to
    /// `owner`.
    pub fn get(&self, id: &str, owner: &Owner, now: DateTime<Utc>) -> Option<Value> {
        let mut tasks = self.lock();
        self.purge(&mut tasks, now);
        let view = tasks
            .get(id)
            .filter(|entry| entry.owner.matches(owner))
            .map(|entry| self.view(id, entry, "complete"));
        drop(tasks);
        view.map(|(out, result)| with_result(out, result.as_deref()))
    }

    /// Cancel a `working` task and stop its work. Returns `false` when no
    /// task with this ID belongs to `owner`.
    pub fn cancel(&self, id: &str, owner: &Owner, now: DateTime<Utc>) -> bool {
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
        drop(tasks);
        true
    }

    /// Delete the tasks whose TTL ended, and stop their work. A timer calls
    /// this at the end of each TTL, so an idle server also frees memory.
    pub fn sweep(&self, now: DateTime<Utc>) {
        let mut tasks = self.lock();
        self.purge(&mut tasks, now);
    }

    /// Whether a task with this ID belongs to `owner`.
    pub fn contains(&self, id: &str, owner: &Owner, now: DateTime<Utc>) -> bool {
        let mut tasks = self.lock();
        self.purge(&mut tasks, now);
        tasks
            .get(id)
            .is_some_and(|entry| entry.owner.matches(owner))
    }
}

/// The `CallToolResult` for a result that does not fit in the byte budget.
fn over_budget_result() -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": "The tool result is too large for the MCP task store. \
                     Call the tool without a task, or ask the operator to \
                     increase `max_stored_bytes`.",
        }],
        "isError": true,
        "resultType": "complete",
    })
}

/// Add a completed result to a task view.
fn with_result(mut out: Value, result: Option<&Value>) -> Value {
    if let Some(result) = result {
        out["result"] = result.clone();
    }
    out
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
        let created = store().create("t1", owner("a"), "q", at(0)).unwrap();
        assert_eq!(created["resultType"], "task");
        assert_eq!(created["status"], "working");
        assert_eq!(created["ttlMs"], 10_000);
        assert_eq!(created["pollIntervalMs"], 1_000);
        assert_eq!(created["createdAt"], "2023-11-14T22:13:20.000Z");
    }

    #[test]
    fn get_requires_the_same_owner() {
        let store = store();
        store.create("t1", owner("a"), "q", at(0));
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
        store.create("t1", owner("a"), "q", at(0));
        assert!(store.cancel("t1", &owner("a"), at(1)));
        store.finish(
            "t1",
            TaskState::Completed(Arc::new(json!({"isError": false}))),
            at(2),
        );
        let task = store.get("t1", &owner("a"), at(3)).unwrap();
        assert_eq!(task["status"], "cancelled");
        assert!(task.get("result").is_none());
        assert_eq!(task["lastUpdatedAt"], "2023-11-14T22:13:21.000Z");
    }

    #[test]
    fn completed_and_failed_tasks_carry_their_payload() {
        let store = store();
        store.create("ok", owner("a"), "q", at(0));
        store.finish(
            "ok",
            TaskState::Completed(Arc::new(json!({"isError": false}))),
            at(1),
        );
        let ok = store.get("ok", &owner("a"), at(1)).unwrap();
        assert_eq!(ok["resultType"], "complete");
        assert_eq!(ok["result"], json!({"isError": false}));

        store.create("bad", owner("a"), "q", at(0));
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
        store.create("t1", owner("a"), "q", at(0));
        assert!(store.get("t1", &owner("a"), at(9)).is_some());
        assert!(store.get("t1", &owner("a"), at(10)).is_none());
    }

    #[test]
    fn full_store_refuses_until_a_task_expires() {
        let store = store();
        assert!(store.create("t1", owner("a"), "q", at(0)).is_some());
        assert!(store.create("t2", owner("a"), "q", at(1)).is_some());
        assert!(store.create("t3", owner("a"), "q", at(2)).is_none());
        assert!(store.create("t3", owner("a"), "q", at(10)).is_some());
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

    #[test]
    fn one_client_cannot_fill_the_store() {
        let store = TaskStore::new(
            McpTaskConfig::default()
                .max_tasks(10)
                .max_tasks_per_caller(1),
        );
        assert!(store.create("a1", owner("a"), "10.0.0.1", at(0)).is_some());
        // A new credential from the same client does not get a new quota.
        assert!(store.create("a2", owner("b"), "10.0.0.1", at(0)).is_none());
        assert!(store.create("c1", owner("a"), "10.0.0.2", at(0)).is_some());
    }

    #[test]
    fn result_over_the_byte_budget_becomes_a_tool_error() {
        let store = TaskStore::new(McpTaskConfig::default().max_stored_bytes(64));
        store.create("big", owner("a"), "q", at(0));
        let big = json!({"content": [{"type": "text", "text": "x".repeat(100)}], "isError": false});
        store.finish("big", TaskState::Completed(Arc::new(big)), at(1));
        let task = store.get("big", &owner("a"), at(1)).unwrap();
        assert_eq!(task["status"], "completed");
        assert_eq!(task["result"]["isError"], true, "{task}");

        store.create("small", owner("a"), "q", at(0));
        let small = json!({"isError": false});
        store.finish(
            "small",
            TaskState::Completed(Arc::new(small.clone())),
            at(1),
        );
        assert_eq!(
            store.get("small", &owner("a"), at(1)).unwrap()["result"],
            small
        );
    }

    #[test]
    fn ttl_has_a_floor_and_wire_values_are_safe_integers() {
        let config = McpTaskConfig::default()
            .ttl(Duration::ZERO)
            .poll_interval(Duration::MAX);
        let created = TaskStore::new(config)
            .create("t1", owner("a"), "q", at(0))
            .unwrap();
        assert_eq!(created["ttlMs"], 1_000);
        assert_eq!(created["pollIntervalMs"], MAX_SAFE_INTEGER);
        let long = TaskStore::new(McpTaskConfig::default().ttl(Duration::MAX))
            .create("t1", owner("a"), "q", at(0))
            .unwrap();
        assert_eq!(long["ttlMs"], MAX_SAFE_INTEGER);
    }

    #[test]
    fn buffer_permits_follow_the_byte_budget() {
        let unit = super::super::MAX_TOOL_RESPONSE_BYTES;
        let store = TaskStore::new(McpTaskConfig::default().max_stored_bytes(2 * unit));
        let first = store.buffers.try_acquire().unwrap();
        let second = store.buffers.try_acquire().unwrap();
        assert!(store.buffers.try_acquire().is_err());
        drop((first, second));
        // A budget below one result still lets one task buffer.
        let small = TaskStore::new(McpTaskConfig::default().max_stored_bytes(1));
        assert!(small.buffers.try_acquire().is_ok());
    }

    #[test]
    fn sweep_deletes_expired_tasks() {
        let store = store();
        store.create("t1", owner("a"), "q", at(0));
        store.finish("t1", TaskState::Completed(Arc::new(json!({}))), at(1));
        store.sweep(at(9));
        assert_eq!(store.lock().len(), 1);
        store.sweep(at(10));
        assert_eq!(store.lock().len(), 0);
    }

    #[tokio::test]
    async fn abort_handle_after_cancel_stops_the_work() {
        let store = store();
        store.create("t1", owner("a"), "q", at(0));
        assert!(store.cancel("t1", &owner("a"), at(1)));
        let job = tokio::spawn(std::future::pending::<()>());
        store.set_abort("t1", &job.abort_handle());
        assert!(job.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn cancel_and_expiry_abort_the_work() {
        let store = store();
        store.create("t1", owner("a"), "q", at(0));
        let job = tokio::spawn(std::future::pending::<()>());
        store.set_abort("t1", &job.abort_handle());
        assert!(store.cancel("t1", &owner("a"), at(1)));
        assert!(job.await.unwrap_err().is_cancelled());

        store.create("t2", owner("a"), "q", at(0));
        let job = tokio::spawn(std::future::pending::<()>());
        store.set_abort("t2", &job.abort_handle());
        assert!(!store.contains("t2", &owner("a"), at(20)));
        assert!(job.await.unwrap_err().is_cancelled());
    }
}
