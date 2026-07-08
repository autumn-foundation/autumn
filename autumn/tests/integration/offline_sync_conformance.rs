//! Shared conformance suite for `SyncBackend` implementations.
//!
//! Run against `MemorySyncBackend` here (always) and `PgSyncBackend` in
//! `offline_sync_pg` (docker-gated), so both backends prove the same
//! push/pull/dedup/conflict/GC semantics.

#![cfg(feature = "offline-sync")]

use autumn_web::sync::{
    Change, ChangeOutcome, LwwResolver, MemorySyncBackend, Op, PullResponse, PushRequest,
    SyncBackend,
};
use chrono::{Duration, Utc};
use serde_json::json;

fn change(change_id: &str, pk: &str, op: Op, payload: Option<serde_json::Value>) -> Change {
    Change {
        change_id: change_id.to_owned(),
        collection: "conformance".to_owned(),
        pk: pk.to_owned(),
        op,
        payload,
        base_version: 0,
        updated_at: Utc::now(),
    }
}

fn push(device: &str, changes: Vec<Change>) -> PushRequest {
    PushRequest {
        device_id: device.to_owned(),
        changes,
    }
}

/// Assert the full backend contract. `backend` must be empty (fresh).
#[allow(clippy::too_many_lines)] // one linear conformance script, clearest unsplit
pub fn run_backend_conformance(backend: &dyn SyncBackend) {
    let resolver = LwwResolver;

    // ── Fresh backend ────────────────────────────────────────────────────
    assert_eq!(backend.latest_version().expect("latest"), 0);
    assert_eq!(backend.tombstone_horizon().expect("horizon"), 0);

    // ── Push applies with strictly increasing versions ───────────────────
    let seed = push(
        "device-a",
        vec![
            change(
                "00000000-0000-4000-8000-000000000001",
                "n1",
                Op::Upsert,
                Some(json!({"title": "one"})),
            ),
            change(
                "00000000-0000-4000-8000-000000000002",
                "n2",
                Op::Upsert,
                Some(json!({"title": "two"})),
            ),
        ],
    );
    let response = backend.apply_push(&seed, &resolver).expect("seed push");
    let versions: Vec<i64> = response
        .outcomes
        .iter()
        .map(|o| match o {
            ChangeOutcome::Applied { version } => *version,
            other => panic!("expected Applied, got {other:?}"),
        })
        .collect();
    assert!(versions[0] > 0);
    assert!(versions[1] > versions[0], "versions strictly increase");
    assert_eq!(backend.latest_version().expect("latest"), versions[1]);

    // ── Retrying the same batch is a no-op (at-least-once dedup) ─────────
    // AlreadyApplied must echo the ORIGINALLY assigned versions so a client
    // that lost the first response can still record its acks (otherwise its
    // next edit of the same row pushes a stale base_version and
    // false-conflicts).
    let retry = backend.apply_push(&seed, &resolver).expect("retry push");
    let retry_versions: Vec<i64> = retry
        .outcomes
        .iter()
        .map(|o| match o {
            ChangeOutcome::AlreadyApplied { version } => *version,
            other => panic!("retry must dedup, got {other:?}"),
        })
        .collect();
    assert_eq!(
        retry_versions, versions,
        "already_applied must carry the versions of the first application"
    );
    assert_eq!(backend.latest_version().expect("latest"), versions[1]);

    // ── Pull pages by version and honors the cursor ──────────────────────
    let PullResponse::Ok {
        rows,
        next_cursor,
        tombstone_horizon,
    } = backend.pull_since(0, 100, 0).expect("pull all")
    else {
        panic!("cursor 0 never requires a resync");
    };
    assert_eq!(rows.len(), 2);
    assert!(rows.windows(2).all(|w| w[0].version < w[1].version));
    assert_eq!(next_cursor, versions[1]);
    assert_eq!(tombstone_horizon, 0);

    let PullResponse::Ok {
        rows, next_cursor, ..
    } = backend
        .pull_since(versions[1], 100, versions[1])
        .expect("pull caught-up")
    else {
        panic!("caught-up cursor never requires a resync");
    };
    assert!(rows.is_empty());
    assert_eq!(next_cursor, versions[1], "empty page keeps the cursor");

    let PullResponse::Ok { rows, .. } = backend.pull_since(0, 1, 0).expect("pull limited") else {
        panic!("cursor 0 never requires a resync");
    };
    assert_eq!(rows.len(), 1, "limit caps the page");

    // ── Conflict: newer client write wins under LWW ──────────────────────
    // base_version = 0 on an EXISTING row is also the "two devices both
    // create the same pk" shape: the second first-insert MUST route through
    // the resolver (never silently overwrite the earlier write). On
    // Postgres this holds under concurrency too because apply_push batches
    // are serialized by an advisory lock — plain `SELECT … FOR UPDATE`
    // locks nothing for not-yet-committed rows.
    let mut winner = change(
        "00000000-0000-4000-8000-000000000003",
        "n1",
        Op::Upsert,
        Some(json!({"title": "one-b"})),
    );
    winner.base_version = 0; // stale: n1 is at versions[0]
    winner.updated_at = Utc::now() + Duration::seconds(30);
    let response = backend
        .apply_push(&push("device-b", vec![winner]), &resolver)
        .expect("conflict push");
    let ChangeOutcome::Resolved { row } = &response.outcomes[0] else {
        panic!(
            "stale base_version must resolve, got {:?}",
            response.outcomes[0]
        );
    };
    assert!(row.version > versions[1], "resolved rows get a NEW version");
    assert_eq!(
        row.payload
            .as_ref()
            .and_then(|p| p.get("title"))
            .and_then(|v| v.as_str()),
        Some("one-b"),
        "newer client write wins LWW"
    );
    let winner_version = row.version;

    // ── Conflict: older client write loses under LWW, still re-versioned ─
    let mut loser = change(
        "00000000-0000-4000-8000-000000000004",
        "n1",
        Op::Upsert,
        Some(json!({"title": "stale"})),
    );
    loser.base_version = versions[0]; // stale again
    loser.updated_at = Utc::now() - Duration::seconds(3600);
    let response = backend
        .apply_push(&push("device-c", vec![loser]), &resolver)
        .expect("losing conflict push");
    let ChangeOutcome::Resolved { row } = &response.outcomes[0] else {
        panic!(
            "stale base_version must resolve, got {:?}",
            response.outcomes[0]
        );
    };
    assert!(
        row.version > winner_version,
        "even KeepServer re-versions the row"
    );
    assert_eq!(
        row.payload
            .as_ref()
            .and_then(|p| p.get("title"))
            .and_then(|v| v.as_str()),
        Some("one-b"),
        "server content survives a losing push"
    );

    // ── Deletes are tombstones, visible in pull ──────────────────────────
    let mut delete = change(
        "00000000-0000-4000-8000-000000000005",
        "n2",
        Op::Delete,
        None,
    );
    delete.base_version = versions[1];
    let response = backend
        .apply_push(&push("device-a", vec![delete]), &resolver)
        .expect("delete push");
    assert!(matches!(
        response.outcomes[0],
        ChangeOutcome::Applied { .. }
    ));
    let PullResponse::Ok { rows, .. } = backend.pull_since(0, 100, 0).expect("pull with tombstone")
    else {
        panic!("cursor 0 never requires a resync");
    };
    let n2 = rows.iter().find(|r| r.pk == "n2").expect("n2 row");
    assert!(n2.deleted, "deletes replicate as tombstones");
    assert_eq!(n2.payload, None);

    // ── GC drops tombstones, advances the horizon, forces resyncs ────────
    let latest = backend.latest_version().expect("latest");
    let removed = backend.gc_tombstones(latest).expect("gc");
    assert_eq!(removed, 1);
    assert_eq!(backend.tombstone_horizon().expect("horizon"), latest);
    // GC is idempotent.
    assert_eq!(backend.gc_tombstones(latest).expect("re-gc"), 0);

    let PullResponse::Ok { rows, .. } = backend.pull_since(0, 100, 0).expect("pull post-gc") else {
        panic!("cursor 0 never requires a resync");
    };
    assert!(rows.iter().all(|r| !r.deleted), "GC'd tombstones are gone");
    let live_version = rows.first().expect("a live row survives GC").version;

    let stale = backend
        .pull_since(versions[0], 100, versions[0])
        .expect("stale pull");
    assert!(
        matches!(stale, PullResponse::FullResyncRequired { tombstone_horizon } if tombstone_horizon == latest),
        "a session starting behind the horizon must be told to resync, got {stale:?}"
    );

    // ── Mid-pagination is exempt from the staleness check ────────────────
    // A page cursor below the horizon with session_start = 0 is a fresh
    // device paging its first sync (or a resync in progress) — it started
    // AFTER the GC and cannot have missed a GC'd tombstone, so it must be
    // allowed to keep paging instead of being trapped in resync-from-0
    // forever.
    let mid_page = backend
        .pull_since(live_version, 100, 0)
        .expect("mid-pagination pull");
    assert!(
        matches!(mid_page, PullResponse::Ok { .. }),
        "a from-0 session paging past a sub-horizon cursor must get rows, got {mid_page:?}"
    );

    // ── Dedup-record GC bounds the applied table ──────────────────────────
    // Every applied change so far left one dedup record; an age-based GC
    // with a future cutoff removes them all, and re-running is a no-op.
    let removed = backend
        .gc_applied(Utc::now() + Duration::seconds(3600))
        .expect("gc applied");
    assert!(
        removed >= 5,
        "all dedup records older than the cutoff must go, removed {removed}"
    );
    assert_eq!(
        backend
            .gc_applied(Utc::now() + Duration::seconds(3600))
            .expect("re-gc applied"),
        0,
        "dedup GC is idempotent"
    );
}

#[test]
fn memory_backend_passes_conformance() {
    run_backend_conformance(&MemorySyncBackend::new());
}
