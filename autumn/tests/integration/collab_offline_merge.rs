//! An offline edit merges convergently on reconnect (issue #1806, AC4).
//!
//! The offline-sync engine resolves a conflicting push with
//! last-write-wins: the older write is discarded. That is data loss for a
//! collaborative field. [`CollabResolver`] changes the verdict for those
//! fields only — it merges the two documents — and keeps last-write-wins for
//! every other column of the same row.

#![cfg(all(feature = "collab", feature = "offline-sync"))]

use autumn_web::collab::{CollabResolver, CollabText};
use autumn_web::sync::protocol::{Change, Op, PullResponse, PushRequest, RemoteRow};
use autumn_web::sync::resolver::{ConflictResolver, LwwResolver, Resolution};
use autumn_web::sync::{MemorySyncBackend, SyncBackend, SyncScope};
use chrono::{Duration, Utc};
use serde_json::{Value, json};

/// A row as the device pushes it.
fn client_change(payload: Value, updated_at: chrono::DateTime<Utc>) -> Change {
    Change {
        change_id: "c1".to_owned(),
        collection: "notes".to_owned(),
        pk: "n1".to_owned(),
        op: Op::Upsert,
        payload: Some(payload),
        base_version: 1,
        updated_at,
    }
}

/// The row the server already holds.
fn server_row(payload: Value, updated_at: chrono::DateTime<Utc>) -> RemoteRow {
    RemoteRow {
        collection: "notes".to_owned(),
        pk: "n1".to_owned(),
        payload: Some(payload),
        version: 2,
        deleted: false,
        updated_at,
        device_id: "server-device".to_owned(),
    }
}

/// Two documents branched from one base, each with its own edit.
fn branched() -> (CollabText, CollabText) {
    let base = CollabText::from_text("seed", "hello world");
    let mut offline = base.clone();
    offline.insert("phone", 5, ",");
    let mut online = base;
    online.insert("laptop", 11, "!");
    (offline, online)
}

/// The loss this exists to prevent: with the stock resolver the older write
/// is thrown away whole.
#[test]
fn last_write_wins_discards_the_offline_edit() {
    let now = Utc::now();
    let (offline, online) = branched();
    let verdict = LwwResolver.resolve(
        "phone",
        &client_change(json!({ "body": offline }), now - Duration::seconds(5)),
        &server_row(json!({ "body": online }), now),
    );
    assert_eq!(
        verdict,
        Resolution::KeepServer,
        "the offline comma is discarded"
    );
}

/// AC4: the same conflict, resolved convergently — both edits survive.
#[test]
fn a_collaborative_field_merges_instead_of_clobbering() {
    let now = Utc::now();
    let (offline, online) = branched();
    let verdict = CollabResolver::new(["body"]).resolve(
        "phone",
        &client_change(json!({ "body": offline }), now - Duration::seconds(5)),
        &server_row(json!({ "body": online }), now),
    );

    let Resolution::Merge(merged) = verdict else {
        panic!("expected a merge, got {verdict:?}");
    };
    let body: CollabText = serde_json::from_value(merged["body"].clone()).expect("decode body");
    assert_eq!(body.text(), "hello, world!", "neither edit was lost");
}

/// The merge is symmetric: whichever side is newer, the text is the same.
#[test]
fn the_merge_does_not_depend_on_which_side_is_newer() {
    let now = Utc::now();
    let (offline, online) = branched();
    let resolver = CollabResolver::new(["body"]);

    let client_newer = resolver.resolve(
        "phone",
        &client_change(
            json!({ "body": offline.clone() }),
            now + Duration::seconds(5),
        ),
        &server_row(json!({ "body": online.clone() }), now),
    );
    let server_newer = resolver.resolve(
        "phone",
        &client_change(json!({ "body": offline }), now - Duration::seconds(5)),
        &server_row(json!({ "body": online }), now),
    );

    let text = |verdict: Resolution| match verdict {
        Resolution::Merge(value) => serde_json::from_value::<CollabText>(value["body"].clone())
            .expect("decode body")
            .text(),
        other => panic!("expected a merge, got {other:?}"),
    };
    assert_eq!(text(client_newer), text(server_newer));
}

/// Out of scope by design: a plain column keeps last-write-wins, so the
/// existing engine's default is unchanged for everything not marked.
#[test]
fn a_plain_column_still_takes_the_winning_side() {
    let now = Utc::now();
    let (offline, online) = branched();
    let verdict = CollabResolver::new(["body"]).resolve(
        "phone",
        &client_change(
            json!({ "body": offline, "title": "from the phone" }),
            now - Duration::seconds(5),
        ),
        &server_row(json!({ "body": online, "title": "from the laptop" }), now),
    );
    let Resolution::Merge(merged) = verdict else {
        panic!("expected a merge, got {verdict:?}");
    };
    assert_eq!(
        merged["title"], "from the laptop",
        "the plain column keeps the last-write-wins answer"
    );
}

/// A field named in the resolver but holding something that is not a
/// document keeps the wrapped verdict — no guessing, no corruption.
#[test]
fn a_non_document_field_keeps_the_wrapped_verdict() {
    let now = Utc::now();
    let verdict = CollabResolver::new(["body"]).resolve(
        "phone",
        &client_change(
            json!({ "body": "just a string" }),
            now + Duration::seconds(5),
        ),
        &server_row(json!({ "body": "another string" }), now),
    );
    assert_eq!(verdict, Resolution::TakeClient);
}

/// The dangerous near-miss: a field that holds an unrelated JSON object must
/// keep the wrapped verdict, never merge. If an arbitrary object decoded as an
/// empty document, the merge would write "no text" over real characters.
#[test]
fn an_unrelated_json_object_in_the_field_keeps_the_wrapped_verdict() {
    let now = Utc::now();
    let (offline, _) = branched();
    let verdict = CollabResolver::new(["body"]).resolve(
        "phone",
        &client_change(json!({ "body": { "note": "not a document" } }), now),
        &server_row(json!({ "body": offline }), now + Duration::seconds(5)),
    );
    assert_eq!(
        verdict,
        Resolution::KeepServer,
        "the server's real document must survive an unrelated object"
    );
}

/// A delete is a decision about whether the row exists, which belongs to the
/// wrapped resolver. Merging would resurrect a deleted row silently.
#[test]
fn a_delete_is_left_to_the_wrapped_resolver() {
    let now = Utc::now();
    let (offline, online) = branched();

    let mut deleting = client_change(
        json!({ "body": offline.clone() }),
        now + Duration::seconds(5),
    );
    deleting.op = Op::Delete;
    deleting.payload = None;
    assert_eq!(
        CollabResolver::new(["body"]).resolve(
            "phone",
            &deleting,
            &server_row(json!({ "body": online.clone() }), now)
        ),
        Resolution::TakeClient,
    );

    let mut deleted_row = server_row(json!({ "body": online }), now);
    deleted_row.deleted = true;
    assert_eq!(
        CollabResolver::new(["body"]).resolve(
            "phone",
            &client_change(json!({ "body": offline }), now - Duration::seconds(5)),
            &deleted_row
        ),
        Resolution::KeepServer,
    );
}

/// A field present on only one side is adopted whole rather than dropped.
#[test]
fn a_field_present_on_one_side_only_is_adopted() {
    let now = Utc::now();
    let (offline, _) = branched();
    let verdict = CollabResolver::new(["body"]).resolve(
        "phone",
        &client_change(json!({ "body": offline }), now - Duration::seconds(5)),
        &server_row(json!({ "title": "no body here" }), now),
    );
    let Resolution::Merge(merged) = verdict else {
        panic!("expected a merge, got {verdict:?}");
    };
    let body: CollabText = serde_json::from_value(merged["body"].clone()).expect("decode body");
    assert_eq!(body.text(), "hello, world");
}

/// Repeated pushes of the same offline change converge on one text: the
/// engine retries at-least-once, so the resolver has to be idempotent.
#[test]
fn replaying_the_same_push_is_idempotent() {
    let now = Utc::now();
    let (offline, online) = branched();
    let resolver = CollabResolver::new(["body"]);
    let change = client_change(json!({ "body": offline }), now - Duration::seconds(5));

    let first = resolver.resolve(
        "phone",
        &change,
        &server_row(json!({ "body": online }), now),
    );
    let Resolution::Merge(merged) = first else {
        panic!("expected a merge");
    };
    let second = resolver.resolve(
        "phone",
        &change,
        &server_row(as_payload(merged["body"].clone()), now),
    );
    let Resolution::Merge(again) = second else {
        panic!("expected a merge");
    };
    let body: CollabText = serde_json::from_value(again["body"].clone()).expect("decode body");
    assert_eq!(body.text(), "hello, world!");
}

/// Wrap a merged body back into a row payload.
fn as_payload(body: Value) -> Value {
    json!({ "body": body })
}

// ── End to end, through the sync backend's real push path ────────────────────

fn upsert(change_id: &str, base_version: i64, body: &CollabText) -> Change {
    Change {
        change_id: change_id.to_owned(),
        collection: "notes".to_owned(),
        pk: "n1".to_owned(),
        op: Op::Upsert,
        payload: Some(json!({ "body": body, "title": "Shopping list" })),
        base_version,
        updated_at: Utc::now(),
    }
}

fn push(device: &str, change: Change) -> PushRequest {
    PushRequest {
        device_id: device.to_owned(),
        changes: vec![change],
    }
}

/// The rows a client would see on its next pull.
fn pulled_rows(backend: &MemorySyncBackend) -> Vec<RemoteRow> {
    match backend
        .pull_since(SyncScope::GLOBAL, 0, 100, i64::MAX)
        .expect("pull")
    {
        PullResponse::Ok { rows, .. } => rows,
        PullResponse::FullResyncRequired { .. } => {
            panic!("a fresh backend never asks for a full resync")
        }
    }
}

/// The reconnect, end to end: a device that edited offline pushes a change
/// based on a version the server has already moved past. The resolver runs
/// inside the push transaction, and the row every device pulls next carries
/// **both** edits.
#[test]
fn an_offline_edit_merges_through_the_real_push_path() {
    let backend = MemorySyncBackend::new();
    let resolver = CollabResolver::new(["body"]);
    let scope = SyncScope::GLOBAL;

    // The row both devices last saw.
    let base = CollabText::from_text("seed", "hello world");
    backend
        .apply_push(
            scope,
            &push(
                "laptop",
                upsert("00000000-0000-4000-8000-000000000001", 0, &base),
            ),
            &resolver,
        )
        .expect("seed push");

    // The laptop edits and pushes first.
    let mut online = base.clone();
    online.insert("laptop", 11, "!");
    backend
        .apply_push(
            scope,
            &push(
                "laptop",
                upsert("00000000-0000-4000-8000-000000000002", 1, &online),
            ),
            &resolver,
        )
        .expect("online push");

    // The phone was offline the whole time: its change is still based on
    // version 1, which is exactly the conflict LWW would resolve by discarding
    // one side.
    let mut offline = base;
    offline.insert("phone", 5, ",");
    backend
        .apply_push(
            scope,
            &push(
                "phone",
                upsert("00000000-0000-4000-8000-000000000003", 1, &offline),
            ),
            &resolver,
        )
        .expect("reconnect push");

    // What every device pulls next.
    let pulled = pulled_rows(&backend);
    let row = pulled
        .iter()
        .find(|row| row.pk == "n1")
        .expect("the note is in the feed");
    let payload = row.payload.as_ref().expect("a live row has a payload");
    let body: CollabText =
        serde_json::from_value(payload["body"].clone()).expect("decode the merged body");

    assert_eq!(
        body.text(),
        "hello, world!",
        "the offline edit merged instead of clobbering"
    );
    assert_eq!(
        payload["title"], "Shopping list",
        "the plain column is untouched"
    );
}

/// The same script under the stock resolver loses the offline edit — the
/// before-and-after this feature exists for.
#[test]
fn the_same_script_under_last_write_wins_loses_the_offline_edit() {
    let backend = MemorySyncBackend::new();
    let resolver = LwwResolver;
    let scope = SyncScope::GLOBAL;

    let base = CollabText::from_text("seed", "hello world");
    backend
        .apply_push(
            scope,
            &push(
                "laptop",
                upsert("00000000-0000-4000-8000-000000000011", 0, &base),
            ),
            &resolver,
        )
        .expect("seed push");

    let mut online = base.clone();
    online.insert("laptop", 11, "!");
    let mut online_change = upsert("00000000-0000-4000-8000-000000000012", 1, &online);
    online_change.updated_at = Utc::now() + Duration::seconds(60);
    backend
        .apply_push(scope, &push("laptop", online_change), &resolver)
        .expect("online push");

    let mut offline = base;
    offline.insert("phone", 5, ",");
    backend
        .apply_push(
            scope,
            &push(
                "phone",
                upsert("00000000-0000-4000-8000-000000000013", 1, &offline),
            ),
            &resolver,
        )
        .expect("reconnect push");

    let pulled = pulled_rows(&backend);
    let row = pulled
        .iter()
        .find(|row| row.pk == "n1")
        .expect("the note is in the feed");
    let body: CollabText =
        serde_json::from_value(row.payload.as_ref().expect("payload")["body"].clone())
            .expect("decode body");
    assert_eq!(
        body.text(),
        "hello world!",
        "last-write-wins keeps only the newer write — the comma is gone"
    );
}

/// `for_table` scopes itself to that collection: a same-named field on
/// another collection keeps last-write-wins, which is what its name promises.
#[test]
fn for_table_does_not_merge_another_collections_field() {
    let now = Utc::now();
    let (offline, online) = branched();
    // No model registers `test_scoped_notes`, so drive the scoping directly.
    let resolver = CollabResolver::new(["body"]).in_collection("notes");

    let mut elsewhere = client_change(json!({ "body": offline.clone() }), now);
    elsewhere.collection = "templates".to_owned();
    assert_eq!(
        resolver.resolve(
            "phone",
            &elsewhere,
            &server_row(json!({ "body": online.clone() }), now)
        ),
        Resolution::KeepServer,
        "another collection is out of scope"
    );

    // In scope, the same conflict merges.
    let verdict = resolver.resolve(
        "phone",
        &client_change(json!({ "body": offline }), now),
        &server_row(json!({ "body": online }), now),
    );
    assert!(matches!(verdict, Resolution::Merge(_)));
}
