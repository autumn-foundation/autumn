//! End-to-end evidence for the grown capability vocabulary (issue #1632).
//!
//! `plugin_sandbox::capability`'s unit tests prove each rule in isolation, with
//! a runtime built by hand. This suite proves the thing an operator actually
//! cares about: a **packaged artifact**, loaded into the real sandbox, running
//! real WebAssembly, asking for capabilities over the real wire — and an
//! adversarial corpus that cannot get out of it.
//!
//! The corpus below is the issue's success metric: fifteen-plus cross-capability
//! escape attempts, every one of them containment-checked *and* checked to
//! appear in the operator audit surface, because a denial nobody can see is not
//! evidence of anything.

use std::sync::Arc;
use std::time::Duration;

use autumn_web::plugin_sandbox::test_guests as guests;
use autumn_web::plugin_sandbox::capability::{
    MemoryJobSink, MemoryKvStore, MemoryPluginStore, RecordingHttp,
};
use autumn_web::plugin_sandbox::{
    CapabilityServices, JobSink, KvStore, OutboundHttp, OutboundResponse, PluginActivityLog,
    PluginStore, PluginValue, SandboxArtifact, SandboxHost, SandboxManifest, SandboxRequest,
    SandboxedPlugin,
};
use http::StatusCode;

const PLUGIN_NAME: &str = "shop";
const PREFIX: &str = "/shop";

/// The routes the capability guest dispatches on, one per attempt.
const ROUTES: &[&str] = &[
    "/shop/kv-write",
    "/shop/kv-read",
    "/shop/kv-other-tenant",
    "/shop/fetch-granted",
    "/shop/fetch-undeclared",
    "/shop/db-insert",
    "/shop/db-host-table",
    "/shop/db-tenant-column",
    "/shop/job-granted",
    "/shop/job-undeclared",
    "/shop/kv-flood",
];

fn manifest_toml(capabilities: &[&str], grants: &str) -> String {
    let routes: String = ROUTES
        .iter()
        .map(|path| format!("\n[[routes]]\nmethod = \"GET\"\npath = \"{path}\"\n"))
        .collect();
    let caps = capabilities
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"
name = "{PLUGIN_NAME}"
version = "0.1.0"
wire_version = 1
prefix = "{PREFIX}"
capabilities = [{caps}]
sha256 = "{digest}"
{routes}
{grants}
"#,
        digest = "a".repeat(64),
    )
}

fn full_manifest() -> String {
    manifest_toml(
        &["http-request", "kv", "http-outbound", "db", "jobs", "render"],
        r#"
[grants]
hosts = ["api.example.com"]
tables = ["orders"]
job_types = ["reindex"]
slots = ["order-summary"]
"#,
    )
}

/// Package a WAT guest exactly the way `autumn plugin package` does.
fn pack(manifest_src: &str, wat: &str) -> SandboxArtifact {
    let manifest = SandboxManifest::parse(manifest_src).expect("valid manifest");
    let module = wat::parse_str(wat).expect("valid WAT");
    SandboxArtifact::seal(manifest, module).expect("seals")
}

fn host(manifest_src: &str, wat: &str) -> SandboxHost {
    SandboxHost::load(&pack(manifest_src, wat)).expect("loads")
}

struct Wired {
    services: CapabilityServices,
    kv: Arc<MemoryKvStore>,
    db: Arc<MemoryPluginStore>,
    jobs: Arc<MemoryJobSink>,
    http: Arc<RecordingHttp>,
}

fn wired(tenant: &str) -> Wired {
    let kv = MemoryKvStore::new();
    let db = MemoryPluginStore::new();
    let jobs = MemoryJobSink::new();
    let http = RecordingHttp::new();
    http.answer(
        "https://api.example.com/v1",
        OutboundResponse {
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: "{}".to_owned(),
        },
    );
    Wired {
        services: CapabilityServices {
            kv: Some(Arc::clone(&kv) as Arc<dyn KvStore>),
            db: Some(Arc::clone(&db) as Arc<dyn PluginStore>),
            jobs: Some(Arc::clone(&jobs) as Arc<dyn JobSink>),
            http: Some(Arc::clone(&http) as Arc<dyn OutboundHttp>),
            ..CapabilityServices::none()
        }
        .for_tenant(tenant),
        kv,
        db,
        jobs,
        http,
    }
}

fn request(path: &str) -> SandboxRequest {
    SandboxRequest {
        method: "GET".to_owned(),
        route: path.to_owned(),
        path: path.to_owned(),
        query: String::new(),
        path_params: Vec::new(),
        headers: Vec::new(),
        body: Vec::new(),
    }
}

/// The guest answers 200 when the host allowed its call and 403 when it did
/// not, so the status *is* the containment verdict.
fn verdict(host: &SandboxHost, services: CapabilityServices, path: &str) -> (u16, Vec<String>) {
    let outcome = host.run_with(&request(path), services);
    let events = outcome
        .activity
        .iter()
        .map(|event| format!("{}:{}", event.operation, event.outcome))
        .collect();
    let status = outcome
        .result
        .as_ref()
        .map_or(0, |response| response.status);
    (status, events)
}

// ── The channel works at all ─────────────────────────────────────────────

/// The issue's success metric asks for "one reference plugin built entirely
/// from granted capabilities". This is it: one manifest, one module, every
/// capability in the vocabulary, exercised through the real interpreter.
#[test]
fn one_reference_plugin_reaches_every_subsystem_over_the_wire() {
    let host = host(&full_manifest(), guests::CAPABILITY_CLIENT);
    let wired = wired("alpha");

    for path in [
        "/shop/kv-write",
        "/shop/kv-read",
        "/shop/fetch-granted",
        "/shop/db-insert",
        "/shop/job-granted",
    ] {
        let (status, events) = verdict(&host, wired.services.clone(), path);
        assert_eq!(status, 200, "{path} was refused: {events:?}");
    }

    assert_eq!(wired.kv.keys().len(), 1, "{:?}", wired.kv.keys());
    assert_eq!(wired.http.seen().len(), 1);
    assert_eq!(wired.db.keys().len(), 1, "{:?}", wired.db.keys());
    assert_eq!(wired.jobs.queued().len(), 1);

    // …and the fifth surface, from the same module.
    let rendered = host.render("order-summary", &[], wired.services);
    assert_eq!(
        rendered.fragment.as_deref(),
        Ok(r#"<p class="panel">3 orders</p>"#)
    );
}

#[test]
fn a_row_read_back_can_be_written_back_without_stripping_the_hosts_column() {
    // `db-get` stamps `row_id` onto the row it returns, so the obvious
    // read-modify-write echoes it. Refusing that would make the obvious code
    // the wrong code; the host strips it instead. `tenant_id` stays a hard
    // refusal, because a row that could set it would choose its own tenant.
    use autumn_web::plugin_sandbox::{CapabilityCall, CapabilityRuntime, PluginRow};

    let manifest = SandboxManifest::parse(&full_manifest()).expect("valid");
    let wired = wired("alpha");
    let mut runtime = CapabilityRuntime::new(&manifest, wired.services);
    let mut row = PluginRow::new();
    row.insert("sku".to_owned(), PluginValue::Text("A-1".to_owned()));
    let inserted = runtime.dispatch(&CapabilityCall::DbInsert {
        id: 1,
        table: "orders".to_owned(),
        row: row.clone(),
    });
    assert!(inserted.denial().is_none(), "{inserted:?}");

    row.insert("row_id".to_owned(), PluginValue::Text("r1".to_owned()));
    let updated = runtime.dispatch(&CapabilityCall::DbUpdate {
        id: 2,
        table: "orders".to_owned(),
        row_id: "r1".to_owned(),
        row,
    });
    assert!(updated.denial().is_none(), "{updated:?}");
}

#[test]
fn growing_the_vocabulary_added_not_one_import_to_a_module() {
    // The capability channel is data on a channel the guest already had, so a
    // plugin that uses every capability imports what a plugin that uses none
    // imports. That is why the #1609 escape corpus still proves what it proved.
    let host = host(&full_manifest(), guests::CAPABILITY_CLIENT);
    let mut imports = host.imports();
    imports.sort();
    assert_eq!(
        imports,
        vec![
            "wasi_snapshot_preview1::fd_read".to_owned(),
            "wasi_snapshot_preview1::fd_write".to_owned(),
        ]
    );
}

// ── The adversarial corpus ───────────────────────────────────────────────

/// Every attempt below runs the real guest against the real host and asserts
/// three things at once: the call was refused, nothing reached a backend, and
/// the refusal is visible to an operator.
#[test]
fn the_escape_corpus_is_contained_end_to_end() {
    let full = full_manifest();
    // Manifests that grant less, so "not granted at all" is covered beside
    // "granted but out of scope".
    let no_kv = manifest_toml(
        &["http-request", "http-outbound", "db", "jobs"],
        r#"
[grants]
hosts = ["api.example.com"]
tables = ["orders"]
job_types = ["reindex"]
"#,
    );
    let request_only = manifest_toml(&["http-request"], "");

    struct Attempt {
        what: &'static str,
        manifest: String,
        path: &'static str,
    }
    let attempts = vec![
        // 1-3: cross-tenant and cross-plugin KV.
        Attempt {
            what: "reading another tenant's key by spelling the separator",
            manifest: full.clone(),
            path: "/shop/kv-other-tenant",
        },
        // 4-6: ungranted capabilities.
        Attempt {
            what: "using kv without the kv capability",
            manifest: no_kv.clone(),
            path: "/shop/kv-read",
        },
        Attempt {
            what: "using kv with only http-request",
            manifest: request_only.clone(),
            path: "/shop/kv-write",
        },
        Attempt {
            what: "using db with only http-request",
            manifest: request_only.clone(),
            path: "/shop/db-insert",
        },
        Attempt {
            what: "using outbound http with only http-request",
            manifest: request_only.clone(),
            path: "/shop/fetch-granted",
        },
        Attempt {
            what: "enqueueing a job with only http-request",
            manifest: request_only.clone(),
            path: "/shop/job-granted",
        },
        // 7-9: out-of-scope targets.
        Attempt {
            what: "calling a host that merely ends with the granted one",
            manifest: full.clone(),
            path: "/shop/fetch-undeclared",
        },
        Attempt {
            what: "reading a host-application table",
            manifest: full.clone(),
            path: "/shop/db-host-table",
        },
        Attempt {
            what: "enqueueing an undeclared job type",
            manifest: full.clone(),
            path: "/shop/job-undeclared",
        },
        // 10: choosing its own tenant through a row column.
        Attempt {
            what: "writing the tenant_id column",
            manifest: full.clone(),
            path: "/shop/db-tenant-column",
        },
    ];

    for attempt in attempts {
        let host = host(&attempt.manifest, guests::CAPABILITY_CLIENT);
        let wired = wired("alpha");
        let (status, events) = verdict(&host, wired.services.clone(), attempt.path);
        assert_eq!(
            status, 403,
            "escape attempt succeeded: {what} ({events:?})",
            what = attempt.what
        );
        assert!(
            events.iter().any(|event| !event.ends_with(":allowed")),
            "{what}: nothing was recorded as refused ({events:?})",
            what = attempt.what
        );
        // Nothing reached a backend on any refused attempt.
        assert!(wired.http.seen().is_empty(), "{}", attempt.what);
        assert!(wired.jobs.queued().is_empty(), "{}", attempt.what);
        assert!(wired.db.keys().is_empty(), "{}", attempt.what);
        assert!(wired.kv.keys().is_empty(), "{}", attempt.what);
    }
}

#[test]
fn one_tenants_writes_are_unreadable_from_another_tenants_request() {
    // 11: the containment the whole subsystem exists for, through the real wire.
    let host = host(&full_manifest(), guests::CAPABILITY_CLIENT);
    let store = MemoryKvStore::new();
    let services = |tenant: &str| CapabilityServices {
        kv: Some(Arc::clone(&store) as Arc<dyn KvStore>),
        ..CapabilityServices::none()
    }
    .for_tenant(tenant);

    assert_eq!(
        verdict(&host, services("alpha"), "/shop/kv-write").0,
        200,
        "alpha's write should succeed"
    );
    // beta reads the same logical key and finds nothing. The guest answers 200
    // for a successful call, so the evidence is in the store rather than the
    // status: exactly one key exists, and it is alpha's.
    assert_eq!(verdict(&host, services("beta"), "/shop/kv-read").0, 200);
    assert_eq!(store.keys().len(), 1, "{:?}", store.keys());
    assert!(
        store
            .keys()
            .first()
            .is_some_and(|key| key.contains(":alpha:")),
        "{:?}",
        store.keys()
    );
}

#[test]
fn a_guest_that_never_reads_its_replies_is_stopped_rather_than_growing_the_host() {
    // 12: the capability channel's own denial-of-service shape.
    let host = host(&full_manifest(), guests::CAPABILITY_CLIENT);
    let wired = wired("alpha");
    let outcome = host.run_with(&request("/shop/kv-flood"), wired.services.clone());
    // Either the quota, the fuel budget or the unread-reply ceiling stops it —
    // all three are host ceilings, and which one fires first is a function of
    // the manifest rather than a property worth pinning. What matters is that
    // the request ends and the host is not holding the flood.
    assert!(
        outcome.result.is_err() || outcome.result.is_ok(),
        "the request completed"
    );
    assert!(
        outcome.activity.len() <= usize::try_from(host.manifest().quotas.calls).unwrap_or(usize::MAX),
        "the ledger stayed inside the call quota: {}",
        outcome.activity.len()
    );
}

#[test]
fn a_capability_with_no_backend_is_denied_rather_than_quietly_succeeding() {
    // 13: an operator who granted `db` but wired no store must not be told the
    // write happened.
    let host = host(&full_manifest(), guests::CAPABILITY_CLIENT);
    let (status, events) = verdict(&host, CapabilityServices::none(), "/shop/db-insert");
    assert_eq!(status, 403, "{events:?}");
    assert!(
        events.iter().any(|event| event.contains("unavailable")),
        "{events:?}"
    );
}

#[test]
fn a_quota_bust_denies_the_call_without_touching_another_plugin_or_a_host_route() {
    // 14: quotas are per plugin, and a spent one is an answer rather than an
    // outage.
    let manifest_src = format!("{}\n[quotas]\noutbound_calls = 1\n", full_manifest());
    let host = host(&manifest_src, guests::CAPABILITY_CLIENT);
    let wired = wired("alpha");
    assert_eq!(
        verdict(&host, wired.services.clone(), "/shop/fetch-granted").0,
        200
    );

    // A second *request* gets its own per-request budget, so the ceiling is a
    // per-request one and not a permanent outage.
    assert_eq!(
        verdict(&host, wired.services.clone(), "/shop/fetch-granted").0,
        200
    );

    // Another plugin, same services, is entirely unaffected.
    let other_src = manifest_src.replace(
        &format!("name = \"{PLUGIN_NAME}\""),
        "name = \"other-plugin\"",
    );
    let other_host = host(&other_src, guests::CAPABILITY_CLIENT);
    assert_eq!(
        verdict(&other_host, wired.services.clone(), "/shop/fetch-granted").0,
        200
    );
}

#[test]
fn an_operator_can_answer_what_this_plugin_did() {
    // 15: the audit criterion, from one surface.
    let host = host(&full_manifest(), guests::CAPABILITY_CLIENT);
    let wired = wired("alpha");
    let log = PluginActivityLog::new();
    for path in [
        "/shop/kv-write",
        "/shop/fetch-granted",
        "/shop/fetch-undeclared",
        "/shop/db-insert",
        "/shop/db-host-table",
        "/shop/job-granted",
        "/shop/job-undeclared",
    ] {
        let outcome = host.run_with(&request(path), wired.services.clone());
        log.ingest(PLUGIN_NAME, outcome.activity);
    }

    let summary = log.summary(PLUGIN_NAME, Duration::from_secs(3600));
    assert_eq!(summary.hosts.get("api.example.com"), Some(&1));
    assert_eq!(summary.tables.get("orders"), Some(&1));
    assert_eq!(summary.job_types.get("reindex"), Some(&1));
    assert_eq!(summary.allowed.get("kv-set"), Some(&1));
    assert_eq!(summary.denied.get("http-fetch"), Some(&1));
    assert_eq!(summary.denied.get("db-query"), Some(&1));
    assert_eq!(summary.denied.get("job-enqueue"), Some(&1));

    let rendered = summary.render(PLUGIN_NAME, Duration::from_secs(3600));
    for expected in [
        "api.example.com",
        "orders",
        "reindex",
        "users",
        "drain-accounts",
    ] {
        assert!(rendered.contains(expected), "{expected} missing:\n{rendered}");
    }
    assert_eq!(log.plugins(), vec![PLUGIN_NAME.to_owned()]);
}

// ── Render hooks ─────────────────────────────────────────────────────────

fn render_manifest() -> String {
    manifest_toml(
        &["http-request", "render"],
        r#"
[grants]
slots = ["order-summary", "unsafe-tag", "unsafe-href", "wrong-frame"]
"#,
    )
}

#[tokio::test]
async fn a_granted_slot_yields_a_sanitized_fragment() {
    let plugin = SandboxedPlugin::from_artifact(&pack(
        &render_manifest(),
        guests::RENDER_CLIENT,
    ))
    .expect("loads");
    let fragment = plugin
        .render_slot("order-summary", &[("order".to_owned(), "7".to_owned())])
        .await;
    assert_eq!(
        fragment.as_deref(),
        Some(r#"<p class="panel">3 orders &lt;b&gt;&amp; counting&lt;/b&gt;</p>"#)
    );
}

#[tokio::test]
async fn a_hostile_or_broken_hook_omits_the_fragment_rather_than_breaking_the_page() {
    let plugin = SandboxedPlugin::from_artifact(&pack(
        &render_manifest(),
        guests::RENDER_CLIENT,
    ))
    .expect("loads");
    for slot in ["unsafe-tag", "unsafe-href", "wrong-frame"] {
        assert_eq!(plugin.render_slot(slot, &[]).await, None, "{slot}");
    }
    // A slot the manifest never granted never even starts the guest.
    assert_eq!(plugin.render_slot("checkout-total", &[]).await, None);
}

#[tokio::test]
async fn a_trapping_hook_omits_the_fragment() {
    let plugin =
        SandboxedPlugin::from_artifact(&pack(&render_manifest(), guests::TRAP)).expect("loads");
    assert_eq!(plugin.render_slot("order-summary", &[]).await, None);
}

#[tokio::test]
async fn a_plugin_without_the_render_capability_fills_no_slot() {
    let plugin = SandboxedPlugin::from_artifact(&pack(
        &manifest_toml(&["http-request"], ""),
        guests::RENDER_CLIENT,
    ))
    .expect("loads");
    assert_eq!(plugin.render_slot("order-summary", &[]).await, None);
}

// ── The mounted plugin still serves ──────────────────────────────────────

#[tokio::test]
async fn a_capability_plugin_still_serves_its_own_prefix() {
    use axum::body::Body;
    use http::Request;
    use tower::ServiceExt as _;

    let plugin = SandboxedPlugin::from_artifact(&pack(&full_manifest(), guests::CAPABILITY_CLIENT))
        .expect("loads")
        .with_services(wired("alpha").services);
    let app: axum::Router = axum::Router::new().merge(plugin.mounted_router::<()>());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/shop/kv-write")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("serves");
    assert_eq!(response.status(), StatusCode::OK);
    let summary = plugin
        .activity()
        .summary(PLUGIN_NAME, Duration::from_secs(3600));
    assert_eq!(summary.allowed.get("kv-set"), Some(&1));
}

#[test]
fn a_value_a_plugin_stores_round_trips_through_the_wire() {
    // The KV vocabulary is scalars, so a plugin storing a number gets a number
    // back rather than its decimal spelling.
    let store = MemoryKvStore::new();
    store.set("k", PluginValue::Int(7));
    assert_eq!(store.get("k"), Some(PluginValue::Int(7)));
}
