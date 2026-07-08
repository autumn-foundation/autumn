//! End-to-end tests for the offline sync engine: a real `SQLite`
//! `SyncStore` syncing through a real axum sync router backed by
//! `MemorySyncBackend`, bound on an ephemeral loopback port.

#![cfg(feature = "offline-sync")]

use std::sync::Arc;

use autumn_web::sync::{
    Change, ChangeOutcome, ConflictResolver, LwwResolver, MAX_PUSH_CHANGES, MemorySyncBackend, Op,
    PullResponse, PushRequest, PushResponse, RemoteRow, Resolution, SyncBackend, SyncConfig,
    SyncEngine, SyncError, SyncScope, SyncStore, Version, server,
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
    match backend
        .pull_since(SyncScope::GLOBAL, 0, 10_000, 0)
        .expect("backend pull")
    {
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
    // The dedup outcome must echo the version assigned on first apply so
    // the client can record the ack it never received.
    let ChangeOutcome::Applied {
        version: first_version,
    } = first.outcomes[0]
    else {
        unreachable!("asserted Applied above");
    };
    let second = push(request).await;
    assert!(
        matches!(
            second.outcomes.as_slice(),
            [ChangeOutcome::AlreadyApplied { version }] if *version == first_version
        ),
        "retry must dedup and echo the original version {first_version}, got: {second:?}"
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

/// A device offline past a tombstone GC still holds a pending edit of the
/// deleted row — and the engine pushes pending changes BEFORE the pull that
/// would demand a full resync. The server must settle that edit with a
/// deterministic server-winning tombstone (the resolver gets no say on
/// GC'd-tombstone conflicts), the engine must converge on the delete
/// without losing other data, and the pass after that must be steady (no
/// resync loop).
#[tokio::test]
async fn offline_edit_of_a_gcd_row_does_not_resurrect_it() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let store_a = open_store(&dir, "a.db");
    let engine_a = engine_for(&store_a, &url);
    let store_b = open_store(&dir, "b.db");
    let engine_b = engine_for(&store_b, &url);

    // A creates "doomed" and "keep"; B syncs both.
    store_a
        .put("notes", "doomed", &note("original"))
        .expect("put");
    store_a.put("notes", "keep", &note("kept")).expect("put");
    engine_a.sync_once().await.expect("a sync 1");
    engine_b.sync_once().await.expect("b sync 1");

    // B goes offline and edits "doomed" (pending, based on its synced
    // version). Meanwhile A deletes it and the server GCs the tombstone.
    store_b
        .put("notes", "doomed", &note("offline edit"))
        .expect("b offline edit");
    store_a.delete("notes", "doomed").expect("a delete");
    engine_a.sync_once().await.expect("a sync 2");
    let removed = backend
        .gc_tombstones(backend.latest_version().expect("latest"))
        .expect("gc");
    assert_eq!(removed, 1, "the tombstone was physically dropped");

    // B reconnects. The push happens first: the server must NOT recreate
    // the row; B converges on the deletion (and full-resyncs its stale
    // cursor afterwards).
    engine_b.sync_once().await.expect("b sync 2");
    let listed: Vec<(String, Note)> = store_b.list("notes").expect("b list");
    let pks: Vec<&str> = listed.iter().map(|(pk, _)| pk.as_str()).collect();
    assert!(
        !pks.contains(&"doomed"),
        "the GC'd deletion must win over the offline edit: {pks:?}"
    );
    assert!(pks.contains(&"keep"), "unrelated rows survive: {pks:?}");
    assert_eq!(
        store_b.pending_count().expect("b pending"),
        0,
        "the pending edit must be settled (resolved), not lost silently or stuck"
    );
    assert!(
        backend_rows(backend.as_ref())
            .iter()
            .all(|r| r.pk != "doomed" || r.deleted),
        "the server must not hold a live resurrected row"
    );

    // Steady state: the next pass neither resyncs nor re-pushes anything.
    let next = engine_b.sync_once().await.expect("b steady");
    assert!(!next.full_resync, "the stale-push handling must not loop");
    assert_eq!(next.pushed, 0);
}

/// End-to-end regression for the null-payload collapse: `put(&None::<T>)`
/// journals the JSON document `null`; the old wire encoding collapsed it
/// into an omitted payload, so the very first push was rejected with 422
/// and the client's queue was bricked (it retried the same rejected batch
/// forever). The document must push, pull, and materialize intact.
#[tokio::test]
async fn null_documents_sync_end_to_end() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let store_a = open_store(&dir, "a.db");
    let engine_a = engine_for(&store_a, &url);
    let store_b = open_store(&dir, "b.db");
    let engine_b = engine_for(&store_b, &url);

    store_a.put("notes", "opt", &None::<String>).expect("put");
    let report = engine_a
        .sync_once()
        .await
        .expect("a null document must push cleanly, not 422-brick the queue");
    assert_eq!(report.pushed, 1);
    assert_eq!(store_a.pending_count().expect("pending"), 0);

    engine_b.sync_once().await.expect("b sync");
    let value: Option<Option<String>> = store_b.get("notes", "opt").expect("get");
    assert_eq!(
        value,
        Some(None),
        "the null document must materialize as a PRESENT row holding None"
    );
    let listed: Vec<(String, serde_json::Value)> = store_b.list("notes").expect("list");
    assert!(
        listed.iter().any(|(pk, v)| pk == "opt" && v.is_null()),
        "the row must be visible, not conflated with a tombstone: {listed:?}"
    );
}

/// Regression for the lost-response retry downgrade: B's edit conflicts
/// with A's newer delete, the server resolves `KeepServer` (tombstone) but
/// the response is lost. The retry must REPLAY the original Resolved
/// tombstone — under the old `AlreadyApplied` downgrade the client recorded
/// the resolved version as a clean ack, kept its losing payload visible,
/// and its next edit could clean-apply over the resolution and resurrect
/// the deleted row.
#[tokio::test]
async fn lost_resolved_response_retry_converges_without_resurrection() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use axum::middleware::Next;
    use axum::response::IntoResponse;

    // Loses exactly ONE push response AFTER the backend has applied it —
    // the client sees a transport-style failure while the server state
    // (and its dedup record) already exists.
    static LOSE_NEXT_PUSH_RESPONSE: AtomicBool = AtomicBool::new(false);
    async fn lossy_push(request: axum::extract::Request, next: Next) -> axum::response::Response {
        let is_push = request.uri().path().ends_with("/push");
        let response = next.run(request).await;
        if is_push && LOSE_NEXT_PUSH_RESPONSE.swap(false, Ordering::SeqCst) {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        response
    }
    LOSE_NEXT_PUSH_RESPONSE.store(false, Ordering::SeqCst);

    let backend = Arc::new(MemorySyncBackend::new());
    let lossy: axum::Router = axum::Router::new().nest(
        "/sync",
        server::router(backend.clone(), Arc::new(LwwResolver))
            .layer(axum::middleware::from_fn(lossy_push)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let _srv = tokio::spawn(async move {
        axum::serve(listener, lossy).await.expect("serve");
    });
    let url = format!("http://{addr}/sync");
    let dir = tempfile::tempdir().expect("tempdir");

    // A creates "victim"; B syncs it. B edits it offline, then A deletes it
    // with a NEWER stamp — B's push will lose the LWW conflict.
    let store_a = open_store(&dir, "a.db");
    let engine_a = engine_for(&store_a, &url);
    let store_b = open_store(&dir, "b.db");
    let engine_b = engine_for(&store_b, &url);
    store_a
        .put("notes", "victim", &note("original"))
        .expect("put");
    engine_a.sync_once().await.expect("a sync 1");
    engine_b.sync_once().await.expect("b sync 1");
    store_b
        .put("notes", "victim", &note("losing edit"))
        .expect("b edit");
    store_a.delete("notes", "victim").expect("a delete");
    engine_a.sync_once().await.expect("a sync 2");

    // B's push is applied server-side (Resolved: the deletion wins) but the
    // response is lost — B's journal entry and losing payload survive.
    LOSE_NEXT_PUSH_RESPONSE.store(true, Ordering::SeqCst);
    let err = engine_b.sync_once().await.expect_err("response is lost");
    assert!(matches!(err, SyncError::Server(_)), "got {err:?}");
    assert_eq!(
        store_b.pending_count().expect("pending"),
        1,
        "the unconfirmed change stays journaled for the retry"
    );

    // The retry replays the original Resolved tombstone: B converges on the
    // deletion instead of recording a clean ack over its losing payload.
    engine_b.sync_once().await.expect("retry");
    assert_eq!(store_b.pending_count().expect("pending"), 0);
    let listed: Vec<(String, Note)> = store_b.list("notes").expect("b list");
    assert!(
        listed.iter().all(|(pk, _)| pk != "victim"),
        "B must converge on the server-winning delete: {listed:?}"
    );
    assert!(
        backend_rows(backend.as_ref())
            .iter()
            .all(|r| r.pk != "victim" || r.deleted),
        "the server must not hold a resurrected row"
    );

    // And a NEW local write afterwards is a deliberate re-create that
    // settles cleanly — no false conflicts from the replayed ack.
    store_b
        .put("notes", "victim", &note("recreated knowingly"))
        .expect("recreate");
    let report = engine_b.sync_once().await.expect("recreate sync");
    assert_eq!(report.pushed, 1);
    assert!(!report.full_resync, "no resync loop");
    assert!(
        backend_rows(backend.as_ref())
            .iter()
            .any(|r| r.pk == "victim" && !r.deleted),
        "a post-convergence re-create is a legitimate new write"
    );
}

/// Regression for the server-demanded resync destroying local data when
/// the replacement pull fails: the `FullResyncRequired` branch used to clear
/// synced rows BEFORE fetching the replacement snapshot, so a connection
/// lost right after the demand left the store emptied — including rows
/// acked by the push moments earlier. The resync must fetch the complete
/// snapshot first and reconcile only after it arrives: every failure
/// leaves the pre-resync state intact (stale cursor re-triggers the
/// demand), and connectivity's return converges with zero loss.
#[tokio::test]
async fn server_demanded_resync_never_drops_rows_while_snapshot_pull_fails() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use axum::middleware::Next;

    // Snapshot pulls are the ones with session=0 (the resync); normal
    // catch-up pulls (session=<stale cursor>) keep working so the server
    // can DEMAND the resync in the first place.
    static SNAPSHOT_UP: AtomicBool = AtomicBool::new(false);
    async fn flaky_snapshot_pull(
        request: axum::extract::Request,
        next: Next,
    ) -> Result<axum::response::Response, axum::http::StatusCode> {
        let is_snapshot = request.uri().path().ends_with("/pull")
            && request
                .uri()
                .query()
                .is_some_and(|q| q.contains("session=0"));
        if is_snapshot && !SNAPSHOT_UP.load(Ordering::SeqCst) {
            return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE);
        }
        Ok(next.run(request).await)
    }
    // Fresh devices legitimately pull with session=0 too, so keep the
    // snapshot route up during seeding and cut it right before the resync.
    SNAPSHOT_UP.store(true, Ordering::SeqCst);

    let backend = Arc::new(MemorySyncBackend::new());
    let flaky: axum::Router = axum::Router::new().nest(
        "/sync",
        server::router(backend.clone(), Arc::new(LwwResolver))
            .layer(axum::middleware::from_fn(flaky_snapshot_pull)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let _srv = tokio::spawn(async move {
        axum::serve(listener, flaky).await.expect("serve");
    });
    let url = format!("http://{addr}/sync");
    let dir = tempfile::tempdir().expect("tempdir");

    // A creates a row; B syncs it (cursor > 0), then goes stale: A deletes
    // "one", adds "two", and the server GCs the tombstone past B's cursor.
    let store_a = open_store(&dir, "a.db");
    let engine_a = engine_for(&store_a, &url);
    let store_b = open_store(&dir, "b.db");
    let engine_b = engine_for(&store_b, &url);
    store_a.put("notes", "one", &note("first")).expect("put");
    engine_a.sync_once().await.expect("a sync 1");
    engine_b.sync_once().await.expect("b sync 1");
    let stale_cursor = store_b.cursor().expect("b cursor");
    assert!(stale_cursor > 0);
    store_a.delete("notes", "one").expect("delete");
    store_a.put("notes", "two", &note("second")).expect("put");
    engine_a.sync_once().await.expect("a sync 2");
    backend
        .gc_tombstones(backend.latest_version().expect("latest"))
        .expect("gc");

    // The demand is set up; now cut the snapshot route.
    SNAPSHOT_UP.store(false, Ordering::SeqCst);

    // B writes locally, then syncs: the push acks "b-note", the catch-up
    // pull is answered with `FullResyncRequired`, and the replacement
    // snapshot pull 503s. Repeatedly. Every failure must leave B's rows —
    // including the JUST-ACKED one — and its cursor untouched.
    store_b
        .put("notes", "b-note", &note("from b"))
        .expect("put");
    for attempt in 0..3 {
        let err = engine_b.sync_once().await.expect_err("snapshot is down");
        assert!(matches!(err, SyncError::Server(_)), "got {err:?}");
        let listed: Vec<(String, Note)> = store_b.list("notes").expect("list");
        let pks: Vec<&str> = listed.iter().map(|(pk, _)| pk.as_str()).collect();
        assert!(
            pks.contains(&"b-note"),
            "the just-acked row must survive failed resync #{attempt}: {pks:?}"
        );
        assert!(
            pks.contains(&"one"),
            "pre-resync rows must survive failed resync #{attempt}: {pks:?}"
        );
        assert_eq!(
            store_b.cursor().expect("b cursor"),
            stale_cursor,
            "a failed resync must leave the stale cursor so the server \
             demands it again"
        );
        assert_eq!(
            store_b.pending_count().expect("pending"),
            0,
            "the push before the demand acked the local write"
        );
    }

    // Connectivity returns: the resync completes and B converges.
    SNAPSHOT_UP.store(true, Ordering::SeqCst);
    let report = engine_b.sync_once().await.expect("resync completes");
    assert!(report.full_resync, "the demanded resync ran");
    let listed: Vec<(String, Note)> = store_b.list("notes").expect("list");
    let pks: Vec<&str> = listed.iter().map(|(pk, _)| pk.as_str()).collect();
    assert!(!pks.contains(&"one"), "the GC'd delete lands: {pks:?}");
    assert!(pks.contains(&"two"), "A's newer row arrives: {pks:?}");
    assert!(pks.contains(&"b-note"), "B's own row survives: {pks:?}");
    assert!(
        store_b.cursor().expect("b cursor") >= backend.tombstone_horizon().expect("horizon"),
        "the reconciled cursor lands at/above the horizon"
    );

    // Steady state afterwards.
    let next = engine_b.sync_once().await.expect("steady");
    assert!(!next.full_resync, "the resync must not loop");
}

/// Regression for the heal path destroying acknowledged local data while
/// offline: a fresh device's first sync pushes fine (journal cleared,
/// server versions recorded, cursor still 0), then the radio drops before
/// the pull. Every subsequent OFFLINE retry enters the zero-cursor heal —
/// which used to clear the acked rows up front, before any server contact,
/// deleting local data that only existed server-side. The heal must fetch
/// the full replacement snapshot FIRST and only then reconcile: transport
/// failures leave local state untouched, and connectivity's return
/// converges with zero loss.
#[tokio::test]
async fn heal_never_drops_acked_rows_while_pull_keeps_failing() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use axum::middleware::Next;

    // A server whose /pull can be switched off: pushes always succeed,
    // pulls return 503 until the flag flips (a radio drop between the
    // push and the pull of one pass, persisting across retries).
    static PULL_UP: AtomicBool = AtomicBool::new(false);
    async fn flaky_pull(
        request: axum::extract::Request,
        next: Next,
    ) -> Result<axum::response::Response, axum::http::StatusCode> {
        if request.uri().path().ends_with("/pull") && !PULL_UP.load(Ordering::SeqCst) {
            return Err(axum::http::StatusCode::SERVICE_UNAVAILABLE);
        }
        Ok(next.run(request).await)
    }
    PULL_UP.store(false, Ordering::SeqCst);

    let backend = Arc::new(MemorySyncBackend::new());
    // Another device's row is already on the server, so convergence has
    // something to pull besides our own writes.
    let seed = PushRequest {
        device_id: "device-a".to_owned(),
        changes: vec![Change {
            change_id: "00000000-0000-4000-8000-0000000000aa".to_owned(),
            collection: "notes".to_owned(),
            pk: "other".to_owned(),
            op: Op::Upsert,
            payload: Some(json!({"title": "from a"})),
            base_version: 0,
            updated_at: Utc::now(),
        }],
    };
    backend
        .apply_push(SyncScope::GLOBAL, &seed, &LwwResolver)
        .expect("seed other device's row");

    let flaky: axum::Router = axum::Router::new().nest(
        "/sync",
        server::router(backend.clone(), Arc::new(LwwResolver))
            .layer(axum::middleware::from_fn(flaky_pull)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let _srv = tokio::spawn(async move {
        axum::serve(listener, flaky).await.expect("serve");
    });
    let url = format!("http://{addr}/sync");

    let dir = tempfile::tempdir().expect("tempdir");
    let store_b = open_store(&dir, "b.db");
    let engine_b = engine_for(&store_b, &url);
    store_b.put("notes", "mine", &note("acked")).expect("put");

    // First sync: push succeeds, pull fails.
    let err = engine_b.sync_once().await.expect_err("pull is down");
    assert!(matches!(err, SyncError::Server(_)), "got {err:?}");
    assert_eq!(store_b.pending_count().expect("pending"), 0, "push acked");
    assert_eq!(store_b.cursor().expect("cursor"), 0, "pull never completed");

    // Offline retries: each pass enters the zero-cursor heal (synced rows,
    // cursor 0) and fails at the pull — the acked row must survive every
    // single one.
    for attempt in 0..3 {
        let mine: Option<Note> = store_b.get("notes", "mine").expect("get");
        assert!(
            mine.is_some(),
            "acked local data must survive offline retry #{attempt}"
        );
        let err = engine_b.sync_once().await.expect_err("still offline");
        assert!(matches!(err, SyncError::Server(_)), "got {err:?}");
        assert_eq!(
            store_b.cursor().expect("cursor"),
            0,
            "a failed heal must leave the cursor at 0 so it re-runs"
        );
    }
    assert!(
        store_b.get::<Note>("notes", "mine").expect("get").is_some(),
        "acked local data must still be present after every offline retry"
    );

    // Connectivity returns: the heal completes and everything converges.
    PULL_UP.store(true, Ordering::SeqCst);
    let report = engine_b.sync_once().await.expect("heal completes");
    assert!(report.full_resync, "the zero-cursor heal ran");
    let listed: Vec<(String, Note)> = store_b.list("notes").expect("list");
    let pks: Vec<&str> = listed.iter().map(|(pk, _)| pk.as_str()).collect();
    assert!(pks.contains(&"mine"), "own acked row survives: {pks:?}");
    assert!(
        pks.contains(&"other"),
        "the other device's row arrives: {pks:?}"
    );
    assert!(store_b.cursor().expect("cursor") > 0, "cursor landed");

    // Steady state afterwards.
    let next = engine_b.sync_once().await.expect("steady");
    assert!(!next.full_resync, "the heal must not loop");
}

/// Regression for the "synced rows but cursor 0" crash state: a crash
/// between materializing pulled rows and persisting the cursor (possible
/// before the two became one transaction, or via a store written by an
/// older build) leaves a device that LOOKS fresh to the server — its
/// session=0 pull is exempt from the tombstone-horizon check — so
/// deletions GC'd in the meantime would never arrive and the stale rows
/// would stay visible forever while the cursor advances past them. The
/// engine must detect the state and heal it with a full resync.
#[tokio::test]
async fn synced_rows_with_zero_cursor_heal_via_full_resync() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let store_a = open_store(&dir, "a.db");
    let engine_a = engine_for(&store_a, &url);
    let store_b = open_store(&dir, "b.db");
    let engine_b = engine_for(&store_b, &url);

    // A creates two rows; B syncs them.
    store_a.put("notes", "keep", &note("kept")).expect("put");
    store_a.put("notes", "gone", &note("doomed")).expect("put");
    engine_a.sync_once().await.expect("a sync 1");
    engine_b.sync_once().await.expect("b sync 1");
    assert!(store_b.cursor().expect("b cursor") > 0);

    // Simulate the crash state on B: rows are materialized but the cursor
    // was never persisted. (Today's apply path persists both atomically, so
    // reach into the SQLite file the way an older build's crash would have
    // left it.)
    {
        use diesel::prelude::*;
        let path = dir.path().join("b.db");
        let mut conn =
            diesel::sqlite::SqliteConnection::establish(path.to_str().expect("utf-8 path"))
                .expect("open raw sqlite");
        diesel::sql_query("UPDATE autumn_sync_state SET value = '0' WHERE key = 'cursor'")
            .execute(&mut conn)
            .expect("reset cursor");
    }
    assert_eq!(store_b.cursor().expect("b cursor"), 0);

    // Meanwhile the row is deleted remotely and the tombstone is GC'd —
    // no future pull can ever carry the deletion again.
    store_a.delete("notes", "gone").expect("delete");
    engine_a.sync_once().await.expect("a sync 2");
    let removed = backend
        .gc_tombstones(backend.latest_version().expect("latest"))
        .expect("gc");
    assert_eq!(removed, 1, "the tombstone was physically dropped");

    // B's next pass must NOT slip through the session=0 exemption with its
    // stale rows: the engine detects synced-rows-at-cursor-0 and resyncs.
    let report = engine_b.sync_once().await.expect("b heal");
    assert!(
        report.full_resync,
        "synced rows with a zero cursor must trigger a full resync"
    );
    let listed: Vec<(String, Note)> = store_b.list("notes").expect("b list");
    let pks: Vec<&str> = listed.iter().map(|(pk, _)| pk.as_str()).collect();
    assert!(
        !pks.contains(&"gone"),
        "the GC'd deletion must reach B via the resync: {pks:?}"
    );
    assert!(pks.contains(&"keep"), "live rows survive the heal: {pks:?}");
    assert!(
        store_b.cursor().expect("b cursor") >= backend.tombstone_horizon().expect("horizon"),
        "the healed cursor must land at/above the horizon"
    );

    // Steady state afterwards: no resync loop.
    let next = engine_b.sync_once().await.expect("b steady");
    assert!(!next.full_resync, "the heal must not loop");
}

/// Regression for the perpetual post-GC full-resync loop: GC naturally
/// leaves the horizon ABOVE the newest surviving row version whenever the
/// most recent change was a delete. A completed catch-up must land the
/// cursor at/above the horizon so subsequent passes are normal — not
/// re-download the world every 30 s forever.
#[tokio::test]
async fn post_gc_steady_state_does_not_loop_full_resyncs() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");

    // Server ends up: one@1 live; two created@2, deleted@3; gc(3) → horizon
    // 3 with max surviving row version 1.
    let store_a = open_store(&dir, "a.db");
    let engine_a = engine_for(&store_a, &url);
    store_a.put("notes", "one", &note("keep")).expect("put one");
    store_a
        .put("notes", "two", &note("doomed"))
        .expect("put two");
    engine_a.sync_once().await.expect("a sync 1");
    store_a.delete("notes", "two").expect("delete two");
    engine_a.sync_once().await.expect("a sync 2");
    let latest = backend.latest_version().expect("latest");
    backend.gc_tombstones(latest).expect("gc");
    let horizon = backend.tombstone_horizon().expect("horizon");
    assert!(
        horizon > 1,
        "precondition: horizon sits above the surviving row version"
    );

    // A fresh device syncs cleanly and reaches a steady state.
    let store_b = open_store(&dir, "b.db");
    let engine_b = engine_for(&store_b, &url);
    let first = engine_b.sync_once().await.expect("b first sync");
    assert!(!first.full_resync, "a fresh device never needs a resync");
    assert!(
        store_b.cursor().expect("cursor") >= horizon,
        "a completed catch-up must land the cursor at/above the horizon"
    );

    // Steady state: no pass after the first may demand a full resync.
    for pass in 0..5 {
        let report = engine_b.sync_once().await.expect("steady-state pass");
        assert!(
            !report.full_resync,
            "pass {pass} looped back into a full resync"
        );
        assert_eq!(report.pulled, 0, "pass {pass} re-downloaded data");
    }
    let n1: Option<Note> = store_b.get("notes", "one").expect("get");
    assert_eq!(n1, Some(note("keep")));
}

/// Regression for the mid-pagination full-resync trap: after any GC, live
/// rows below the horizon are the norm. A fresh device whose first sync
/// needs more than one page must be able to complete it — its intermediate
/// page cursors are below the horizon, but its session started from 0.
#[tokio::test]
async fn multi_page_initial_sync_completes_after_gc() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");

    // Server: a@1, b@2 live; c created@3, deleted@4; gc(4) → horizon 4.
    let store_a = open_store(&dir, "a.db");
    let engine_a = engine_for(&store_a, &url);
    store_a.put("notes", "a", &note("first")).expect("put a");
    store_a.put("notes", "b", &note("second")).expect("put b");
    store_a.put("notes", "c", &note("doomed")).expect("put c");
    engine_a.sync_once().await.expect("a sync 1");
    store_a.delete("notes", "c").expect("delete c");
    engine_a.sync_once().await.expect("a sync 2");
    let latest = backend.latest_version().expect("latest");
    backend.gc_tombstones(latest).expect("gc");

    // Fresh device with a 1-row page size: two live rows below the horizon
    // force multi-page pagination through sub-horizon cursors.
    let store_b = open_store(&dir, "b.db");
    let mut config = SyncConfig::new(&url);
    config.pull_batch_size = 1;
    let engine_b = SyncEngine::new(store_b.clone(), config);

    let report = engine_b
        .sync_once()
        .await
        .expect("multi-page initial sync must complete");
    assert!(!report.full_resync, "a fresh device never needs a resync");
    assert_eq!(
        store_b.get::<Note>("notes", "a").expect("get a"),
        Some(note("first"))
    );
    assert_eq!(
        store_b.get::<Note>("notes", "b").expect("get b"),
        Some(note("second")),
        "every live page must arrive, including those past the first"
    );
    assert!(store_b.cursor().expect("cursor") >= latest);

    // And the steady state holds afterwards.
    let next = engine_b.sync_once().await.expect("steady-state pass");
    assert!(!next.full_resync);
}

/// Test double for the from-zero GC race: a backend that runs a
/// delete-plus-`gc_tombstones` (or a horizon-advancing churn cycle)
/// immediately BEFORE chosen pull calls, deterministically simulating a
/// tombstone GC committing between two pages of one from-zero
/// catch-up/snapshot. From-zero sessions are exempt from the server's
/// horizon check, so without the client-side per-page horizon guard an
/// early page's live row whose delete is GC'd before a later page would
/// stay visible forever while the cursor lands past the new horizon.
struct GcInjectingBackend {
    inner: MemorySyncBackend,
    /// pk to delete + GC right before the Nth `pull_since` call (1-based).
    delete_before_pull: std::sync::Mutex<std::collections::HashMap<usize, &'static str>>,
    /// While true, every pull after the first is preceded by a
    /// create+delete+GC churn cycle, so the horizon moves on every page.
    churn: std::sync::atomic::AtomicBool,
    pulls: std::sync::atomic::AtomicUsize,
}

impl GcInjectingBackend {
    fn new(inner: MemorySyncBackend) -> Self {
        Self {
            inner,
            delete_before_pull: std::sync::Mutex::new(std::collections::HashMap::new()),
            churn: std::sync::atomic::AtomicBool::new(false),
            pulls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn pull_count(&self) -> usize {
        self.pulls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn push_one(&self, change: Change) -> Version {
        let response = self
            .inner
            .apply_push(
                SyncScope::GLOBAL,
                &PushRequest {
                    device_id: "gc-injector".to_owned(),
                    changes: vec![change],
                },
                &LwwResolver,
            )
            .expect("injected push");
        match response.outcomes[0] {
            ChangeOutcome::Applied { version } => version,
            ref other => panic!("injected change must clean-apply, got {other:?}"),
        }
    }

    /// Delete the live row `pk` and GC its tombstone away.
    fn delete_and_gc(&self, pk: &str) {
        let rows = match self
            .inner
            .pull_since(SyncScope::GLOBAL, 0, 10_000, 0)
            .expect("inner pull")
        {
            PullResponse::Ok { rows, .. } => rows,
            PullResponse::FullResyncRequired { .. } => unreachable!("cursor 0 never resyncs"),
        };
        let row = rows
            .iter()
            .find(|r| r.pk == pk && !r.deleted)
            .expect("live row to delete");
        self.push_one(Change {
            change_id: format!("gc-inject-delete-{pk}"),
            collection: row.collection.clone(),
            pk: pk.to_owned(),
            op: Op::Delete,
            payload: None,
            base_version: row.version,
            updated_at: Utc::now(),
        });
        let latest = self.inner.latest_version().expect("latest");
        assert!(
            self.inner.gc_tombstones(latest).expect("gc") >= 1,
            "the injected tombstone must be GC'd"
        );
    }

    /// Create a throwaway row, delete it, and GC — advancing the horizon
    /// without touching the payload rows.
    fn churn_once(&self, n: usize) {
        let version = self.push_one(Change {
            change_id: format!("churn-up-{n}"),
            collection: "churn".to_owned(),
            pk: format!("churn-{n}"),
            op: Op::Upsert,
            payload: Some(json!({"churn": n})),
            base_version: 0,
            updated_at: Utc::now(),
        });
        self.push_one(Change {
            change_id: format!("churn-del-{n}"),
            collection: "churn".to_owned(),
            pk: format!("churn-{n}"),
            op: Op::Delete,
            payload: None,
            base_version: version,
            updated_at: Utc::now(),
        });
        let latest = self.inner.latest_version().expect("latest");
        self.inner.gc_tombstones(latest).expect("churn gc");
    }
}

impl SyncBackend for GcInjectingBackend {
    fn apply_push(
        &self,
        scope: &str,
        request: &PushRequest,
        resolver: &dyn ConflictResolver,
    ) -> Result<PushResponse, SyncError> {
        self.inner.apply_push(scope, request, resolver)
    }
    fn pull_since(
        &self,
        scope: &str,
        cursor: Version,
        limit: i64,
        session_start: Version,
    ) -> Result<PullResponse, SyncError> {
        let n = self.pulls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let injected = self.delete_before_pull.lock().expect("lock").remove(&n);
        if let Some(pk) = injected {
            self.delete_and_gc(pk);
        }
        if n > 1 && self.churn.load(std::sync::atomic::Ordering::SeqCst) {
            self.churn_once(n);
        }
        self.inner.pull_since(scope, cursor, limit, session_start)
    }
    fn gc_tombstones(&self, up_to: Version) -> Result<u64, SyncError> {
        self.inner.gc_tombstones(up_to)
    }
    fn gc_applied(&self, older_than: chrono::DateTime<Utc>) -> Result<u64, SyncError> {
        self.inner.gc_applied(older_than)
    }
    fn tombstone_horizon(&self) -> Result<Version, SyncError> {
        self.inner.tombstone_horizon()
    }
    fn latest_version(&self) -> Result<Version, SyncError> {
        self.inner.latest_version()
    }
}

/// Seed n1..n5 into a wrapped backend and serve it; page size 2 makes the
/// initial from-zero catch-up span three pages.
async fn gc_race_fixture(
    backend: &Arc<GcInjectingBackend>,
) -> (String, tokio::task::JoinHandle<()>, SyncConfig) {
    for i in 1..=5 {
        backend.push_one(Change {
            change_id: format!("00000000-0000-4000-8000-0000000000a{i}"),
            collection: "notes".to_owned(),
            pk: format!("n{i}"),
            op: Op::Upsert,
            payload: Some(json!({"title": format!("note {i}")})),
            base_version: 0,
            updated_at: Utc::now(),
        });
    }
    let (url, srv) = start_sync_server(
        backend.clone() as Arc<dyn SyncBackend>,
        Arc::new(LwwResolver),
    )
    .await;
    let mut config = SyncConfig::new(&url);
    config.pull_batch_size = 2;
    (url, srv, config)
}

/// A tombstone GC committing between two pages of a fresh device's
/// from-zero catch-up must not leave the deleted row visible: the per-page
/// horizon guard detects the movement, defers to the (restart-protected)
/// snapshot resync, and the device converges with the row absent.
#[tokio::test]
async fn gc_between_pages_of_a_from_zero_catchup_converges_without_stale_rows() {
    let backend = Arc::new(GcInjectingBackend::new(MemorySyncBackend::new()));
    let (_url, _srv, config) = gc_race_fixture(&backend).await;
    // Page 1 serves n1 live; before page 2 its delete commits AND its
    // tombstone is GC'd — the tombstone can never be pulled.
    backend
        .delete_before_pull
        .lock()
        .expect("lock")
        .insert(2, "n1");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(&dir, "fresh.db");
    let report = SyncEngine::new(store.clone(), config)
        .sync_once()
        .await
        .expect("sync converges despite the racing GC");
    assert!(
        report.full_resync,
        "the horizon movement must trigger the snapshot resync"
    );
    assert_eq!(
        store.get::<Note>("notes", "n1").expect("get n1"),
        None,
        "the row deleted+GC'd mid-catch-up must not survive"
    );
    for i in 2..=5 {
        assert!(
            store
                .get::<Note>("notes", &format!("n{i}"))
                .expect("get")
                .is_some(),
            "live row n{i} must arrive"
        );
    }
}

/// A GC racing the SNAPSHOT itself (between two of its pages) restarts the
/// snapshot from scratch: rows already buffered from early pages would
/// otherwise resurrect — the tombstone for a buffered row was physically
/// dropped before a later page could deliver it.
#[tokio::test]
async fn gc_between_snapshot_pages_restarts_the_snapshot() {
    let backend = Arc::new(GcInjectingBackend::new(MemorySyncBackend::new()));
    let (_url, _srv, config) = gc_race_fixture(&backend).await;
    {
        let mut inject = backend.delete_before_pull.lock().expect("lock");
        // Pull #2: second page of the initial catch-up → triggers the
        // resync. Pull #4: second page of snapshot attempt 1, whose first
        // page (pull #3) already buffered n2 live → must restart.
        inject.insert(2, "n1");
        inject.insert(4, "n2");
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(&dir, "fresh.db");
    let report = SyncEngine::new(store.clone(), config)
        .sync_once()
        .await
        .expect("sync converges after the snapshot restart");
    assert!(report.full_resync);
    assert_eq!(
        store.get::<Note>("notes", "n1").expect("get n1"),
        None,
        "the catch-up-raced deletion must hold"
    );
    assert_eq!(
        store.get::<Note>("notes", "n2").expect("get n2"),
        None,
        "the row buffered by the ABORTED snapshot attempt must not \
         survive the restart"
    );
    for i in 3..=5 {
        assert!(
            store
                .get::<Note>("notes", &format!("n{i}"))
                .expect("get")
                .is_some(),
            "live row n{i} must arrive"
        );
    }
    // Catch-up pages (2) + aborted snapshot attempt (2) + clean attempt (2).
    assert_eq!(backend.pull_count(), 6, "exactly one snapshot restart");
}

/// Snapshot restarts are BOUNDED: a GC schedule that keeps moving the
/// horizon between every pair of pages eventually surfaces an error
/// instead of restarting forever — and local state stays intact, so a
/// later pass (once the churn stops) converges normally.
#[tokio::test]
async fn snapshot_restarts_are_bounded_and_recoverable() {
    let backend = Arc::new(GcInjectingBackend::new(MemorySyncBackend::new()));
    let (_url, _srv, config) = gc_race_fixture(&backend).await;
    backend
        .churn
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(&dir, "fresh.db");
    let engine = SyncEngine::new(store.clone(), config);
    let err = engine
        .sync_once()
        .await
        .expect_err("perpetual mid-snapshot GC churn must surface an error");
    assert!(
        matches!(&err, SyncError::Server(msg) if msg.contains("mid-snapshot")),
        "the error must name the mid-snapshot GC churn, got {err:?}"
    );

    // The churn stops; the same store recovers on the next pass.
    backend
        .churn
        .store(false, std::sync::atomic::Ordering::SeqCst);
    engine.sync_once().await.expect("recovery pass");
    for i in 1..=5 {
        assert!(
            store
                .get::<Note>("notes", &format!("n{i}"))
                .expect("get")
                .is_some(),
            "live row n{i} must arrive after the churn stops"
        );
    }
}

/// Regression guard for the guard itself: a normal multi-page from-zero
/// catch-up with a STABLE horizon completes in exactly its page count —
/// no spurious restarts, no resync.
#[tokio::test]
async fn stable_horizon_multi_page_catchup_never_restarts() {
    let backend = Arc::new(GcInjectingBackend::new(MemorySyncBackend::new()));
    let (_url, _srv, config) = gc_race_fixture(&backend).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(&dir, "fresh.db");
    let report = SyncEngine::new(store.clone(), config)
        .sync_once()
        .await
        .expect("plain multi-page sync");
    assert!(!report.full_resync, "a stable horizon never resyncs");
    assert_eq!(report.pulled, 5);
    assert_eq!(
        backend.pull_count(),
        3,
        "three pages, no restarts (2 + 2 + 1 rows)"
    );
}

/// Overlapping `sync_once` calls (the Tauri shell's resume kick racing the
/// background loop) must serialize: the journal batch is pushed exactly
/// once across both passes.
#[tokio::test]
async fn concurrent_sync_once_passes_serialize() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let store = open_store(&dir, "a.db");
    store.put("notes", "n1", &note("one")).expect("put");
    store.put("notes", "n2", &note("two")).expect("put");
    store.put("notes", "n3", &note("three")).expect("put");

    let engine = engine_for(&store, &url);
    let (r1, r2) = tokio::join!(engine.sync_once(), engine.sync_once());
    let (r1, r2) = (r1.expect("pass 1"), r2.expect("pass 2"));
    assert_eq!(
        r1.pushed + r2.pushed,
        3,
        "the batch must be pushed exactly once across overlapping passes"
    );
    assert_eq!(store.pending_count().expect("pending"), 0);
    assert_eq!(backend_rows(backend.as_ref()).len(), 3);
}

/// End-to-end m2 regression: a retry that gets `already_applied` records
/// the acked version, so the NEXT edit of the same row pushes a correct
/// `base_version` and does not trip the conflict resolver. The resolver
/// here always keeps the server row, so a false conflict would visibly
/// reject the second edit.
#[tokio::test]
async fn already_applied_retry_then_edit_does_not_false_conflict() {
    struct KeepServerResolver;
    impl ConflictResolver for KeepServerResolver {
        fn resolve(&self, _device: &str, _client: &Change, _server: &RemoteRow) -> Resolution {
            Resolution::KeepServer
        }
    }

    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(KeepServerResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let store = open_store(&dir, "a.db");
    let engine = engine_for(&store, &url);
    store.put("notes", "n1", &note("first")).expect("put");

    // Simulate a lost push response: the server applies the batch, but the
    // client never records the ack (journal still holds the change).
    let request = PushRequest {
        device_id: store.device_id().expect("device id"),
        changes: store.pending_changes(10).expect("pending"),
    };
    backend
        .apply_push(SyncScope::GLOBAL, &request, &LwwResolver)
        .expect("server-side apply");
    assert_eq!(store.pending_count().expect("pending"), 1);

    // The engine retries → already_applied → the ack is recorded now.
    engine.sync_once().await.expect("retry sync");
    assert_eq!(store.pending_count().expect("pending"), 0);

    // A subsequent edit must apply cleanly. Under the old behavior its
    // base_version stayed 0 → conflict → KeepServer would discard it.
    store.put("notes", "n1", &note("second")).expect("edit");
    engine.sync_once().await.expect("edit sync");
    let rows = backend_rows(backend.as_ref());
    assert_eq!(
        rows.iter()
            .find(|r| r.pk == "n1")
            .and_then(|r| r.payload.as_ref())
            .and_then(|p| p.get("title"))
            .and_then(|v| v.as_str()),
        Some("second"),
        "the follow-up edit must not be treated as a conflict"
    );
}

/// `pull_batch_size = 0` (or negative) must be clamped, never spin
/// `sync_once` in an infinite empty-page loop.
#[tokio::test]
async fn pull_batch_size_zero_is_clamped_not_an_infinite_loop() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let store_a = open_store(&dir, "a.db");
    store_a.put("notes", "n1", &note("one")).expect("put");
    store_a.put("notes", "n2", &note("two")).expect("put");
    engine_for(&store_a, &url).sync_once().await.expect("seed");

    let store_b = open_store(&dir, "b.db");
    let mut config = SyncConfig::new(&url);
    config.pull_batch_size = 0;
    let engine_b = SyncEngine::new(store_b.clone(), config);

    let report = tokio::time::timeout(std::time::Duration::from_secs(30), engine_b.sync_once())
        .await
        .expect("sync_once must terminate with a zero batch size")
        .expect("sync");
    assert_eq!(report.pulled, 2);
    let listed: Vec<(String, Note)> = store_b.list("notes").expect("list");
    assert_eq!(listed.len(), 2);
}

/// Server-side request bounds: oversized push batches are rejected with
/// 413, and absurd pull limits are clamped instead of honored.
#[tokio::test]
async fn server_bounds_push_batch_size_and_pull_limit() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let client = reqwest::Client::new();

    let oversized = PushRequest {
        device_id: "device-a".to_owned(),
        changes: (0..=MAX_PUSH_CHANGES)
            .map(|i| Change {
                change_id: format!("00000000-0000-4000-8000-{i:012}"),
                collection: "notes".to_owned(),
                pk: format!("n{i}"),
                op: Op::Delete,
                payload: None,
                base_version: 0,
                updated_at: Utc::now(),
            })
            .collect(),
    };
    let response = client
        .post(format!("{url}/push"))
        .json(&oversized)
        .send()
        .await
        .expect("send oversized push");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "a push batch beyond MAX_PUSH_CHANGES must be rejected"
    );
    assert!(
        backend_rows(backend.as_ref()).is_empty(),
        "a rejected batch must not be applied"
    );

    let response = client
        .get(format!("{url}/pull?cursor=0&limit=999999999"))
        .send()
        .await
        .expect("send oversized pull");
    assert!(response.status().is_success(), "pull limits are clamped");
    let pull: PullResponse = response.json().await.expect("pull json");
    assert!(matches!(pull, PullResponse::Ok { .. }));
}

/// Protocol validation over HTTP: a push batch containing an upsert without
/// a payload is rejected with 422 and nothing is applied — a NULL-payload
/// row would be invisible to clients (store get/list treat a missing
/// payload as absent) while still advancing their cursor.
#[tokio::test]
async fn server_rejects_payloadless_upsert_with_422() {
    let backend = Arc::new(MemorySyncBackend::new());
    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let client = reqwest::Client::new();

    let bad = PushRequest {
        device_id: "device-a".to_owned(),
        changes: vec![
            Change {
                change_id: "00000000-0000-4000-8000-000000000001".to_owned(),
                collection: "notes".to_owned(),
                pk: "good".to_owned(),
                op: Op::Upsert,
                payload: Some(json!({"title": "valid sibling"})),
                base_version: 0,
                updated_at: Utc::now(),
            },
            Change {
                change_id: "00000000-0000-4000-8000-000000000002".to_owned(),
                collection: "notes".to_owned(),
                pk: "bad".to_owned(),
                op: Op::Upsert,
                payload: None,
                base_version: 0,
                updated_at: Utc::now(),
            },
        ],
    };
    let response = client
        .post(format!("{url}/push"))
        .json(&bad)
        .send()
        .await
        .expect("send payload-less upsert");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        "an upsert without a payload must be rejected as a protocol error"
    );
    let body = response.text().await.expect("error body");
    assert!(
        body.contains("payload"),
        "the error must name the missing payload, got: {body}"
    );
    assert!(
        backend_rows(backend.as_ref()).is_empty(),
        "a rejected batch must not be applied — not even its valid changes"
    );

    // Deletes legitimately omit the payload and must keep working.
    let tombstone_only = PushRequest {
        device_id: "device-a".to_owned(),
        changes: vec![Change {
            change_id: "00000000-0000-4000-8000-000000000003".to_owned(),
            collection: "notes".to_owned(),
            pk: "gone".to_owned(),
            op: Op::Delete,
            payload: None,
            base_version: 0,
            updated_at: Utc::now(),
        }],
    };
    let response = client
        .post(format!("{url}/push"))
        .json(&tombstone_only)
        .send()
        .await
        .expect("send delete");
    assert!(
        response.status().is_success(),
        "deletes without a payload are valid, got {}",
        response.status()
    );
    let push: PushResponse = response.json().await.expect("push json");
    assert!(matches!(push.outcomes[0], ChangeOutcome::Applied { .. }));
}

/// The documented auth wiring works end-to-end: an auth middleware layer
/// on the sync router rejects unauthenticated engines, and
/// `SyncConfig::bearer_token` gets an engine through it.
#[tokio::test]
async fn bearer_token_authenticates_against_a_guarded_router() {
    use axum::middleware::Next;

    async fn require_sync_auth(
        request: axum::extract::Request,
        next: Next,
    ) -> Result<axum::response::Response, axum::http::StatusCode> {
        let authorized = request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| token == "sync-secret");
        if authorized {
            Ok(next.run(request).await)
        } else {
            Err(axum::http::StatusCode::UNAUTHORIZED)
        }
    }

    let backend: Arc<dyn SyncBackend> = Arc::new(MemorySyncBackend::new());
    let guarded: axum::Router = axum::Router::new().nest(
        "/sync",
        server::router(backend, Arc::new(LwwResolver))
            .layer(axum::middleware::from_fn(require_sync_auth)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let _srv = tokio::spawn(async move {
        axum::serve(listener, guarded).await.expect("serve");
    });
    let url = format!("http://{addr}/sync");

    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(&dir, "a.db");
    store.put("notes", "n1", &note("private")).expect("put");

    // Without a token the engine is rejected (and loses no local state).
    let unauthenticated = engine_for(&store, &url);
    let err = unauthenticated.sync_once().await.expect_err("401 expected");
    assert!(
        matches!(&err, SyncError::Server(msg) if msg.contains("401")),
        "expected a 401 server error, got {err:?}"
    );
    assert_eq!(store.pending_count().expect("pending"), 1);

    // With the token the same engine wiring syncs.
    let mut config = SyncConfig::new(&url);
    config.bearer_token = Some("sync-secret".to_owned());
    let authenticated = SyncEngine::new(store.clone(), config);
    let report = authenticated.sync_once().await.expect("authorized sync");
    assert_eq!(report.pushed, 1);
    assert_eq!(store.pending_count().expect("pending"), 0);
}

/// The documented multi-user wiring works end-to-end: auth middleware
/// derives a `SyncScope` from the authenticated principal (here: the
/// bearer token) and `server::scoped_router` partitions all data by it —
/// two users writing the same `(collection, pk)` never see or clobber each
/// other's rows. The scope is never client-supplied: the engines send only
/// their credential, and the wire protocol carries no scope field.
#[tokio::test]
async fn scoped_router_partitions_data_by_authenticated_principal() {
    use axum::middleware::Next;

    /// Authenticate the bearer token and attach the principal's scope —
    /// the middleware shape documented in
    /// docs/guide/tauri-mobile-offline-sync.md.
    async fn auth_and_scope(
        mut request: axum::extract::Request,
        next: Next,
    ) -> Result<axum::response::Response, axum::http::StatusCode> {
        let user = request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .and_then(|token| match token {
                "alice-token" => Some("user:alice"),
                "bob-token" => Some("user:bob"),
                _ => None,
            })
            .ok_or(axum::http::StatusCode::UNAUTHORIZED)?;
        request.extensions_mut().insert(SyncScope::new(user));
        Ok(next.run(request).await)
    }

    let backend = Arc::new(MemorySyncBackend::new());
    let router: axum::Router = axum::Router::new().nest(
        "/sync",
        server::scoped_router(backend.clone(), Arc::new(LwwResolver))
            .layer(axum::middleware::from_fn(auth_and_scope)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let _srv = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    let url = format!("http://{addr}/sync");

    let engine_as = |store: &SyncStore, token: &str| {
        let mut config = SyncConfig::new(&url);
        config.bearer_token = Some(token.to_owned());
        SyncEngine::new(store.clone(), config)
    };

    // Both users write the SAME collection/pk — offline, independently.
    let dir = tempfile::tempdir().expect("tempdir");
    let alice = open_store(&dir, "alice.db");
    alice
        .put("notes", "n1", &note("alice's note"))
        .expect("put");
    let bob = open_store(&dir, "bob.db");
    bob.put("notes", "n1", &note("bob's note")).expect("put");

    let report = engine_as(&alice, "alice-token")
        .sync_once()
        .await
        .expect("alice sync");
    assert_eq!(report.pushed, 1);
    // Bob's identical pk clean-applies in HIS scope (a distinct row): no
    // conflict against alice's row, no resolver involvement.
    let report = engine_as(&bob, "bob-token")
        .sync_once()
        .await
        .expect("bob sync");
    assert_eq!(report.pushed, 1);

    // Each principal pulls only its own scope: a second sync leaves each
    // store holding exactly its own document.
    engine_as(&alice, "alice-token")
        .sync_once()
        .await
        .expect("alice re-sync");
    engine_as(&bob, "bob-token")
        .sync_once()
        .await
        .expect("bob re-sync");
    assert_eq!(
        alice.get::<Note>("notes", "n1").expect("alice get"),
        Some(note("alice's note")),
        "bob's write must never reach alice's scope"
    );
    assert_eq!(
        bob.get::<Note>("notes", "n1").expect("bob get"),
        Some(note("bob's note")),
        "alice's write must never reach bob's scope"
    );

    // And server-side the two scopes hold two distinct rows.
    let scope_rows = |scope: &str| match backend
        .pull_since(scope, 0, 10_000, 0)
        .expect("backend pull")
    {
        PullResponse::Ok { rows, .. } => rows,
        PullResponse::FullResyncRequired { .. } => panic!("unexpected resync from cursor 0"),
    };
    assert_eq!(scope_rows("user:alice").len(), 1);
    assert_eq!(scope_rows("user:bob").len(), 1);
    assert_eq!(
        scope_rows(SyncScope::GLOBAL).len(),
        0,
        "nothing may leak into the single-tenant default scope"
    );
}

/// `scoped_router` fails CLOSED: a deployment whose middleware forgot to
/// insert the `SyncScope` extension is misconfigured, and requests are
/// rejected with 500 before touching the backend — falling back to a
/// shared scope would silently merge tenants' data.
#[tokio::test]
async fn scoped_router_fails_closed_without_scope_extension() {
    let backend = Arc::new(MemorySyncBackend::new());
    // No middleware inserts a SyncScope: every request must be rejected.
    let router: axum::Router = axum::Router::new().nest(
        "/sync",
        server::scoped_router(backend.clone(), Arc::new(LwwResolver)),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let _srv = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    let url = format!("http://{addr}/sync");
    let client = reqwest::Client::new();

    let push = PushRequest {
        device_id: "device-a".to_owned(),
        changes: vec![Change {
            change_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            collection: "notes".to_owned(),
            pk: "n1".to_owned(),
            op: Op::Upsert,
            payload: Some(json!({"title": "rejected"})),
            base_version: 0,
            updated_at: Utc::now(),
        }],
    };
    let response = client
        .post(format!("{url}/push"))
        .json(&push)
        .send()
        .await
        .expect("send push");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "a scope-less push against scoped_router must fail closed"
    );
    let body = response.text().await.expect("error body");
    assert!(
        body.contains("SyncScope"),
        "the rejection must name the missing extension, got: {body}"
    );

    let response = client
        .get(format!("{url}/pull?cursor=0&limit=10"))
        .send()
        .await
        .expect("send pull");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        "a scope-less pull against scoped_router must fail closed"
    );

    assert_eq!(
        backend.latest_version().expect("latest"),
        0,
        "nothing may be applied by rejected scope-less requests"
    );
}

/// `spawn_background` behavior: while the backend errors, passes back off
/// and keep retrying; once it heals, the loop converges the store without
/// intervention. Bounds are generous — no wall-clock exactness.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one linear outage→recovery script + fake backend
async fn spawn_background_backs_off_then_recovers() {
    /// A backend that fails every call until `failures_left` drains, then
    /// delegates to the inner memory backend.
    struct FlakyBackend {
        inner: MemorySyncBackend,
        failures_left: std::sync::atomic::AtomicUsize,
        attempts: std::sync::atomic::AtomicUsize,
    }
    impl FlakyBackend {
        fn gate(&self) -> Result<(), SyncError> {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let failed = self
                .failures_left
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                )
                .is_ok();
            if failed {
                Err(SyncError::Backend("injected outage".into()))
            } else {
                Ok(())
            }
        }
    }
    impl SyncBackend for FlakyBackend {
        fn apply_push(
            &self,
            scope: &str,
            request: &PushRequest,
            resolver: &dyn ConflictResolver,
        ) -> Result<PushResponse, SyncError> {
            self.gate()?;
            self.inner.apply_push(scope, request, resolver)
        }
        fn pull_since(
            &self,
            scope: &str,
            cursor: Version,
            limit: i64,
            session_start: Version,
        ) -> Result<PullResponse, SyncError> {
            self.gate()?;
            self.inner.pull_since(scope, cursor, limit, session_start)
        }
        fn gc_tombstones(&self, up_to: Version) -> Result<u64, SyncError> {
            self.inner.gc_tombstones(up_to)
        }
        fn gc_applied(&self, older_than: chrono::DateTime<Utc>) -> Result<u64, SyncError> {
            self.inner.gc_applied(older_than)
        }
        fn tombstone_horizon(&self) -> Result<Version, SyncError> {
            self.inner.tombstone_horizon()
        }
        fn latest_version(&self) -> Result<Version, SyncError> {
            self.inner.latest_version()
        }
    }

    const INJECTED_FAILURES: usize = 3;
    let backend = Arc::new(FlakyBackend {
        inner: MemorySyncBackend::new(),
        failures_left: std::sync::atomic::AtomicUsize::new(INJECTED_FAILURES),
        attempts: std::sync::atomic::AtomicUsize::new(0),
    });
    // Seed the healthy inner state the loop should eventually deliver.
    backend
        .inner
        .apply_push(
            SyncScope::GLOBAL,
            &PushRequest {
                device_id: "seeder".to_owned(),
                changes: vec![Change {
                    change_id: "00000000-0000-4000-8000-00000000feed".to_owned(),
                    collection: "notes".to_owned(),
                    pk: "n1".to_owned(),
                    op: Op::Upsert,
                    payload: Some(json!({"title": "recovered"})),
                    base_version: 0,
                    updated_at: Utc::now(),
                }],
            },
            &LwwResolver,
        )
        .expect("seed");

    let (url, _srv) = start_sync_server(backend.clone(), Arc::new(LwwResolver)).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let store = open_store(&dir, "a.db");
    store
        .put("notes", "local", &note("queued while flaky"))
        .expect("put");

    let mut config = SyncConfig::new(&url);
    config.min_backoff = std::time::Duration::from_millis(10);
    config.max_backoff = std::time::Duration::from_millis(50);
    let engine = SyncEngine::new(store.clone(), config);
    let task = engine.spawn_background(std::time::Duration::from_millis(20));

    // The loop must ride out the outage (backing off, not exiting) and
    // converge once the backend heals. Generous 30 s ceiling; typical
    // completion is tens of milliseconds.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let pulled: Option<Note> = store.get("notes", "n1").expect("get");
        if pulled.is_some() && store.pending_count().expect("pending") == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background loop failed to recover after the injected outage"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    task.abort();

    let attempts = backend.attempts.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        attempts > INJECTED_FAILURES,
        "the loop must retry through the outage (attempts: {attempts})"
    );
    assert_eq!(
        backend
            .failures_left
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "every injected failure must have been consumed by a retry"
    );
    let rows = backend_rows(&backend.inner);
    assert!(
        rows.iter().any(|r| r.pk == "local"),
        "the offline write must reach the server after recovery"
    );
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
