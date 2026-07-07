//! End-to-end tests for the offline sync engine: a real `SQLite`
//! `SyncStore` syncing through a real axum sync router backed by
//! `MemorySyncBackend`, bound on an ephemeral loopback port.

#![cfg(feature = "offline-sync")]

use std::sync::Arc;

use autumn_web::sync::{
    Change, ChangeOutcome, ConflictResolver, LwwResolver, MemorySyncBackend, Op, PullResponse,
    PushRequest, PushResponse, RemoteRow, Resolution, SyncBackend, SyncConfig, SyncEngine,
    SyncError, SyncStore, server,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Note {
    title: String,
}

fn note(title: &str) -> Note {
    Note {
        title: title.to_owned(),
    }
}

/// Serve the real sync router (nested under `/sync` like production) on an
/// ephemeral loopback port; returns the engine-facing base URL.
async fn start_sync_server(
    backend: Arc<dyn SyncBackend>,
    resolver: Arc<dyn ConflictResolver>,
) -> (String, tokio::task::JoinHandle<()>) {
    let router: axum::Router = axum::Router::new().nest("/sync", server::router(backend, resolver));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    (format!("http://{addr}/sync"), handle)
}

fn open_store(dir: &tempfile::TempDir, name: &str) -> SyncStore {
    SyncStore::open(dir.path().join(name)).expect("open store")
}

fn engine_for(store: &SyncStore, base_url: &str) -> SyncEngine {
    SyncEngine::new(store.clone(), SyncConfig::new(base_url))
}

fn backend_rows(backend: &dyn SyncBackend) -> Vec<RemoteRow> {
    match backend.pull_since(0, 10_000).expect("backend pull") {
        PullResponse::Ok { rows, .. } => rows,
        PullResponse::FullResyncRequired { .. } => panic!("unexpected full resync from cursor 0"),
    }
}

#[tokio::test]
async fn offline_writes_replay_to_server_on_sync() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(&dir, "a.db");

    // The app works fully offline: no server is running yet.
    store.put("notes", "n1", &note("one")).expect("put n1");
    store.put("notes", "n2", &note("two")).expect("put n2");
    store.put("notes", "n3", &note("three")).expect("put n3");
    store.delete("notes", "n3").expect("delete n3");
    assert_eq!(store.pending_count().expect("count"), 3);

    // Connection restored: the server comes up and one sync converges it.
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let report = engine_for(&store, &url).sync_once().await.expect("sync");

    assert_eq!(report.pushed, 3);
    assert!(!report.full_resync);
    assert_eq!(store.pending_count().expect("count"), 0, "journal drained");
    assert!(store.cursor().expect("cursor") > 0, "cursor advanced");

    let rows = backend_rows(backend.as_ref());
    assert_eq!(rows.len(), 3, "two live rows + one tombstone");
    let by_pk = |pk: &str| rows.iter().find(|r| r.pk == pk).expect("row");
    assert!(!by_pk("n1").deleted);
    assert_eq!(
        by_pk("n2")
            .payload
            .as_ref()
            .and_then(|p| p.get("title"))
            .and_then(|v| v.as_str()),
        Some("two")
    );
    assert!(by_pk("n3").deleted, "the offline delete became a tombstone");
}

#[tokio::test]
async fn push_retry_is_idempotent() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;

    let request = PushRequest {
        device_id: "device-a".to_owned(),
        changes: vec![Change {
            change_id: "11111111-1111-4111-8111-111111111111".to_owned(),
            collection: "notes".to_owned(),
            pk: "n1".to_owned(),
            op: Op::Upsert,
            payload: Some(json!({"title": "hello"})),
            base_version: 0,
            updated_at: Utc::now(),
        }],
    };

    let client = reqwest::Client::new();
    let push = |req: PushRequest| {
        let client = client.clone();
        let url = format!("{url}/push");
        async move {
            let response = client.post(url).json(&req).send().await.expect("send push");
            assert!(response.status().is_success(), "push should be 2xx");
            response
                .json::<PushResponse>()
                .await
                .expect("push response json")
        }
    };

    let first = push(request.clone()).await;
    assert!(
        matches!(first.outcomes.as_slice(), [ChangeOutcome::Applied { version }] if *version > 0),
        "first push applies: {first:?}"
    );
    let version_after_first = backend.latest_version().expect("latest");

    // Simulated lost response: the client re-sends the identical batch.
    let second = push(request).await;
    assert!(
        matches!(second.outcomes.as_slice(), [ChangeOutcome::AlreadyApplied]),
        "retry must dedup, got: {second:?}"
    );
    assert_eq!(
        backend.latest_version().expect("latest"),
        version_after_first,
        "retry must not re-apply"
    );
    assert_eq!(backend_rows(backend.as_ref()).len(), 1);
}

#[tokio::test]
async fn pull_applies_remote_changes_and_advances_cursor() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");

    // Device A creates two rows, then deletes one.
    let store_a = open_store(&dir, "a.db");
    let engine_a = engine_for(&store_a, &url);
    store_a
        .put("notes", "n1", &note("keep me"))
        .expect("put n1");
    store_a
        .put("notes", "n2", &note("delete me"))
        .expect("put n2");
    engine_a.sync_once().await.expect("a sync 1");
    store_a.delete("notes", "n2").expect("delete n2");
    engine_a.sync_once().await.expect("a sync 2");

    // Device B sees A's rows AND A's tombstone after one sync.
    let store_b = open_store(&dir, "b.db");
    let engine_b = engine_for(&store_b, &url);
    let report = engine_b.sync_once().await.expect("b sync");
    assert!(report.pulled >= 2);

    let n1: Option<Note> = store_b.get("notes", "n1").expect("get n1");
    assert_eq!(n1, Some(note("keep me")));
    let n2: Option<Note> = store_b.get("notes", "n2").expect("get n2");
    assert_eq!(n2, None, "remote tombstone must delete locally");
    let listed: Vec<(String, Note)> = store_b.list("notes").expect("list");
    assert_eq!(listed.len(), 1);

    assert_eq!(
        store_b.cursor().expect("cursor"),
        backend.latest_version().expect("latest"),
        "cursor lands on the newest server version"
    );
}

#[tokio::test]
async fn conflict_default_lww_converges_both_devices() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let store_a = open_store(&dir, "a.db");
    let store_b = open_store(&dir, "b.db");
    let engine_a = engine_for(&store_a, &url);
    let engine_b = engine_for(&store_b, &url);

    // Both devices share the row.
    store_a.put("notes", "n1", &note("base")).expect("seed");
    engine_a.sync_once().await.expect("a seed sync");
    engine_b.sync_once().await.expect("b seed sync");

    // Concurrent edits: B writes first, A writes later (the LWW winner).
    store_b
        .put("notes", "n1", &note("b-early"))
        .expect("b edit");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    store_a.put("notes", "n1", &note("a-late")).expect("a edit");

    engine_a.sync_once().await.expect("a push");
    engine_b.sync_once().await.expect("b push+resolve");

    // B (the loser) converged immediately via the push resolution.
    let b_view: Option<Note> = store_b.get("notes", "n1").expect("b get");
    assert_eq!(b_view, Some(note("a-late")), "LWW winner on loser device");
    assert_eq!(store_b.pending_count().expect("b pending"), 0);

    // A converges (idempotently) on its next pull of the resolved version.
    engine_a.sync_once().await.expect("a pull resolution");
    let a_view: Option<Note> = store_a.get("notes", "n1").expect("a get");
    assert_eq!(a_view, Some(note("a-late")), "LWW winner on winner device");

    let latest = backend.latest_version().expect("latest");
    assert_eq!(store_a.cursor().expect("a cursor"), latest);
    assert_eq!(store_b.cursor().expect("b cursor"), latest);
}

#[tokio::test]
async fn custom_conflict_resolver_merges_payloads() {
    /// Field-merge resolver: overlay the client's JSON object onto the
    /// server's — both sides' additions survive.
    struct FieldMergeResolver;
    impl ConflictResolver for FieldMergeResolver {
        fn resolve(
            &self,
            _client_device_id: &str,
            client: &Change,
            server: &RemoteRow,
        ) -> Resolution {
            let mut merged = server.payload.clone().unwrap_or_else(|| json!({}));
            if let (Some(target), Some(source)) = (
                merged.as_object_mut(),
                client
                    .payload
                    .as_ref()
                    .and_then(serde_json::Value::as_object),
            ) {
                for (key, value) in source {
                    target.insert(key.clone(), value.clone());
                }
            }
            Resolution::Merge(merged)
        }
    }

    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(FieldMergeResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let store_a = open_store(&dir, "a.db");
    let store_b = open_store(&dir, "b.db");
    let engine_a = engine_for(&store_a, &url);
    let engine_b = engine_for(&store_b, &url);

    store_a
        .put("docs", "d1", &json!({"base": true}))
        .expect("seed");
    engine_a.sync_once().await.expect("a seed sync");
    engine_b.sync_once().await.expect("b seed sync");

    // Divergent edits touching different fields.
    store_a
        .put("docs", "d1", &json!({"base": true, "from_a": 1}))
        .expect("a edit");
    store_b
        .put("docs", "d1", &json!({"base": true, "from_b": 2}))
        .expect("b edit");

    engine_a.sync_once().await.expect("a push");
    engine_b.sync_once().await.expect("b push+merge");
    engine_a.sync_once().await.expect("a pull merge");

    let expect_merged = |value: Option<serde_json::Value>, device: &str| {
        let value = value.unwrap_or_else(|| panic!("{device} row missing"));
        assert_eq!(
            value.get("from_a"),
            Some(&json!(1)),
            "{device} kept A's field"
        );
        assert_eq!(
            value.get("from_b"),
            Some(&json!(2)),
            "{device} kept B's field"
        );
    };
    expect_merged(store_a.get("docs", "d1").expect("a get"), "device A");
    expect_merged(store_b.get("docs", "d1").expect("b get"), "device B");

    // The merged row exists server-side with a new version.
    let rows = backend_rows(backend.as_ref());
    let row = rows.iter().find(|r| r.pk == "d1").expect("server row");
    assert_eq!(
        row.payload.as_ref().and_then(|p| p.get("from_b")),
        Some(&json!(2))
    );
}

#[tokio::test]
async fn gc_horizon_forces_full_resync_preserving_pending() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let store_a = open_store(&dir, "a.db");
    let engine_a = engine_for(&store_a, &url);
    let store_b = open_store(&dir, "b.db");
    let engine_b = engine_for(&store_b, &url);

    // A creates a row; B syncs it (B's cursor is now > 0 but will go stale).
    store_a
        .put("notes", "one", &note("first"))
        .expect("put one");
    engine_a.sync_once().await.expect("a sync 1");
    engine_b.sync_once().await.expect("b sync 1");
    let stale_cursor = store_b.cursor().expect("b cursor");
    assert!(stale_cursor > 0);

    // A deletes "one" and adds "two"; then the server GCs tombstones.
    store_a.delete("notes", "one").expect("delete one");
    store_a
        .put("notes", "two", &note("second"))
        .expect("put two");
    engine_a.sync_once().await.expect("a sync 2");
    let latest = backend.latest_version().expect("latest");
    let removed = backend.gc_tombstones(latest).expect("gc");
    assert_eq!(removed, 1, "the tombstone was physically dropped");
    assert!(store_b.cursor().expect("b cursor") < backend.tombstone_horizon().expect("horizon"));

    // B queues an offline write BEFORE discovering it needs a full resync.
    store_b
        .put("notes", "b-note", &note("from b"))
        .expect("b put");

    let report = engine_b.sync_once().await.expect("b resync");
    assert!(
        report.full_resync,
        "stale cursor must trigger a full resync"
    );
    assert_eq!(
        store_b.pending_count().expect("b pending"),
        0,
        "pending replayed"
    );

    // B converged on post-GC reality, and its own write survived the resync.
    let listed: Vec<(String, Note)> = store_b.list("notes").expect("b list");
    let pks: Vec<&str> = listed.iter().map(|(pk, _)| pk.as_str()).collect();
    assert!(pks.contains(&"two"), "b has A's newer row: {pks:?}");
    assert!(
        pks.contains(&"b-note"),
        "b's own pending write survived: {pks:?}"
    );
    assert!(
        !pks.contains(&"one"),
        "the GC'd delete stays deleted: {pks:?}"
    );

    // ... and the server received B's preserved pending change.
    let rows = backend_rows(backend.as_ref());
    assert!(rows.iter().any(|r| r.pk == "b-note" && !r.deleted));
}

#[tokio::test]
async fn sync_once_fails_cleanly_when_server_unreachable() {
    // Reserve a loopback port, then close it: guaranteed-unreachable URL.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);

    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(&dir, "a.db");
    store
        .put("notes", "n1", &note("offline write"))
        .expect("put");

    let engine = engine_for(&store, &format!("http://{addr}/sync"));
    let err = engine.sync_once().await.expect_err("server is down");
    assert!(
        matches!(err, SyncError::Transport(_)),
        "unreachable server is a transport error, got: {err:?}"
    );

    // Offline capability under failure: nothing was lost or corrupted.
    assert_eq!(store.pending_count().expect("pending"), 1, "journal intact");
    let got: Option<Note> = store.get("notes", "n1").expect("get");
    assert_eq!(got, Some(note("offline write")), "local reads still work");
    assert_eq!(store.cursor().expect("cursor"), 0);
}
