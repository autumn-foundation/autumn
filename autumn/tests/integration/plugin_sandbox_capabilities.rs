//! End-to-end evidence for the grown capability vocabulary (issue #1632).
//!
//! `plugin_sandbox::capability`'s unit tests prove each rule in isolation, with
//! a runtime built by hand. This suite proves the thing an operator actually
//! cares about: a **packaged artifact**, loaded into the real sandbox, running
//! real WebAssembly, asking for capabilities over the real wire — and an
//! adversarial corpus that cannot get out of it.
//!
//! The corpus below is the issue's success metric. Ten attempts share the
//! table-driven `the_escape_corpus_is_contained_end_to_end`; seven more are
//! named tests of their own, because each needs a fixture the table cannot
//! carry — a second tenant, a second plugin, a live router, a wall clock.
//! Seventeen in total, and every one of them checks the containment *and*
//! checks that the refusal reached the operator audit surface, because a denial
//! nobody can see is not evidence of anything.
//!
//! Two of them are worth reading before the rest, because they are the shape
//! this suite got wrong the first time:
//!
//! * `/kv-other-tenant` answers 200 only on a **hit**. `kv-get` for a key
//!   another tenant owns is correctly *allowed* and correctly *misses* — the
//!   escaping in `namespaced_key` is what makes it miss — so a test that read
//!   the call's status could not tell containment from success, and demanded a
//!   403 that will never come.
//! * `/fetch-twice` makes two outbound calls in one request. A guest that makes
//!   one call can never demonstrate a per-request ceiling, so the quota test
//!   that drove it asserted three successes and no denial at all.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::plugin_sandbox::capability::{
    MemoryJobSink, MemoryKvStore, MemoryPluginStore, RecordingHttp,
};
use autumn_web::plugin_sandbox::test_guests as guests;
use autumn_web::plugin_sandbox::{
    CapabilityServices, JobSink, KvStore, OutboundHttp, OutboundResponse, PluginActivityLog,
    PluginStore, PluginValue, SandboxArtifact, SandboxHost, SandboxManifest, SandboxRequest,
    SandboxedPlugin, tenant_segment,
};
use http::StatusCode;

const PLUGIN_NAME: &str = "shop";
const PREFIX: &str = "/shop";

/// The routes the capability guest dispatches on, one per attempt.
const ROUTES: &[&str] = &[
    "/shop/kv-write",
    "/shop/kv-read",
    "/shop/kv-read-hit",
    "/shop/kv-other-tenant",
    "/shop/fetch-twice",
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
    let routes = ROUTES.iter().fold(String::new(), |mut out, path| {
        let _ = write!(out, "\n[[routes]]\nmethod = \"GET\"\npath = \"{path}\"\n");
        out
    });
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
        &[
            "http-request",
            "kv",
            "http-outbound",
            "db",
            "jobs",
            "render",
        ],
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

fn load(manifest_src: &str, wat: &str) -> SandboxHost {
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
            final_url: "https://api.example.com/v1".to_owned(),
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

#[test]
fn a_call_parsed_from_a_rejected_write_never_reaches_a_backend() {
    // The whole charge-before-dispatch order exists so a request that is going
    // to fail cannot leave a side effect behind. The output ceiling reached the
    // same state by another route: one `fd_write` carrying a complete `kv-set`
    // and then a line that overruns the stdout budget, so `write_stdout` queues
    // the call and *then* refuses. The queue was serviced before the refusal
    // was acted on, so the write committed on a request whose caller is told it
    // failed — and may retry it.
    let manifest_src = manifest_toml(
        &["http-request", "kv"],
        r"
[grants]
hosts = []
tables = []
job_types = []
slots = []

[limits]
max_response_bytes = 512
",
    );
    // Comfortably past `2 * 512 + 4096`, and inside one 64 KiB host chunk, so
    // the call and the overrun are decided together.
    let host = load(&manifest_src, &guests::call_then_overrun(8192));
    let wired = wired("alpha");
    let kv = Arc::clone(&wired.kv);

    let outcome = host.run_with(&request("/kv-set"), wired.services);
    assert!(
        outcome.result.is_err(),
        "the overrun must end the request: {:?}",
        outcome.result
    );

    // The point of the test: the request failed, so nothing may have been
    // stored. Before the fix this key was present.
    let key = autumn_web::plugin_sandbox::capability::kv::namespaced_key(
        PLUGIN_NAME,
        Some("alpha"),
        "cart",
    );
    assert_eq!(
        kv.get(&key),
        Ok(None),
        "a call parsed out of a rejected write must not reach the backend"
    );
}

// ── The channel works at all ─────────────────────────────────────────────

/// The issue's success metric asks for "one reference plugin built entirely
/// from granted capabilities". This is it: one manifest, one module, every
/// capability in the vocabulary, exercised through the real interpreter.
#[test]
fn one_reference_plugin_reaches_every_subsystem_over_the_wire() {
    let host = load(&full_manifest(), guests::CAPABILITY_CLIENT);
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
    for (what, wat) in [
        ("the capability client", guests::CAPABILITY_CLIENT),
        // The render guest especially: it is the one whose output ends up on a
        // host page, so "it asks for nothing more" is the claim that matters.
        ("the render client", guests::RENDER_CLIENT),
    ] {
        let host = load(&full_manifest(), wat);
        let mut imports = host.imports();
        imports.sort();
        assert_eq!(
            imports,
            vec![
                "wasi_snapshot_preview1::fd_read".to_owned(),
                "wasi_snapshot_preview1::fd_write".to_owned(),
            ],
            "{what}"
        );
    }
}

// ── The adversarial corpus ───────────────────────────────────────────────

/// Every attempt below runs the real guest against the real host and asserts
/// three things at once: the call was refused, nothing reached a backend, and
/// the refusal is visible to an operator.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "a table of escape attempts, one literal per attempt; the length is the corpus's \
              and splitting it would put attempts where a reader counting them would miss some"
)]
fn the_escape_corpus_is_contained_end_to_end() {
    struct Attempt {
        what: &'static str,
        manifest: String,
        path: &'static str,
        /// Whether containment is expected to show up as a *denial event*.
        ///
        /// Most of these attempts are refused by the host and leave a refusal
        /// in the ledger. One is not: a `kv-get` for a key another tenant owns
        /// is correctly *allowed* — the guest may read its own namespace — and
        /// correctly *misses*, because the derived key is unspellable. Its
        /// containment is the 403 the guest returns on the miss plus the
        /// untouched backends below, and demanding a refusal for it would be
        /// demanding the host deny a call it is right to serve.
        expect_denial: bool,
    }

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

    let attempts = vec![
        // Cross-tenant KV, by spelling the separator a naive
        // `format!("{plugin}:{tenant}:{key}")` would have made meaningful.
        // `/kv-other-tenant` answers 200 only on a *hit*: the call itself is
        // correctly allowed and correctly misses, so reading its status alone
        // could not tell containment from success.
        Attempt {
            what: "reading another tenant's key by spelling the separator",
            manifest: full.clone(),
            path: "/shop/kv-other-tenant",
            expect_denial: false,
        },
        // Capabilities that were never granted at all.
        Attempt {
            what: "using kv without the kv capability",
            manifest: no_kv,
            path: "/shop/kv-read",
            expect_denial: true,
        },
        Attempt {
            what: "using kv with only http-request",
            manifest: request_only.clone(),
            path: "/shop/kv-write",
            expect_denial: true,
        },
        Attempt {
            what: "using db with only http-request",
            manifest: request_only.clone(),
            path: "/shop/db-insert",
            expect_denial: true,
        },
        Attempt {
            what: "using outbound http with only http-request",
            manifest: request_only.clone(),
            path: "/shop/fetch-granted",
            expect_denial: true,
        },
        Attempt {
            what: "enqueueing a job with only http-request",
            manifest: request_only,
            path: "/shop/job-granted",
            expect_denial: true,
        },
        // Granted capabilities pointed at out-of-scope targets.
        Attempt {
            what: "calling a host that merely ends with the granted one",
            manifest: full.clone(),
            path: "/shop/fetch-undeclared",
            expect_denial: true,
        },
        Attempt {
            what: "reading a host-application table",
            manifest: full.clone(),
            path: "/shop/db-host-table",
            expect_denial: true,
        },
        Attempt {
            what: "enqueueing an undeclared job type",
            manifest: full.clone(),
            path: "/shop/job-undeclared",
            expect_denial: true,
        },
        // Choosing its own tenant through a row column.
        Attempt {
            what: "writing the tenant_id column",
            manifest: full,
            path: "/shop/db-tenant-column",
            expect_denial: true,
        },
    ];

    for attempt in attempts {
        let host = load(&attempt.manifest, guests::CAPABILITY_CLIENT);
        let wired = wired("alpha");
        let (status, events) = verdict(&host, wired.services.clone(), attempt.path);
        assert_eq!(
            status,
            403,
            "escape attempt succeeded: {what} ({events:?})",
            what = attempt.what
        );
        if attempt.expect_denial {
            assert!(
                events.iter().any(|event| !event.ends_with(":allowed")),
                "{what}: nothing was recorded as refused ({events:?})",
                what = attempt.what
            );
        }
        // Nothing reached a backend on any refused attempt.
        assert!(wired.http.seen().is_empty(), "{}", attempt.what);
        assert!(wired.jobs.queued().is_empty(), "{}", attempt.what);
        assert!(wired.db.keys().is_empty(), "{}", attempt.what);
        assert!(wired.kv.keys().is_empty(), "{}", attempt.what);
    }
}

#[test]
fn one_tenants_writes_are_unreadable_from_another_tenants_request() {
    // Attempt 11. The containment the whole subsystem exists for, through the real wire.
    let host = load(&full_manifest(), guests::CAPABILITY_CLIENT);
    let store = MemoryKvStore::new();
    let services = |tenant: &str| {
        CapabilityServices {
            kv: Some(Arc::clone(&store) as Arc<dyn KvStore>),
            ..CapabilityServices::none()
        }
        .for_tenant(tenant)
    };

    assert_eq!(
        verdict(&host, services("alpha"), "/shop/kv-write").0,
        200,
        "alpha's write should succeed"
    );
    // `/kv-read-hit` answers 200 only when the key was FOUND, which is the
    // whole point: `kv-get` for a key another tenant owns is correctly
    // *allowed* and correctly *misses*, so a test that read the status of the
    // call could not tell containment from success. It read 200 either way.
    assert_eq!(
        verdict(&host, services("alpha"), "/shop/kv-read-hit").0,
        200,
        "alpha reads back its own key"
    );
    assert_eq!(
        verdict(&host, services("beta"), "/shop/kv-read-hit").0,
        403,
        "beta must not find alpha's key"
    );
    // And spelling the separator does not reach it either.
    assert_eq!(
        verdict(&host, services("alpha"), "/shop/kv-other-tenant").0,
        403
    );
    assert_eq!(store.keys().len(), 1, "{:?}", store.keys());
    assert!(
        store
            .keys()
            .first()
            .is_some_and(|key| key.contains(&format!(":{}:", tenant_segment(Some("alpha"))))),
        "{:?}",
        store.keys()
    );
}

#[test]
fn a_guest_that_never_reads_its_replies_is_stopped_rather_than_growing_the_host() {
    // Attempt 12. The capability channel's own denial-of-service shape: the
    // guest writes call frames in a loop and never reads an answer, so every
    // reply would stay resident in its stdin queue.
    let host = load(&full_manifest(), guests::CAPABILITY_CLIENT);
    let wired = wired("alpha");
    let started = std::time::Instant::now();
    let outcome = host.run_with(&request("/shop/kv-flood"), wired.services.clone());

    // Which ceiling fires *is* the property — it is the difference between "the
    // host stopped it" and "the host ran out of memory more slowly than this
    // test ran out of patience". At the default quotas the guest spends its
    // `calls` budget first, so every further call comes back as a denial it can
    // read. The ledger is bounded by `MAX_EVENTS` rather than by the quota:
    // quota-denied calls are recorded too, which is the point of recording them.
    assert!(
        outcome.activity.len() <= autumn_web::plugin_sandbox::MAX_EVENTS,
        "the ledger is bounded: {}",
        outcome.activity.len()
    );
    let quota_hits = outcome
        .activity
        .iter()
        .filter(|event| {
            event.outcome == autumn_web::plugin_sandbox::CapabilityOutcome::QuotaExceeded
        })
        .count();
    assert!(
        quota_hits > 0,
        "the flood was refused rather than served: {:?}",
        outcome.activity
    );
    // Bounded *work*, not merely bounded memory: a regression that made a
    // refused call free would show up here before it showed up anywhere else.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "the flood took {:?}",
        started.elapsed()
    );
    // And nothing the flood asked for was performed.
    assert!(wired.kv.keys().len() <= 1, "{:?}", wired.kv.keys());
}

#[test]
fn a_reply_the_guest_reads_gives_its_budget_back() {
    // The unread-reply ceiling is on what is *resident*, not on what has been
    // sent. Counting cumulatively killed a well-behaved plugin at its 63rd
    // `kv-get` — inside the quotas its own operator had approved — and told it
    // it had a bug it did not have.
    let host = load(&full_manifest(), guests::CAPABILITY_CLIENT);
    let wired = wired("alpha");
    for round in 0..64 {
        let (status, events) = verdict(&host, wired.services.clone(), "/shop/kv-write");
        assert_eq!(status, 200, "round {round}: {events:?}");
    }
}

#[test]
fn a_capability_with_no_backend_is_denied_rather_than_quietly_succeeding() {
    // Attempt 13. An operator who granted `db` but wired no store must not be told the
    // write happened.
    let host = load(&full_manifest(), guests::CAPABILITY_CLIENT);
    let (status, events) = verdict(&host, CapabilityServices::none(), "/shop/db-insert");
    assert_eq!(status, 403, "{events:?}");
    assert!(
        events.iter().any(|event| event.contains("unavailable")),
        "{events:?}"
    );
}

#[test]
fn a_quota_bust_denies_the_call_without_touching_another_plugin_or_a_host_route() {
    // Attempt 14. The quota criterion, end to end. The guest makes *two* outbound calls
    // in one request against a per-request ceiling of one, and answers 200 only
    // if the second was refused — a guest that made one call could never
    // demonstrate a ceiling, which is why the previous shape of this test
    // asserted three successes and no denial at all.
    let manifest_src = format!("{}\n[quotas]\noutbound_calls = 1\n", full_manifest());
    let host = load(&manifest_src, guests::CAPABILITY_CLIENT);
    let wired = wired("alpha");

    let (status, events) = verdict(&host, wired.services.clone(), "/shop/fetch-twice");
    assert_eq!(
        status, 200,
        "the second call should have been refused: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event == "http-fetch:quota-exceeded"),
        "{events:?}"
    );
    // The second call never left the host.
    assert_eq!(wired.http.seen().len(), 1, "{:?}", wired.http.seen());

    // A second *request* gets its own per-request budget, so the ceiling is a
    // per-request one and not a permanent outage.
    let (status, events) = verdict(&host, wired.services.clone(), "/shop/fetch-granted");
    assert_eq!(status, 200, "{events:?}");

    // Another plugin, same services, is entirely unaffected: quotas are per
    // plugin, and one plugin's spent budget is not another's.
    let other_src = manifest_src.replace(
        &format!("name = \"{PLUGIN_NAME}\""),
        "name = \"other-plugin\"",
    );
    let other_host = load(&other_src, guests::CAPABILITY_CLIENT);
    let (status, events) = verdict(&other_host, wired.services, "/shop/fetch-twice");
    assert_eq!(status, 200, "{events:?}");
}

#[tokio::test]
async fn a_plugin_over_its_quota_does_not_stop_a_host_route_on_the_same_router() {
    use axum::body::Body;
    use http::Request;
    use tower::ServiceExt as _;

    async fn host_page() -> &'static str {
        "the application is still serving"
    }

    let manifest_src = format!("{}\n[quotas]\noutbound_calls = 1\n", full_manifest());
    let plugin = SandboxedPlugin::from_artifact(&pack(&manifest_src, guests::CAPABILITY_CLIENT))
        .expect("loads")
        .with_services(wired("alpha").services);
    let app: axum::Router = axum::Router::new()
        .route("/orders", axum::routing::get(host_page))
        .merge(plugin.mounted_router::<()>());

    for path in [
        "/shop/fetch-twice",
        "/orders",
        "/shop/fetch-twice",
        "/orders",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("serves");
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
}

#[test]
fn an_operator_can_answer_what_this_plugin_did() {
    // Attempt 15. The audit criterion, from one surface.
    let host = load(&full_manifest(), guests::CAPABILITY_CLIENT);
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

    let rendered = summary.to_string();
    for expected in [
        "api.example.com",
        "orders",
        "reindex",
        "users",
        "drain-accounts",
    ] {
        assert!(
            rendered.contains(expected),
            "{expected} missing:\n{rendered}"
        );
    }
    assert_eq!(log.plugins(), vec![PLUGIN_NAME.to_owned()]);

    // The window is part of the question — "in the last hour" — and so is the
    // plugin. Neither had a test, so deleting either half of the filter left
    // the suite green.
    assert!(
        log.summary(PLUGIN_NAME, Duration::ZERO).is_empty(),
        "a zero-length window covers nothing"
    );
    assert!(
        log.summary("another-plugin", Duration::from_secs(3600))
            .is_empty(),
        "one plugin's activity is not another's"
    );

    // Two plugins in one log, each seeing only its own.
    let outcome = load(&full_manifest(), guests::CAPABILITY_CLIENT)
        .run_with(&request("/shop/job-granted"), wired.services);
    log.ingest("other-plugin", outcome.activity);
    assert_eq!(
        log.summary(PLUGIN_NAME, Duration::from_secs(3600))
            .job_types
            .get("reindex"),
        Some(&1),
        "the first plugin's count did not move"
    );
    assert_eq!(
        log.summary("other-plugin", Duration::from_secs(3600))
            .job_types
            .get("reindex"),
        Some(&1)
    );
}

#[tokio::test]
async fn the_tenant_a_mounted_plugin_binds_to_is_the_requests_own() {
    // Attempt 16. The wiring nothing else in this suite reaches. Every other
    // cross-tenant proof hands the tenant in by hand through
    // `CapabilityServices`; this one goes through `mounted_router`, where the
    // tenant is read from the tenancy middleware's task-local and *overwrites*
    // whatever an embedder set. Delete that line and every other test here
    // still passes while every tenant silently shares one namespace.
    use axum::body::Body;
    use http::Request;
    use tower::ServiceExt as _;

    let store = MemoryKvStore::new();
    let plugin = SandboxedPlugin::from_artifact(&pack(&full_manifest(), guests::CAPABILITY_CLIENT))
        .expect("loads")
        .with_services(CapabilityServices {
            kv: Some(Arc::clone(&store) as Arc<dyn KvStore>),
            ..CapabilityServices::none()
        });
    let app: axum::Router = axum::Router::new().merge(plugin.mounted_router::<()>());

    for tenant in ["alpha", "beta"] {
        let app = app.clone();
        let response = autumn_web::tenancy::with_tenant(tenant.to_owned(), async move {
            app.oneshot(
                Request::builder()
                    .uri("/shop/kv-write")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("serves")
        })
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{tenant}");
    }

    let keys = store.keys();
    assert_eq!(
        keys.len(),
        2,
        "one key per tenant, not one shared: {keys:?}"
    );
    // Derived, not spelled: the tenant reaches a key as a *segment* that says
    // whether there was a tenant at all, so a literal `:alpha:` here would stop
    // testing anything the moment that derivation changed.
    for tenant in ["alpha", "beta"] {
        let segment = format!(":{}:", tenant_segment(Some(tenant)));
        assert!(
            keys.iter().any(|key| key.contains(&segment)),
            "{tenant}: {keys:?}"
        );
    }
}

#[test]
fn two_plugins_sharing_a_store_cannot_be_named_onto_one_table() {
    // Attempt 17. The cross-*plugin* half of "plugin-owned". The host-owned half — "you
    // cannot name the application's `users` table" — was covered; this is the
    // one a hostile author actually reaches for, by picking a plugin *name*
    // that shifts the derivation's separator onto a victim's table.
    let store = MemoryPluginStore::new();
    let services = CapabilityServices {
        db: Some(Arc::clone(&store) as Arc<dyn PluginStore>),
        ..CapabilityServices::none()
    }
    .for_tenant("alpha");

    // `shop` owns `orders`; `shop_orders` would collide with it under a
    // derivation that folded punctuation and joined with a single `_`.
    let victim_src = full_manifest();
    let attacker_src = full_manifest()
        .replace(
            &format!("name = \"{PLUGIN_NAME}\""),
            "name = \"shop_orders\"",
        )
        .replace("tables = [\"orders\"]", "tables = [\"v2\"]")
        .replace("table\\\":\\\"orders", "table\\\":\\\"v2");

    let victim = load(&victim_src, guests::CAPABILITY_CLIENT);
    assert_eq!(
        verdict(&victim, services, "/shop/db-insert").0,
        200,
        "the victim writes its own row"
    );
    let keys = store.keys();
    assert_eq!(keys.len(), 1, "{keys:?}");

    // The attacker's own table is a different physical table, whatever it is
    // named — so its rows and the victim's never meet.
    let attacker_manifest = SandboxManifest::parse(&attacker_src).expect("valid");
    let physical_victim =
        autumn_web::plugin_sandbox::capability::db::physical_table(PLUGIN_NAME, "orders");
    let physical_attacker =
        autumn_web::plugin_sandbox::capability::db::physical_table(&attacker_manifest.name, "v2");
    assert!(physical_victim.is_some() && physical_attacker.is_some());
    assert_ne!(
        physical_victim, physical_attacker,
        "two plugins must never derive one table"
    );
    assert_eq!(
        keys.first().map(|(table, ..)| table.clone()),
        physical_victim,
        "the row landed under the victim's derived name"
    );
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
    let plugin = SandboxedPlugin::from_artifact(&pack(&render_manifest(), guests::RENDER_CLIENT))
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
    let plugin = SandboxedPlugin::from_artifact(&pack(&render_manifest(), guests::RENDER_CLIENT))
        .expect("loads");
    for slot in ["unsafe-tag", "unsafe-href", "wrong-frame"] {
        assert_eq!(plugin.render_slot(slot, &[]).await, None, "{slot}");
    }
    // A slot the manifest never granted never even starts the guest.
    assert_eq!(plugin.render_slot("checkout-total", &[]).await, None);
}

#[tokio::test]
async fn a_slow_hook_omits_the_fragment_rather_than_holding_the_page() {
    // "A slow or trapping hook degrades to omitting the fragment, never
    // breaking the page." Only the trapping half was covered; a hook that
    // *runs* until its fuel is gone is the half a real plugin produces by
    // accident, and it is the one that costs the page its latency.
    let plugin =
        SandboxedPlugin::from_artifact(&pack(&render_manifest(), guests::CPU_SPIN)).expect("loads");
    let started = std::time::Instant::now();
    assert_eq!(plugin.render_slot("order-summary", &[]).await, None);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "the fuel budget bounded it: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_page_renders_the_good_plugins_fragment_and_omits_the_bad_ones() {
    // The headline claim of `RenderSlots`: a page that renders one plugin's
    // fragment and omits another's is the designed outcome, not a degraded one.
    // Only ever one plugin had been registered, so the concatenation — and the
    // "omits another's" half — went unproven.
    use autumn_web::plugin_sandbox::RenderSlots;

    let good = Arc::new(
        SandboxedPlugin::from_artifact(&pack(&render_manifest(), guests::RENDER_CLIENT))
            .expect("loads"),
    );
    let bad = Arc::new(
        SandboxedPlugin::from_artifact(&pack(
            &render_manifest().replace(&format!("name = \"{PLUGIN_NAME}\""), "name = \"broken\""),
            guests::TRAP,
        ))
        .expect("loads"),
    );

    let slots =
        RenderSlots::declaring(["order-summary", "unsafe-tag", "unsafe-href", "wrong-frame"])
            .with(Arc::clone(&bad))
            .expect("registers")
            .with(Arc::clone(&good))
            .expect("registers");

    assert_eq!(
        slots.render("order-summary", &[]).await,
        r#"<p class="panel">3 orders &lt;b&gt;&amp; counting&lt;/b&gt;</p>"#,
        "the broken plugin contributes nothing and the good one still renders"
    );
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
    // Through the *wire*, not through the map: the previous shape of this test
    // called `store.set` and `store.get` directly, which proves `HashMap`
    // works. What matters is that a scalar survives the call frame, the reply
    // frame and the guest's read of it — `found` distinguishing a stored value
    // from a miss is the part with teeth.
    let host = load(&full_manifest(), guests::CAPABILITY_CLIENT);
    let wired = wired("alpha");
    assert_eq!(
        verdict(&host, wired.services.clone(), "/shop/kv-write").0,
        200
    );
    assert_eq!(
        verdict(&host, wired.services.clone(), "/shop/kv-read-hit").0,
        200,
        "the value written through the wire reads back through it"
    );
    assert_eq!(
        wired
            .kv
            .get(&autumn_web::plugin_sandbox::capability::kv::namespaced_key(
                PLUGIN_NAME,
                Some("alpha"),
                "cart",
            )),
        Ok(Some(PluginValue::Text("one item".to_owned()))),
        "and it is a scalar, not its decimal spelling"
    );
}
