//! Role-aware background tasks for plugins and apps (#3545).
//!
//! A plugin declares a long-running loop. The framework places it:
//!
//! ```rust,no_run
//! use autumn_web::app::AppBuilder;
//! use autumn_web::background::PluginCapability;
//! use autumn_web::config::Capability;
//!
//! /// The capability of the sync loop. Its default is the `job_workers` value
//! /// of the role.
//! const SYNC_WORKER: PluginCapability = PluginCapability::new("sync.worker", Capability::JobWorkers);
//!
//! fn wire(app: AppBuilder) -> AppBuilder {
//!     app.background_task("sync.poller", SYNC_WORKER, |state, shutdown| async move {
//!         loop {
//!             tokio::select! {
//!                 () = shutdown.cancelled() => return Ok(()),
//!                 () = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
//!                     // One poll, with `state`.
//!                     let _ = &state;
//!                 }
//!             }
//!         }
//!     })
//! }
//! ```
//!
//! # Rules
//!
//! - The framework starts a task only on a role that runs its capability. A
//!   task on [`Capability::Maintenance`] runs on `combined` and `worker`, and
//!   not on `web`.
//! - A [`PluginCapability`] has a built-in parent. A role runs it when it runs
//!   the parent. `[roles.<name>] capabilities = { "sync.worker" = false }`
//!   changes that for one named role. A role cannot turn on a plugin
//!   capability that its base does not run.
//! - A task with [`queues`](BackgroundTask::queues) runs only on a role that
//!   runs job workers and drains one of the queues. The role `pin` (or
//!   `jobs.pin`) applies, as for `#[job]` queues. `autumn jobs manifest`
//!   lists the queues, so the `autumn doctor` coverage check includes them.
//! - Each task gets a child of the server shutdown token. The graceful drain
//!   waits for the tasks for at most half of the remaining
//!   `server.shutdown_timeout_secs`. Then the framework aborts the tasks that
//!   did not stop.
//! - [`explicit`](BackgroundTask::explicit) is the switch of the plugin
//!   config. `Some(true)` runs the task on every role. The framework logs a
//!   warning on a role that does not place it. `Some(false)` never runs it.
//! - The framework does not restart a task that returns or panics.
//!   `/actuator/tasks` shows its status.
//!
//! `/actuator/tasks` (`background_tasks`) and `autumn graph`
//! (`background_task` nodes) list each task and the roles that run it.
//!
//! See `docs/guide/extensibility.md` and `docs/guide/jobs.md`.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::config::{AutumnConfig, Capability, ProcessRole, Role};
use crate::state::AppState;

/// A capability that a plugin defines (#3545).
///
/// It has a name, `<plugin>.<name>`, and a built-in parent. A role runs it
/// when the role runs the parent, unless `[roles.<name>] capabilities` sets
/// it. Declare it as a `const` and use the same value everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PluginCapability {
    name: &'static str,
    parent: Capability,
}

impl PluginCapability {
    /// A plugin capability named `name` with the default of `parent`.
    ///
    /// Registration checks the name. The name is `<plugin>.<name>`. It starts
    /// with a lowercase letter. It has only `a-z`, `0-9`, `_`, `-` and `.`,
    /// and at most 63 characters.
    #[must_use]
    pub const fn new(name: &'static str, parent: Capability) -> Self {
        Self { name, parent }
    }

    /// The name, for example `"harvest.worker"`.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The built-in capability that gives the default.
    #[must_use]
    pub const fn parent(&self) -> Capability {
        self.parent
    }
}

/// The capability that a background task needs: built-in or plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum BackgroundCapability {
    /// A built-in capability, for example [`Capability::Maintenance`].
    Builtin(Capability),
    /// A capability that a plugin defines.
    Plugin(PluginCapability),
}

impl BackgroundCapability {
    /// The name: the built-in `snake_case` name, or the plugin name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Builtin(capability) => capability.as_str(),
            Self::Plugin(capability) => capability.name(),
        }
    }

    /// The built-in capability that decides the default. A built-in
    /// capability is its own parent.
    #[must_use]
    pub const fn parent(&self) -> Capability {
        match self {
            Self::Builtin(capability) => *capability,
            Self::Plugin(capability) => capability.parent(),
        }
    }
}

impl From<Capability> for BackgroundCapability {
    fn from(capability: Capability) -> Self {
        Self::Builtin(capability)
    }
}

impl From<PluginCapability> for BackgroundCapability {
    fn from(capability: PluginCapability) -> Self {
        Self::Plugin(capability)
    }
}

type TaskFuture = Pin<Box<dyn Future<Output = crate::AutumnResult<()>> + Send>>;
type TaskFn = Arc<dyn Fn(AppState, CancellationToken) -> TaskFuture + Send + Sync>;

/// One background task: a name, a capability and an async loop.
///
/// Register it with [`AppBuilder::background`](crate::app::AppBuilder::background),
/// or use the short form
/// [`AppBuilder::background_task`](crate::app::AppBuilder::background_task).
#[derive(Clone)]
pub struct BackgroundTask {
    name: Cow<'static, str>,
    capability: BackgroundCapability,
    queues: Vec<String>,
    explicit: Option<bool>,
    run: TaskFn,
    location: &'static Location<'static>,
    registered_by: Option<String>,
}

impl std::fmt::Debug for BackgroundTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundTask")
            .field("name", &self.name)
            .field("capability", &self.capability)
            .field("queues", &self.queues)
            .field("explicit", &self.explicit)
            .field("location", &self.location)
            .field("registered_by", &self.registered_by)
            .finish_non_exhaustive()
    }
}

impl BackgroundTask {
    /// A task named `name` that needs `capability`.
    ///
    /// `run` gets the app state and a shutdown token. When the token is
    /// cancelled, `run` must return. The drain aborts it at the end of its
    /// budget. `autumn graph` shows the source location of this call.
    #[track_caller]
    #[must_use]
    pub fn new<F, Fut>(
        name: impl Into<Cow<'static, str>>,
        capability: impl Into<BackgroundCapability>,
        run: F,
    ) -> Self
    where
        F: Fn(AppState, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::AutumnResult<()>> + Send + 'static,
    {
        Self {
            name: name.into(),
            capability: capability.into(),
            queues: Vec::new(),
            explicit: None,
            run: Arc::new(move |state, shutdown| Box::pin(run(state, shutdown))),
            location: Location::caller(),
            registered_by: None,
        }
    }

    /// The job queues this task drains. The task runs only on a role that
    /// drains one of them.
    #[must_use]
    pub fn queues<I, S>(mut self, queues: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.queues.clear();
        for queue in queues {
            let queue = crate::job::normalize_queue_name(&queue.into());
            if !self.queues.contains(&queue) {
                self.queues.push(queue);
            }
        }
        self
    }

    /// The explicit switch of the plugin config. `None` (the default) lets
    /// the role decide. `Some(true)` runs the task on every role, with a
    /// startup warning where the role does not run its capability.
    /// `Some(false)` never runs it.
    #[must_use]
    pub const fn explicit(mut self, enabled: Option<bool>) -> Self {
        self.explicit = enabled;
        self
    }

    /// Record the plugin whose `build()` registers this task.
    pub(crate) fn registered_by_plugin(mut self, plugin: Option<String>) -> Self {
        if self.registered_by.is_none() {
            self.registered_by = plugin;
        }
        self
    }

    /// The task name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The capability this task needs.
    #[must_use]
    pub const fn capability(&self) -> BackgroundCapability {
        self.capability
    }

    /// The queues this task drains.
    #[must_use]
    pub fn queue_names(&self) -> &[String] {
        &self.queues
    }

    /// The explicit switch, if set.
    #[must_use]
    pub const fn explicit_switch(&self) -> Option<bool> {
        self.explicit
    }

    /// The plugin that registered this task, or `None` for the app.
    #[must_use]
    pub fn registered_by(&self) -> Option<&str> {
        self.registered_by.as_deref()
    }

    /// `file:line` of the registration.
    #[must_use]
    pub fn location(&self) -> String {
        format!("{}:{}", self.location.file(), self.location.line())
    }
}

/// Whether a role starts a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placement {
    /// The role runs the capability and drains a task queue.
    Run,
    /// `explicit = Some(true)` starts the task on a role that does not run it.
    Forced,
    /// The role does not start the task.
    Skip(Skip),
}

impl Placement {
    pub(crate) const fn starts(self) -> bool {
        matches!(self, Self::Run | Self::Forced)
    }
}

/// Why a role does not start a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    /// The role does not run the capability.
    Capability,
    /// The role drains none of the task queues.
    Queues,
    /// `explicit = Some(false)`.
    Disabled,
}

/// Whether `pin` admits one of `queues`. An empty `pin` admits every queue.
fn pin_admits_one(queues: &[String], pin: &[String]) -> bool {
    pin.is_empty()
        || pin
            .iter()
            .any(|p| queues.contains(&crate::job::normalize_queue_name(p)))
}

/// Whether `role` drains one of `queues`: it runs job workers, and `pin`
/// admits one of them. A task with no queues does not depend on this.
fn drains_one(queues: &[String], role: &Role, pin: &[String]) -> bool {
    queues.is_empty() || (role.capabilities().job_workers && pin_admits_one(queues, pin))
}

/// Where `task` runs on `role`, whose effective pin is `pin`.
///
/// `verification/background_tasks.rs` models this function.
pub(crate) fn place(task: &BackgroundTask, role: &Role, pin: &[String]) -> Placement {
    let placed = role.runs_background(task.capability);
    let drains = drains_one(&task.queues, role, pin);
    match task.explicit {
        Some(false) => Placement::Skip(Skip::Disabled),
        Some(true) if placed && drains => Placement::Run,
        Some(true) => Placement::Forced,
        None if !placed => Placement::Skip(Skip::Capability),
        None if !drains => Placement::Skip(Skip::Queues),
        None => Placement::Run,
    }
}

/// The roles of `config` that start `task`: `combined`, `web`, `worker`, then
/// each valid `[roles.<name>]` preset in name order.
pub(crate) fn roles_running(task: &BackgroundTask, config: &AutumnConfig) -> Vec<String> {
    let built_in = [ProcessRole::Combined, ProcessRole::Web, ProcessRole::Worker]
        .into_iter()
        .map(Role::from);
    // `resolve_role` validates the preset. An invalid one starts nothing.
    let presets = config.roles.keys().filter_map(|name| {
        let name: crate::config::RoleName = name.parse().ok()?;
        crate::config::resolve_role(&name, &config.roles).ok()
    });
    built_in
        .chain(presets)
        .filter(|role| {
            let jobs = role.jobs_config(&config.jobs);
            place(task, role, &jobs.pin).starts()
        })
        .map(|role| role.name().to_owned())
        .collect()
}

/// A `[roles.<name>] capabilities` value that the registered plugins do not
/// allow.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum BackgroundError {
    /// No plugin declares the capability.
    #[error(
        "[roles.{role}] capabilities: no plugin declares {capability:?}; {}",
        declared_text(.declared)
    )]
    UnknownCapability {
        /// The role name.
        role: String,
        /// The key.
        capability: String,
        /// The declared plugin capabilities.
        declared: Vec<String>,
    },
    /// The value is `true`, but the base does not run the parent.
    #[error(
        "[roles.{role}] capabilities: \"{capability}\" = true is above its base. The base \
         \"{}\" does not run {parent}.",
        .base.as_str()
    )]
    AboveBase {
        /// The role name.
        role: String,
        /// The key.
        capability: String,
        /// The parent capability.
        parent: Capability,
        /// The base of the role.
        base: ProcessRole,
    },
}

fn declared_text(declared: &[String]) -> String {
    if declared.is_empty() {
        "no plugin capabilities are declared".to_owned()
    } else {
        format!("declared plugin capabilities: {}", declared.join(", "))
    }
}

/// The background tasks and plugin capabilities of one app.
#[derive(Default, Clone)]
pub(crate) struct Registry {
    tasks: Vec<BackgroundTask>,
    capabilities: BTreeMap<&'static str, Capability>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("tasks", &self.tasks)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

impl Registry {
    /// Declare `capability`.
    ///
    /// # Panics
    ///
    /// On an invalid name, or a name that is declared with another parent.
    pub(crate) fn add_capability(&mut self, capability: PluginCapability) {
        let name = capability.name();
        assert!(
            crate::config::is_valid_capability_name(name),
            "invalid plugin capability name {name:?}: use `<plugin>.<name>`, with only a-z, \
             0-9, '_', '-' and '.'"
        );
        if let Some(parent) = self.capabilities.get(name) {
            assert!(
                *parent == capability.parent(),
                "plugin capability {name:?} is declared twice, with parents {parent} and {}",
                capability.parent()
            );
            return;
        }
        self.capabilities.insert(name, capability.parent());
    }

    /// Register `task`. A plugin capability of the task is declared too.
    ///
    /// # Panics
    ///
    /// On an invalid task name, a duplicate task name, or a capability that
    /// [`add_capability`](Self::add_capability) refuses.
    pub(crate) fn add_task(&mut self, task: BackgroundTask) {
        let name = task.name();
        assert!(
            is_valid_task_name(name),
            "invalid background task name {name:?}: it starts with a lowercase letter and has \
             only a-z, 0-9, '_', '-' and '.', with at most 63 characters"
        );
        assert!(
            !self.tasks.iter().any(|t| t.name() == name),
            "background task {name:?} is registered twice"
        );
        if let BackgroundCapability::Plugin(capability) = task.capability {
            self.add_capability(capability);
        }
        self.tasks.push(task);
    }

    /// Move every task and capability of `other` into this registry.
    pub(crate) fn extend(&mut self, other: Self) {
        for (name, parent) in other.capabilities {
            self.add_capability(PluginCapability::new(name, parent));
        }
        for task in other.tasks {
            self.add_task(task);
        }
    }

    pub(crate) fn tasks(&self) -> &[BackgroundTask] {
        &self.tasks
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tasks.is_empty() && self.capabilities.is_empty()
    }

    /// Every queue of a task that is not switched off, normalized, in
    /// first-seen order.
    pub(crate) fn queues(&self) -> Vec<String> {
        let mut queues: Vec<String> = Vec::new();
        for queue in self
            .tasks
            .iter()
            .filter(|task| task.explicit != Some(false))
            .flat_map(|task| task.queues.iter())
        {
            if !queues.contains(queue) {
                queues.push(queue.clone());
            }
        }
        queues
    }

    /// Check the `capabilities` table of every declared preset against the
    /// declared plugin capabilities.
    ///
    /// An unknown key on the preset named `active` is an error. On another
    /// preset it is a warning: that binary may not link the plugin. A value
    /// above the base is an error on every preset.
    ///
    /// Returns the warnings.
    ///
    /// # Errors
    ///
    /// The first [`BackgroundError`], in preset name order.
    pub(crate) fn validate_roles(
        &self,
        config: &AutumnConfig,
        active: &str,
    ) -> Result<Vec<BackgroundError>, BackgroundError> {
        let mut warnings = Vec::new();
        for (role, preset) in &config.roles {
            for (capability, &value) in &preset.capabilities {
                let Some(&parent) = self.capabilities.get(capability.as_str()) else {
                    let error = BackgroundError::UnknownCapability {
                        role: role.clone(),
                        capability: capability.clone(),
                        declared: self.capabilities.keys().map(|&n| n.to_owned()).collect(),
                    };
                    if role == active {
                        return Err(error);
                    }
                    warnings.push(error);
                    continue;
                };
                if value && !preset.base.runs(parent) {
                    return Err(BackgroundError::AboveBase {
                        role: role.clone(),
                        capability: capability.clone(),
                        parent,
                        base: preset.base,
                    });
                }
            }
        }
        Ok(warnings)
    }
}

/// The grammar of a task name: as a role name, plus `.`.
fn is_valid_task_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 63
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-' | '.'))
}

/// The state of one background task on this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BackgroundTaskState {
    /// The task runs.
    Running,
    /// The role does not run the capability, or drains none of the queues.
    NotPlaced,
    /// `explicit = Some(false)`.
    Disabled,
    /// The task returned after the shutdown token was cancelled.
    Stopped,
    /// The task returned `Ok` before shutdown.
    Exited,
    /// The task returned an error.
    Failed,
    /// The task panicked.
    Panicked,
    /// The drain aborted the task at the end of its budget.
    Aborted,
}

/// One entry of `background_tasks` in `/actuator/tasks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct BackgroundTaskStatus {
    /// The capability name.
    pub capability: String,
    /// The built-in parent of a plugin capability.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The queues the task drains.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub queues: Vec<String>,
    /// The explicit switch, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explicit: Option<bool>,
    /// The plugin that registered the task. `None` for the app.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registered_by: Option<String>,
    /// `file:line` of the registration.
    pub location: String,
    /// The roles of this config that start the task.
    pub roles: Vec<String>,
    /// Whether this process starts the task.
    pub runs_here: bool,
    /// The state on this process.
    pub status: BackgroundTaskState,
    /// The error or panic message of the last run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// The status of `task` before it starts.
pub(crate) fn initial_status(
    task: &BackgroundTask,
    placement: Placement,
    roles: Vec<String>,
) -> BackgroundTaskStatus {
    let capability = task.capability;
    BackgroundTaskStatus {
        capability: capability.name().to_owned(),
        parent: match capability {
            BackgroundCapability::Plugin(plugin) => Some(plugin.parent().as_str().to_owned()),
            BackgroundCapability::Builtin(_) => None,
        },
        queues: task.queues.clone(),
        explicit: task.explicit,
        registered_by: task.registered_by.clone(),
        location: task.location(),
        roles,
        runs_here: placement.starts(),
        status: match placement {
            Placement::Run | Placement::Forced => BackgroundTaskState::Running,
            Placement::Skip(Skip::Disabled) => BackgroundTaskState::Disabled,
            Placement::Skip(_) => BackgroundTaskState::NotPlaced,
        },
        last_error: None,
    }
}

/// The started tasks of one process.
#[derive(Debug, Default)]
pub(crate) struct BackgroundRuntime {
    tasks: Vec<(String, tokio::task::JoinHandle<()>)>,
    /// Records `aborted` for a task that the drain aborts.
    registry: Option<crate::actuator::TaskRegistry>,
}

/// How long the drain waits for an aborted task to stop.
const ABORT_WAIT: Duration = Duration::from_secs(1);

/// The part of the remaining shutdown budget that the background drain can
/// use: half. The fleet close, the replication flush and the `on_shutdown`
/// hooks keep the rest.
pub(crate) fn drain_budget(remaining: Duration) -> Duration {
    remaining / 2
}

/// Start each task of `registry` that the role of `state` places.
///
/// Each task gets a child of `shutdown`. `config` gives the effective pin and
/// the declared presets for the `roles` list.
pub(crate) fn start(
    registry: &Registry,
    state: &AppState,
    shutdown: &CancellationToken,
    config: &AutumnConfig,
) -> BackgroundRuntime {
    let role = state.role();
    let jobs = role.jobs_config(&config.jobs);
    let mut runtime = BackgroundRuntime {
        tasks: Vec::new(),
        registry: Some(state.task_registry.clone()),
    };
    for task in registry.tasks() {
        let placement = place(task, role, &jobs.pin);
        let roles = roles_running(task, config);
        if roles.is_empty() && task.explicit != Some(false) {
            tracing::warn!(
                task = task.name(),
                capability = task.capability.name(),
                queues = ?task.queues,
                "background task runs on no role of this config"
            );
        }
        let status = initial_status(task, placement, roles);
        state.task_registry.register_background(task.name(), status);
        match placement {
            Placement::Skip(Skip::Disabled) => {
                tracing::info!(
                    task = task.name(),
                    role = role.name(),
                    "background task not started: its explicit switch is off"
                );
                continue;
            }
            Placement::Skip(skip) => {
                tracing::debug!(
                    task = task.name(),
                    capability = task.capability.name(),
                    role = role.name(),
                    reason = ?skip,
                    "background task not started on this role"
                );
                continue;
            }
            Placement::Forced => tracing::warn!(
                task = task.name(),
                capability = task.capability.name(),
                role = role.name(),
                "background task starts because its explicit switch is on, but role {} does \
                 not run {} or drain its queues",
                role.name(),
                task.capability.name()
            ),
            Placement::Run => tracing::info!(
                task = task.name(),
                capability = task.capability.name(),
                "background task started"
            ),
        }
        let name = task.name().to_owned();
        let token = shutdown.child_token();
        let run = Arc::clone(&task.run);
        let task_state = state.clone();
        let registry = state.task_registry.clone();
        let task_name = name.clone();
        let handle = crate::sim::spawn_app_task(state, async move {
            // Call `run` inside the guard: a panic before the first `.await`
            // is caught too.
            let outcome =
                std::panic::AssertUnwindSafe(async { run(task_state, token.clone()).await })
                    .catch_unwind()
                    .await;
            let (end, error) = match outcome {
                Ok(Ok(())) if token.is_cancelled() => (BackgroundTaskState::Stopped, None),
                Ok(Ok(())) => {
                    tracing::warn!(task = %task_name, "background task returned before shutdown");
                    (BackgroundTaskState::Exited, None)
                }
                Ok(Err(error)) => {
                    tracing::error!(task = %task_name, error = %error, "background task failed");
                    (BackgroundTaskState::Failed, Some(error.to_string()))
                }
                Err(panic) => {
                    let message = panic_message(panic.as_ref());
                    tracing::error!(task = %task_name, panic = %message, "background task panicked");
                    (BackgroundTaskState::Panicked, Some(message))
                }
            };
            registry.record_background_end(&task_name, end, error);
        });
        runtime.tasks.push((name, handle));
    }
    runtime
}

/// The text of a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_owned())
}

impl BackgroundRuntime {
    /// The number of started tasks.
    pub(crate) const fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Wait for every task to return, up to `budget`. Abort the tasks that
    /// are still running after it, and wait up to one second for each to
    /// stop. The caller cancels the shutdown token before this call.
    ///
    /// Returns the number of aborted tasks.
    pub(crate) async fn drain(self, budget: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + budget;
        let mut aborted = 0;
        for (name, mut handle) in self.tasks {
            if tokio::time::timeout_at(deadline, &mut handle)
                .await
                .is_err()
            {
                // `abort` only asks. Wait for the task to stop, so the later
                // phases do not run beside it. A task that never yields does
                // not stop: the wait has a limit.
                handle.abort();
                if tokio::time::timeout(ABORT_WAIT, handle).await.is_err() {
                    tracing::error!(
                        task = %name,
                        "shutdown: background task did not stop after abort; it does not yield"
                    );
                }
                aborted += 1;
                if let Some(registry) = &self.registry {
                    registry.record_background_end(
                        &name,
                        BackgroundTaskState::Aborted,
                        Some("the task did not stop within the shutdown budget".to_owned()),
                    );
                }
                tracing::warn!(
                    task = %name,
                    "shutdown: background task did not stop within the budget; aborted"
                );
            }
        }
        aborted
    }
}

/// One graph entry for each task of `registry` (#3545).
#[cfg(feature = "graph")]
pub(crate) fn graph_entries(
    registry: &Registry,
    config: &AutumnConfig,
) -> Vec<crate::graph::manifest::BackgroundTaskEntry> {
    registry
        .tasks()
        .iter()
        .map(|task| {
            let status = initial_status(task, Placement::Run, roles_running(task, config));
            crate::graph::manifest::BackgroundTaskEntry {
                name: task.name().to_owned(),
                module: task
                    .registered_by
                    .clone()
                    .unwrap_or_else(|| "app".to_owned()),
                location: status.location,
                facts: crate::graph::manifest::BackgroundFacts {
                    capability: status.capability,
                    parent: status.parent,
                    queues: status.queues,
                    explicit: status.explicit,
                    roles: status.roles,
                },
            }
        })
        .collect()
}

/// The task queues, for the job runtime pin check (#3545).
#[derive(Debug, Clone, Default)]
pub(crate) struct BackgroundQueues(pub(crate) Vec<String>);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RolePreset;

    const SYNC: PluginCapability = PluginCapability::new("sync.worker", Capability::JobWorkers);

    fn task(capability: impl Into<BackgroundCapability>) -> BackgroundTask {
        BackgroundTask::new("test.task", capability, |_state, _shutdown| async {
            Ok(())
        })
    }

    fn preset_role(toml_text: &str) -> Role {
        let mut config: AutumnConfig = toml::from_str(toml_text).expect("config parses");
        config.role = "tier".parse().expect("role name");
        config.resolve_role().expect("role resolves")
    }

    fn built_in(role: ProcessRole) -> Role {
        Role::from(role)
    }

    #[test]
    fn built_in_capability_runs_on_combined_and_worker_only() {
        for capability in [
            Capability::Maintenance,
            Capability::Scheduler,
            Capability::JobWorkers,
        ] {
            let task = task(capability);
            assert_eq!(
                place(&task, &built_in(ProcessRole::Combined), &[]),
                Placement::Run
            );
            assert_eq!(
                place(&task, &built_in(ProcessRole::Worker), &[]),
                Placement::Run
            );
            assert_eq!(
                place(&task, &built_in(ProcessRole::Web), &[]),
                Placement::Skip(Skip::Capability)
            );
        }
        let http = task(Capability::ServesHttp);
        assert_eq!(
            place(&http, &built_in(ProcessRole::Web), &[]),
            Placement::Run
        );
        assert_eq!(
            place(&http, &built_in(ProcessRole::Worker), &[]),
            Placement::Skip(Skip::Capability)
        );
    }

    #[test]
    fn plugin_capability_follows_its_parent() {
        let task = task(SYNC);
        assert_eq!(
            place(&task, &built_in(ProcessRole::Worker), &[]),
            Placement::Run
        );
        assert_eq!(
            place(&task, &built_in(ProcessRole::Web), &[]),
            Placement::Skip(Skip::Capability)
        );
        // `pin = []` turns job workers off, so the plugin capability is off.
        let cron = preset_role("[roles.tier]\nbase = \"worker\"\npin = []\n");
        assert_eq!(place(&task, &cron, &[]), Placement::Skip(Skip::Capability));
    }

    #[test]
    fn named_role_sets_a_plugin_capability() {
        let task = task(SYNC);
        let off = preset_role(
            "[roles.tier]\nbase = \"worker\"\ncapabilities = { \"sync.worker\" = false }\n",
        );
        assert_eq!(place(&task, &off, &[]), Placement::Skip(Skip::Capability));
        let on = preset_role(
            "[roles.tier]\nbase = \"worker\"\npin = []\ncapabilities = { \"sync.worker\" = true }\n",
        );
        assert_eq!(place(&task, &on, &[]), Placement::Run);
        // A role cannot turn on a capability that its base does not run.
        let above = preset_role(
            "[roles.tier]\nbase = \"web\"\ncapabilities = { \"sync.worker\" = true }\n",
        );
        assert_eq!(place(&task, &above, &[]), Placement::Skip(Skip::Capability));
    }

    #[test]
    fn queues_follow_the_pin() {
        let queued = task(SYNC).queues(["sync", " sync "]);
        assert_eq!(queued.queue_names(), ["sync"]);
        let worker = built_in(ProcessRole::Worker);
        assert_eq!(place(&queued, &worker, &[]), Placement::Run);
        assert_eq!(
            place(&queued, &worker, &["sync".to_owned()]),
            Placement::Run
        );
        assert_eq!(
            place(&queued, &worker, &["critical".to_owned()]),
            Placement::Skip(Skip::Queues)
        );
        // A role with no job workers drains no queue, whatever the pin.
        let cron = preset_role("[roles.tier]\nbase = \"worker\"\npin = []\n");
        let housekeeping = task(Capability::Maintenance).queues(["sync"]);
        assert_eq!(
            place(&housekeeping, &cron, &[]),
            Placement::Skip(Skip::Queues)
        );
        assert_eq!(place(&housekeeping, &worker, &[]), Placement::Run);
        let http = task(Capability::ServesHttp).queues(["sync"]);
        assert_eq!(
            place(&http, &built_in(ProcessRole::Web), &[]),
            Placement::Skip(Skip::Queues)
        );
        // A task with no queues does not depend on the pin.
        let plain = task(SYNC);
        assert_eq!(
            place(&plain, &worker, &["critical".to_owned()]),
            Placement::Run
        );
    }

    #[test]
    fn explicit_switch_wins() {
        let on = task(SYNC).explicit(Some(true));
        assert_eq!(
            place(&on, &built_in(ProcessRole::Web), &[]),
            Placement::Forced
        );
        assert_eq!(
            place(&on, &built_in(ProcessRole::Worker), &[]),
            Placement::Run
        );
        let off = task(SYNC).explicit(Some(false));
        for role in [ProcessRole::Combined, ProcessRole::Web, ProcessRole::Worker] {
            assert_eq!(
                place(&off, &built_in(role), &[]),
                Placement::Skip(Skip::Disabled)
            );
        }
    }

    /// Every capability, override, switch and queue shape, against the
    /// rules `verification/background_tasks.rs` proves.
    #[test]
    fn place_matches_the_model() {
        let overrides = [None, Some(false), Some(true)];
        let switches = [None, Some(false), Some(true)];
        let pins: [&[&str]; 3] = [&[], &["q"], &["other"]];
        let mut checked = 0_usize;
        for base in [ProcessRole::Combined, ProcessRole::Web, ProcessRole::Worker] {
            for pin_empty in [false, true] {
                for &parent in Capability::ALL {
                    for plugin in [false, true] {
                        for value in overrides {
                            if !plugin && value.is_some() {
                                continue;
                            }
                            let mut preset = RolePreset::new(base);
                            if pin_empty && base.capabilities().job_workers {
                                preset.pin = Some(Vec::new());
                            }
                            if let Some(value) = value {
                                preset.capabilities.insert("model.cap".to_owned(), value);
                            }
                            let mut config = AutumnConfig::default();
                            config.roles.insert("tier".to_owned(), preset);
                            config.role = "tier".parse().expect("role name");
                            let role = config.resolve_role().expect("valid preset");
                            let capability: BackgroundCapability = if plugin {
                                PluginCapability::new("model.cap", parent).into()
                            } else {
                                parent.into()
                            };
                            let ceiling = base.runs(parent);
                            let resolved = role.runs(parent);
                            assert!(!resolved || ceiling, "role_presets.rs: resolved => ceiling");
                            for explicit in switches {
                                for queues in [&[][..], &["q"][..]] {
                                    for pin in pins {
                                        let pin: Vec<String> =
                                            pin.iter().map(|q| (*q).to_owned()).collect();
                                        let task = BackgroundTask::new(
                                            "model.task",
                                            capability,
                                            |_state, _shutdown| async { Ok(()) },
                                        )
                                        .queues(queues.iter().copied())
                                        .explicit(explicit);
                                        let drains = queues.is_empty()
                                            || (role.capabilities().job_workers
                                                && (pin.is_empty()
                                                    || pin.iter().any(|p| p == "q")));
                                        let on = if plugin {
                                            value.map_or(resolved, |v| v && ceiling)
                                        } else {
                                            resolved
                                        };
                                        let expected = explicit.unwrap_or(on && drains);
                                        let placement = place(&task, &role, &pin);
                                        assert_eq!(
                                            placement.starts(),
                                            expected,
                                            "{base:?} {parent} plugin={plugin} value={value:?} \
                                             explicit={explicit:?} queues={queues:?} pin={pin:?}"
                                        );
                                        if explicit.is_none() {
                                            assert!(!placement.starts() || ceiling);
                                        }
                                        if placement == Placement::Forced {
                                            assert_eq!(explicit, Some(true));
                                            assert!(!(on && drains));
                                        }
                                        checked += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(checked > 2_000, "{checked}");
    }

    #[test]
    fn roles_running_lists_built_in_roles_then_presets() {
        let config: AutumnConfig = toml::from_str(
            r#"
[roles.bulk]
base = "worker"
capabilities = { "sync.worker" = false }

[roles.cron]
base = "worker"
pin = []

[roles.sync]
base = "worker"
pin = ["sync"]

[roles.api]
base = "web"
"#,
        )
        .expect("config parses");
        assert_eq!(
            roles_running(&task(SYNC), &config),
            ["combined", "worker", "sync"]
        );
        assert_eq!(
            roles_running(&task(Capability::Maintenance), &config),
            ["combined", "worker", "bulk", "cron", "sync"]
        );
        assert_eq!(
            roles_running(&task(SYNC).queues(["other"]), &config),
            ["combined", "worker"]
        );
        assert_eq!(
            roles_running(&task(Capability::ServesHttp), &config),
            ["combined", "web", "api"]
        );
        assert!(roles_running(&task(SYNC).explicit(Some(false)), &config).is_empty());
        assert_eq!(
            roles_running(&task(SYNC).explicit(Some(true)), &config),
            ["combined", "web", "worker", "api", "bulk", "cron", "sync"]
        );
    }

    #[test]
    fn validate_roles_checks_every_preset() {
        let mut registry = Registry::default();
        registry.add_task(task(SYNC));
        let config =
            |toml_text: &str| -> AutumnConfig { toml::from_str(toml_text).expect("parses") };

        assert_eq!(
            registry.validate_roles(
                &config(
                    "[roles.bulk]\nbase = \"worker\"\ncapabilities = { \"sync.worker\" = false }\n"
                ),
                "bulk"
            ),
            Ok(Vec::new())
        );
        let typo = "[roles.bulk]\nbase = \"worker\"\ncapabilities = { \"sync.wroker\" = false }\n";
        let unknown = BackgroundError::UnknownCapability {
            role: "bulk".to_owned(),
            capability: "sync.wroker".to_owned(),
            declared: vec!["sync.worker".to_owned()],
        };
        // On the active role, an unknown key is an error.
        let error = registry
            .validate_roles(&config(typo), "bulk")
            .expect_err("unknown");
        assert_eq!(error, unknown);
        assert!(error.to_string().contains("no plugin declares"), "{error}");
        // On another preset, it is a warning: that binary may not link the
        // plugin.
        assert_eq!(
            registry.validate_roles(&config(typo), "web"),
            Ok(vec![unknown])
        );

        // Above the base is an error on every preset.
        let above = registry
            .validate_roles(
                &config(
                    "[roles.edge]\nbase = \"web\"\ncapabilities = { \"sync.worker\" = true }\n",
                ),
                "combined",
            )
            .expect_err("above");
        assert!(
            matches!(above, BackgroundError::AboveBase { .. }),
            "{above:?}"
        );
        assert!(above.to_string().contains("above its base"), "{above}");
        assert!(
            above
                .to_string()
                .contains("\"web\" does not run job_workers"),
            "{above}"
        );
        // `false` on a web base is fine: it changes nothing.
        assert_eq!(
            registry.validate_roles(
                &config(
                    "[roles.edge]\nbase = \"web\"\ncapabilities = { \"sync.worker\" = false }\n"
                ),
                "edge",
            ),
            Ok(Vec::new())
        );
    }

    #[test]
    fn queues_skip_switched_off_tasks() {
        let mut registry = Registry::default();
        registry.add_task(
            BackgroundTask::new("sync.off", SYNC, |_s, _t| async { Ok(()) })
                .queues(["off"])
                .explicit(Some(false)),
        );
        registry.add_task(
            BackgroundTask::new("sync.on", SYNC, |_s, _t| async { Ok(()) }).queues(["on"]),
        );
        assert_eq!(registry.queues(), ["on"]);
    }

    #[test]
    fn drain_budget_leaves_half_for_the_later_phases() {
        assert_eq!(
            drain_budget(Duration::from_secs(30)),
            Duration::from_secs(15)
        );
        assert_eq!(drain_budget(Duration::ZERO), Duration::ZERO);
    }

    /// Start one task on a default (`combined`) state and wait for it.
    async fn run_one(task: BackgroundTask) -> BackgroundTaskStatus {
        let state = AppState::for_test();
        let mut registry = Registry::default();
        let name = task.name().to_owned();
        registry.add_task(task);
        let shutdown = CancellationToken::new();
        let runtime = start(&registry, &state, &shutdown, &AutumnConfig::default());
        assert_eq!(runtime.len(), 1);
        assert_eq!(runtime.drain(Duration::from_secs(5)).await, 0);
        state.task_registry.background_snapshot()[&name].clone()
    }

    #[tokio::test]
    async fn start_catches_a_panic_before_the_first_await() {
        #[allow(unreachable_code, reason = "the closure panics before its future")]
        let status = run_one(BackgroundTask::new("test.early_panic", SYNC, |_s, _t| {
            panic!("early boom");
            async { Ok(()) }
        }))
        .await;
        assert_eq!(status.status, BackgroundTaskState::Panicked);
        assert_eq!(status.last_error.as_deref(), Some("early boom"));
    }

    #[tokio::test]
    async fn start_records_a_panic_and_an_early_return() {
        let status = run_one(BackgroundTask::new("test.panic", SYNC, |_s, _t| async {
            panic!("late boom");
        }))
        .await;
        assert_eq!(status.status, BackgroundTaskState::Panicked);
        assert_eq!(status.last_error.as_deref(), Some("late boom"));

        let status = run_one(BackgroundTask::new("test.exits", SYNC, |_s, _t| async {
            Ok(())
        }))
        .await;
        assert_eq!(status.status, BackgroundTaskState::Exited);
        assert_eq!(status.roles, ["combined", "worker"]);
    }

    #[test]
    #[should_panic(expected = "registered twice")]
    fn duplicate_task_name_panics() {
        let mut registry = Registry::default();
        registry.add_task(task(SYNC));
        registry.add_task(task(Capability::Maintenance));
    }

    #[test]
    #[should_panic(expected = "declared twice")]
    fn conflicting_capability_parent_panics() {
        let mut registry = Registry::default();
        registry.add_capability(SYNC);
        registry.add_capability(PluginCapability::new("sync.worker", Capability::Scheduler));
    }

    #[test]
    #[should_panic(expected = "invalid plugin capability name")]
    fn invalid_capability_name_panics() {
        let mut registry = Registry::default();
        registry.add_capability(PluginCapability::new("scheduler", Capability::Scheduler));
    }

    #[test]
    #[should_panic(expected = "invalid background task name")]
    fn invalid_task_name_panics() {
        let mut registry = Registry::default();
        registry.add_task(BackgroundTask::new("Bad Name", SYNC, |_s, _t| async {
            Ok(())
        }));
    }

    #[test]
    fn same_capability_twice_is_one_declaration() {
        let mut registry = Registry::default();
        registry.add_capability(SYNC);
        registry.add_capability(SYNC);
        let mut other = Registry::default();
        other.add_task(task(SYNC).queues(["sync", "bulk"]));
        registry.extend(other);
        assert_eq!(registry.capabilities.len(), 1);
        assert_eq!(registry.queues(), ["sync", "bulk"]);
    }

    #[test]
    fn task_records_its_source_location() {
        let task = task(SYNC);
        assert!(
            task.location().starts_with("autumn/src/background.rs:"),
            "{}",
            task.location()
        );
    }

    #[test]
    fn initial_status_reports_the_placement() {
        let status = initial_status(
            &task(SYNC).queues(["sync"]),
            Placement::Run,
            vec!["worker".to_owned()],
        );
        assert_eq!(status.capability, "sync.worker");
        assert_eq!(status.parent.as_deref(), Some("job_workers"));
        assert_eq!(status.status, BackgroundTaskState::Running);
        assert!(status.runs_here);
        let json = serde_json::to_value(&status).expect("serialize");
        assert_eq!(json["queues"], serde_json::json!(["sync"]));
        assert_eq!(json["status"], "running");
        assert!(json.get("explicit").is_none());

        let skipped = initial_status(
            &task(Capability::Maintenance),
            Placement::Skip(Skip::Capability),
            Vec::new(),
        );
        assert_eq!(skipped.status, BackgroundTaskState::NotPlaced);
        assert!(skipped.parent.is_none());
        let disabled = initial_status(&task(SYNC), Placement::Skip(Skip::Disabled), Vec::new());
        assert_eq!(disabled.status, BackgroundTaskState::Disabled);
    }

    /// Sets its flag when the future that owns it is dropped.
    struct DropFlag(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// `drain` returns only after an aborted task has stopped, so the later
    /// shutdown phases do not run beside it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_returns_after_an_aborted_task_stops() {
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = DropFlag(Arc::clone(&dropped));
        let runtime = BackgroundRuntime {
            registry: None,
            tasks: vec![(
                "stuck".to_owned(),
                tokio::spawn(async move {
                    let _guard = guard;
                    loop {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }),
            )],
        };
        assert_eq!(runtime.drain(Duration::from_millis(20)).await, 1);
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "the aborted task still runs after drain"
        );
    }

    #[tokio::test]
    async fn drain_waits_for_tasks_and_aborts_the_slow_ones() {
        let shutdown = CancellationToken::new();
        let polite = shutdown.child_token();
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = Arc::clone(&stopped);
        let registry = crate::actuator::TaskRegistry::new();
        for name in ["polite", "stuck"] {
            registry.register_background(
                name,
                initial_status(&task(SYNC), Placement::Run, Vec::new()),
            );
        }
        let runtime = BackgroundRuntime {
            registry: Some(registry.clone()),
            tasks: vec![
                (
                    "polite".to_owned(),
                    tokio::spawn(async move {
                        polite.cancelled().await;
                        seen.store(true, std::sync::atomic::Ordering::SeqCst);
                    }),
                ),
                (
                    "stuck".to_owned(),
                    tokio::spawn(async { std::future::pending::<()>().await }),
                ),
            ],
        };
        assert_eq!(runtime.len(), 2);
        shutdown.cancel();
        let aborted = runtime.drain(Duration::from_millis(100)).await;
        assert_eq!(aborted, 1);
        assert!(stopped.load(std::sync::atomic::Ordering::SeqCst));
        let snapshot = registry.background_snapshot();
        assert_eq!(snapshot["stuck"].status, BackgroundTaskState::Aborted);
        assert_eq!(snapshot["polite"].status, BackgroundTaskState::Running);
    }
}
