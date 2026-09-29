//! Integration tests for `autumn_web::sync::SyncStore` — the local
//! offline `SQLite` store with write-through change journaling.

#![cfg(feature = "offline-sync")]

use autumn_web::sync::{Op, SyncStore};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Note {
    id: String,
    title: String,
    done: bool,
}

fn note(id: &str, title: &str) -> Note {
    Note {
        id: id.to_owned(),
        title: title.to_owned(),
        done: false,
    }
}

#[test]
fn store_open_creates_schema_and_reopens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sync.db");

    let first_device_id;
    {
        let store = SyncStore::open(&path).expect("open store");
        store.put("notes", "n1", &note("n1", "hello")).expect("put");
        first_device_id = store.device_id().expect("device_id");
        assert!(!first_device_id.is_empty(), "device id should be generated");
    }

    // Reopen the same file: data, journal, and identity all persist.
    let store = SyncStore::open(&path).expect("reopen store");
    let fetched: Option<Note> = store.get("notes", "n1").expect("get");
    assert_eq!(fetched, Some(note("n1", "hello")));
    assert_eq!(
        store.device_id().expect("device_id"),
        first_device_id,
        "device id must be stable across reopens"
    );
    assert_eq!(
        store.pending_count().expect("pending_count"),
        1,
        "journal must survive reopen"
    );
}

#[test]
fn put_get_list_roundtrip_typed_payloads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SyncStore::open(dir.path().join("sync.db")).expect("open store");

    store
        .put("notes", "b", &note("b", "second"))
        .expect("put b");
    store.put("notes", "a", &note("a", "first")).expect("put a");
    store
        .put("todos", "t1", &note("t1", "other collection"))
        .expect("put todo");

    let got: Option<Note> = store.get("notes", "a").expect("get");
    assert_eq!(got, Some(note("a", "first")));

    // Update in place.
    store
        .put("notes", "a", &note("a", "first-edited"))
        .expect("re-put a");
    let got: Option<Note> = store.get("notes", "a").expect("get after edit");
    assert_eq!(got, Some(note("a", "first-edited")));

    // List is per-collection, pk-ordered, and typed.
    let listed: Vec<(String, Note)> = store.list("notes").expect("list");
    assert_eq!(
        listed,
        vec![
            ("a".to_owned(), note("a", "first-edited")),
            ("b".to_owned(), note("b", "second")),
        ]
    );

    // Unknown rows are None, unknown collections empty.
    let missing: Option<Note> = store.get("notes", "zzz").expect("get missing");
    assert_eq!(missing, None);
    let empty: Vec<(String, Note)> = store.list("nothing").expect("list empty");
    assert!(empty.is_empty());
}

#[test]
fn delete_records_tombstone_and_pending_delete() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SyncStore::open(dir.path().join("sync.db")).expect("open store");

    store
        .put("notes", "n1", &note("n1", "doomed"))
        .expect("put");
    store.delete("notes", "n1").expect("delete");

    let got: Option<Note> = store.get("notes", "n1").expect("get");
    assert_eq!(got, None, "deleted rows must read as absent");
    let listed: Vec<(String, Note)> = store.list("notes").expect("list");
    assert!(listed.is_empty(), "tombstones must not appear in list()");

    let pending = store.pending_changes(10).expect("pending_changes");
    let entry = pending
        .iter()
        .find(|c| c.collection == "notes" && c.pk == "n1")
        .expect("a pending change for the deleted row");
    assert_eq!(entry.op, Op::Delete);
    assert_eq!(entry.payload, None, "deletes carry no payload");
    assert!(!entry.change_id.is_empty());
}

#[test]
fn writes_journal_pending_changes_atomically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SyncStore::open(dir.path().join("sync.db")).expect("open store");

    // Two writes to the same pk coalesce into ONE pending entry.
    store.put("notes", "n1", &note("n1", "v1")).expect("put v1");
    store.put("notes", "n1", &note("n1", "v2")).expect("put v2");
    assert_eq!(store.pending_count().expect("count"), 1);

    let pending = store.pending_changes(10).expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].op, Op::Upsert);
    assert_eq!(pending[0].collection, "notes");
    assert_eq!(pending[0].pk, "n1");
    assert_eq!(
        pending[0].base_version, 0,
        "a never-synced row is based on version 0"
    );
    let payload = pending[0].payload.as_ref().expect("upsert payload");
    assert_eq!(
        payload.get("title").and_then(|v| v.as_str()),
        Some("v2"),
        "the coalesced journal entry carries the latest payload"
    );

    // A different pk is a separate journal entry.
    store
        .put("notes", "n2", &note("n2", "other"))
        .expect("put n2");
    assert_eq!(store.pending_count().expect("count"), 2);

    // Deleting a pending-upsert pk coalesces it into a single delete.
    store.delete("notes", "n1").expect("delete n1");
    assert_eq!(store.pending_count().expect("count"), 2);
    let pending = store.pending_changes(10).expect("pending");
    let n1 = pending.iter().find(|c| c.pk == "n1").expect("n1 entry");
    assert_eq!(n1.op, Op::Delete);
}

/// Two `SyncStore` instances on ONE file (the generated wiring's shape:
/// the shell's background engine plus the app's routes) writing
/// concurrently: every write must succeed. Write transactions use
/// `BEGIN IMMEDIATE`, so cross-connection writers queue on the busy
/// timeout instead of failing their read→write snapshot upgrade with
/// "database is locked" (`SQLITE_BUSY_SNAPSHOT` bypasses the busy
/// handler for deferred transactions).
#[test]
fn concurrent_stores_on_one_file_never_fail_writes() {
    const WRITES_PER_THREAD: usize = 500;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sync.db");
    let store1 = SyncStore::open(&path).expect("open 1");
    let store2 = SyncStore::open(&path).expect("open 2");

    let worker = |store: SyncStore, prefix: &'static str| {
        std::thread::spawn(move || {
            for i in 0..WRITES_PER_THREAD {
                store
                    .put("notes", &format!("{prefix}-{i}"), &format!("payload {i}"))
                    .unwrap_or_else(|e| panic!("{prefix} write {i} failed: {e}"));
            }
        })
    };
    let t1 = worker(store1.clone(), "a");
    let t2 = worker(store2, "b");
    t1.join().expect("thread a");
    t2.join().expect("thread b");

    let listed: Vec<(String, String)> = store1.list("notes").expect("list");
    assert_eq!(
        listed.len(),
        2 * WRITES_PER_THREAD,
        "all concurrent writes from both connections must land"
    );
    assert_eq!(
        store1.pending_count().expect("pending"),
        2 * WRITES_PER_THREAD as u64,
        "every write must have journaled its pending change"
    );
}

#[test]
fn reads_reflect_local_writes_before_any_sync() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SyncStore::open(dir.path().join("sync.db")).expect("open store");

    // Fully offline: no server exists anywhere. Writes must be readable
    // immediately and queue up for later.
    store
        .put("notes", "n1", &note("n1", "offline one"))
        .expect("put");
    store
        .put("notes", "n2", &note("n2", "offline two"))
        .expect("put");

    let got: Option<Note> = store.get("notes", "n1").expect("get");
    assert_eq!(got, Some(note("n1", "offline one")));
    let listed: Vec<(String, Note)> = store.list("notes").expect("list");
    assert_eq!(listed.len(), 2);

    assert_eq!(store.pending_count().expect("count"), 2);
    assert_eq!(
        store.cursor().expect("cursor"),
        0,
        "nothing pulled yet — cursor stays at zero"
    );
}
