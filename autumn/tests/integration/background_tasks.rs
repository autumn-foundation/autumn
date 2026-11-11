//! Role-aware background tasks (issue #3545): the framework starts a task only
//! on the roles that run its capability, and stops it with the app.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::background::PluginCapability;
use autumn_web::config::{AutumnConfig, Capability};
use autumn_web::plugin::Plugin;
use autumn_web::test::{TestApp, TestClient};

/// A plugin capability with the `job_workers` default.
const SYNC_WORKER: PluginCapability = PluginCapability::new("sync.worker", Capability::JobWorkers);

/// What one task saw.
#[derive(Default)]
struct Probe {
    starts: AtomicUsize,
    stopped: AtomicBool,
}

impl Probe {
    fn started(&self) -> bool {
        self.starts.load(Ordering::SeqCst) > 0
    }
}

/// Registers one loop that runs until shutdown.
struct LoopPlugin {
    name: &'static str,
    capability: autumn_web::background::BackgroundCapability,
    probe: Arc<Probe>,
}

impl Plugin for LoopPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(self.name)
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let probe = self.probe;
        app.background_task(self.name, self.capability, move |_state, shutdown| {
            let probe = Arc::clone(&probe);
            async move {
                probe.starts.fetch_add(1, Ordering::SeqCst);
                shutdown.cancelled().await;
                probe.stopped.store(true, Ordering::SeqCst);
                Ok(())
            }
        })
    }
}

fn config(toml_text: &str) -> AutumnConfig {
    toml::from_str(toml_text).expect("config parses")
}

fn client(
    toml_text: &str,
    capability: impl Into<autumn_web::background::BackgroundCapability>,
) -> (TestClient, Arc<Probe>) {
    let probe = Arc::new(Probe::default());
    let client = TestApp::new()
        .config(config(toml_text))
        .plugin(LoopPlugin {
            name: "test.loop",
            capability: capability.into(),
            probe: Arc::clone(&probe),
        })
        .build();
    (client, probe)
}

/// Wait up to two seconds for `done`.
async fn eventually(done: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    done()
}

const SENSITIVE: &str = "[actuator]\nsensitive = true\n";

#[tokio::test]
async fn task_runs_on_combined() {
    let (_client, probe) = client(
        &format!("role = \"combined\"\n{SENSITIVE}"),
        Capability::Maintenance,
    );
    assert!(
        eventually(|| probe.started()).await,
        "the task did not start"
    );
}

#[tokio::test]
async fn task_runs_on_worker() {
    let (_client, probe) = client(
        &format!("role = \"worker\"\n{SENSITIVE}"),
        Capability::Maintenance,
    );
    assert!(
        eventually(|| probe.started()).await,
        "the task did not start"
    );
}

#[tokio::test]
async fn task_does_not_run_on_web() {
    let (client, probe) = client(
        &format!("role = \"web\"\n{SENSITIVE}"),
        Capability::Maintenance,
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!probe.started(), "the task started on web");
    let body: serde_json::Value = client
        .get("/actuator/tasks")
        .send()
        .await
        .assert_ok()
        .json();
    let task = &body["background_tasks"]["test.loop"];
    assert_eq!(task["runs_here"], false, "{body}");
    assert_eq!(task["status"], "not_placed", "{body}");
}

#[tokio::test]
async fn task_stops_on_shutdown() {
    let (client, probe) = client("role = \"combined\"\n", Capability::Maintenance);
    assert!(
        eventually(|| probe.started()).await,
        "the task did not start"
    );
    assert!(!probe.stopped.load(Ordering::SeqCst));
    drop(client);
    assert!(
        eventually(|| probe.stopped.load(Ordering::SeqCst)).await,
        "the task did not see the shutdown token"
    );
}

#[tokio::test]
async fn plugin_capability_follows_its_parent_on_built_in_roles() {
    let (_client, probe) = client("role = \"worker\"\n", SYNC_WORKER);
    assert!(
        eventually(|| probe.started()).await,
        "the task did not start on worker"
    );

    let (_client, probe) = client("role = \"web\"\n", SYNC_WORKER);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!probe.started(), "the task started on web");
}

#[tokio::test]
async fn named_role_turns_a_plugin_capability_off() {
    let toml_text = r#"
role = "bulk"

[roles.bulk]
base = "worker"
capabilities = { "sync.worker" = false }
"#;
    let (_client, probe) = client(toml_text, SYNC_WORKER);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!probe.started(), "the role turned the capability off");
}

#[tokio::test]
async fn named_role_turns_a_plugin_capability_on() {
    // `pin = []` turns job workers off, so `sync.worker` is off by default.
    // The worker base runs job workers, so the role can turn it on.
    let toml_text = r#"
role = "cron"

[roles.cron]
base = "worker"
pin = []
capabilities = { "sync.worker" = true }
"#;
    let (_client, probe) = client(toml_text, SYNC_WORKER);
    assert!(
        eventually(|| probe.started()).await,
        "the role turned the capability on"
    );
}

#[tokio::test]
#[should_panic(expected = "above its base")]
async fn named_role_cannot_turn_a_plugin_capability_on_above_its_base() {
    let toml_text = r#"
role = "edge"

[roles.edge]
base = "web"
capabilities = { "sync.worker" = true }
"#;
    let _ = client(toml_text, SYNC_WORKER);
}

#[tokio::test]
#[should_panic(expected = "no plugin declares")]
async fn named_role_with_an_unknown_plugin_capability_fails() {
    let toml_text = r#"
role = "bulk"

[roles.bulk]
base = "worker"
capabilities = { "sync.wroker" = false }
"#;
    let _ = client(toml_text, SYNC_WORKER);
}

#[tokio::test]
async fn actuator_tasks_lists_the_roles_that_run_a_task() {
    let toml_text = r#"
role = "combined"

[actuator]
sensitive = true

[roles.bulk]
base = "worker"
capabilities = { "sync.worker" = false }

[roles.cron]
base = "worker"
pin = []
"#;
    let (client, probe) = client(toml_text, SYNC_WORKER);
    assert!(eventually(|| probe.started()).await);
    let body: serde_json::Value = client
        .get("/actuator/tasks")
        .send()
        .await
        .assert_ok()
        .json();
    let task = &body["background_tasks"]["test.loop"];
    assert_eq!(task["capability"], "sync.worker", "{body}");
    assert_eq!(task["parent"], "job_workers", "{body}");
    assert_eq!(task["registered_by"], "test.loop", "{body}");
    assert_eq!(task["runs_here"], true, "{body}");
    assert_eq!(task["status"], "running", "{body}");
    assert_eq!(
        task["roles"],
        serde_json::json!(["combined", "worker"]),
        "{body}"
    );
}

#[tokio::test]
async fn queued_task_runs_only_where_the_pin_drains_its_queue() {
    let probe = Arc::new(Probe::default());
    let task_probe = Arc::clone(&probe);
    let toml_text = r#"
role = "critical"

[actuator]
sensitive = true

[roles.critical]
base = "worker"
pin = ["critical"]

[roles.sync]
base = "worker"
pin = ["sync"]
"#;
    let client = TestApp::new()
        .config(config(toml_text))
        .background(
            autumn_web::background::BackgroundTask::new(
                "sync.drain",
                SYNC_WORKER,
                move |_state, shutdown| {
                    let probe = Arc::clone(&task_probe);
                    async move {
                        probe.starts.fetch_add(1, Ordering::SeqCst);
                        shutdown.cancelled().await;
                        Ok(())
                    }
                },
            )
            .queues(["sync"]),
        )
        .build();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !probe.started(),
        "the critical tier does not drain the sync queue"
    );
    let body: serde_json::Value = client
        .get("/actuator/tasks")
        .send()
        .await
        .assert_ok()
        .json();
    let task = &body["background_tasks"]["sync.drain"];
    assert_eq!(task["queues"], serde_json::json!(["sync"]), "{body}");
    assert_eq!(
        task["roles"],
        serde_json::json!(["combined", "worker", "sync"]),
        "{body}"
    );
}

#[tokio::test]
async fn explicit_switch_runs_a_task_on_web() {
    let probe = Arc::new(Probe::default());
    let task_probe = Arc::clone(&probe);
    let _client = TestApp::new()
        .config(config("role = \"web\"\n"))
        .background(
            autumn_web::background::BackgroundTask::new(
                "sync.forced",
                SYNC_WORKER,
                move |_state, shutdown| {
                    let probe = Arc::clone(&task_probe);
                    async move {
                        probe.starts.fetch_add(1, Ordering::SeqCst);
                        shutdown.cancelled().await;
                        Ok(())
                    }
                },
            )
            .explicit(Some(true)),
        )
        .build();
    assert!(
        eventually(|| probe.started()).await,
        "an explicit switch wins"
    );
}

#[tokio::test]
async fn failed_task_reports_its_error() {
    let toml_text = "role = \"combined\"\n[actuator]\nsensitive = true\n";
    let client = TestApp::new()
        .config(config(toml_text))
        .background_task(
            "test.fails",
            Capability::Maintenance,
            |_state, _shutdown| async {
                Err(autumn_web::AutumnError::internal_server_error_msg("boom"))
            },
        )
        .build();
    let mut status = serde_json::Value::Null;
    for _ in 0..200 {
        let body: serde_json::Value = client
            .get("/actuator/tasks")
            .send()
            .await
            .assert_ok()
            .json();
        status = body["background_tasks"]["test.fails"].clone();
        if status["status"] == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(status["status"], "failed", "{status}");
    assert!(
        status["last_error"]
            .as_str()
            .is_some_and(|e| e.contains("boom")),
        "{status}"
    );
}

#[tokio::test]
async fn role_runs_plugin_reads_the_resolved_role() {
    let role = config(
        r#"
role = "bulk"

[roles.bulk]
base = "worker"
capabilities = { "sync.worker" = false }
"#,
    )
    .resolve_role()
    .expect("role resolves");
    assert!(!role.runs_plugin(SYNC_WORKER));
    let worker = autumn_web::Role::from(autumn_web::ProcessRole::Worker);
    assert!(worker.runs_plugin(SYNC_WORKER));
    let web = autumn_web::Role::from(autumn_web::ProcessRole::Web);
    assert!(!web.runs_plugin(SYNC_WORKER));
}
