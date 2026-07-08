//! Shared conformance suite for `SyncBackend` implementations.
//!
//! Run against `MemorySyncBackend` here (always) and `PgSyncBackend` in
//! `offline_sync_pg` (docker-gated), so both backends prove the same
//! push/pull/dedup/conflict/GC and scope-isolation semantics.

#![cfg(feature = "offline-sync")]

use autumn_web::sync::{
    Change, ChangeOutcome, LwwResolver, MemorySyncBackend, Op, PullResponse, PushRequest,
    SyncBackend, SyncScope,
};
use chrono::{Duration, Utc};
use serde_json::json;

/// The single-tenant scope the linear script below runs in — what
/// `server::router` assigns when no auth middleware inserts a `SyncScope`.
/// The scope-isolation section at the end uses two extra tenant scopes.
const SCOPE: &str = SyncScope::GLOBAL;

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
    let response = backend
        .apply_push(SCOPE, &seed, &resolver)
        .expect("seed push");
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
    let retry = backend
        .apply_push(SCOPE, &seed, &resolver)
        .expect("retry push");
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
    } = backend.pull_since(SCOPE, 0, 100, 0).expect("pull all")
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
        .pull_since(SCOPE, versions[1], 100, versions[1])
        .expect("pull caught-up")
    else {
        panic!("caught-up cursor never requires a resync");
    };
    assert!(rows.is_empty());
    assert_eq!(next_cursor, versions[1], "empty page keeps the cursor");

    let PullResponse::Ok { rows, .. } = backend.pull_since(SCOPE, 0, 1, 0).expect("pull limited")
    else {
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
    let winner_request = push("device-b", vec![winner]);
    let response = backend
        .apply_push(SCOPE, &winner_request, &resolver)
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
    let winner_row = row.clone();

    // ── Retrying a RESOLVED change replays the resolution ────────────────
    // A lost response must not downgrade the outcome to a clean-looking
    // AlreadyApplied ack: the client would record the resolved version
    // without ever seeing the resolved row — its losing payload would stay
    // visible and its next edit (based on that version) would clean-apply
    // over the resolution (e.g. resurrect a server-winning delete).
    let latest_before_replay = backend.latest_version().expect("latest");
    let replay = backend
        .apply_push(SCOPE, &winner_request, &resolver)
        .expect("replay push");
    let ChangeOutcome::Resolved { row } = &replay.outcomes[0] else {
        panic!(
            "a retry of a Resolved change must replay Resolved, got {:?}",
            replay.outcomes[0]
        );
    };
    assert_eq!(
        row, &winner_row,
        "the replay must carry the originally resolved row"
    );
    assert_eq!(
        backend.latest_version().expect("latest"),
        latest_before_replay,
        "a replay must not assign versions"
    );

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
        .apply_push(SCOPE, &push("device-c", vec![loser]), &resolver)
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
        .apply_push(SCOPE, &push("device-a", vec![delete]), &resolver)
        .expect("delete push");
    assert!(matches!(
        response.outcomes[0],
        ChangeOutcome::Applied { .. }
    ));
    let PullResponse::Ok { rows, .. } = backend
        .pull_since(SCOPE, 0, 100, 0)
        .expect("pull with tombstone")
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

    let PullResponse::Ok { rows, .. } = backend.pull_since(SCOPE, 0, 100, 0).expect("pull post-gc")
    else {
        panic!("cursor 0 never requires a resync");
    };
    assert!(rows.iter().all(|r| !r.deleted), "GC'd tombstones are gone");
    let live_version = rows.first().expect("a live row survives GC").version;

    let stale = backend
        .pull_since(SCOPE, versions[0], 100, versions[0])
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
        .pull_since(SCOPE, live_version, 100, 0)
        .expect("mid-pagination pull");
    assert!(
        matches!(mid_page, PullResponse::Ok { .. }),
        "a from-0 session paging past a sub-horizon cursor must get rows, got {mid_page:?}"
    );

    // ── GC horizon is clamped to the newest committed version ────────────
    // A maintenance job may pass an arbitrarily large up_to ("everything so
    // far"). The persisted horizon must not run ahead of the change feed:
    // clients set cursor = max(next_cursor, tombstone_horizon) after a
    // completed pull, so an ahead-of-feed horizon would push cursors past
    // rows they have not seen — rows created later (versions <= horizon)
    // would then be permanently invisible to those clients. On Postgres
    // the bound must come from COMMITTED state observed under the push
    // advisory lock (the sequence is non-transactional: a concurrent
    // in-flight push's nextval is visible before its row commits); this
    // sequential suite pins the clamp value itself, and the lock-site
    // comment in sync/server.rs documents the concurrency guarantee.
    let latest_before_gc = backend.latest_version().expect("latest");
    backend.gc_tombstones(i64::MAX).expect("gc with huge up_to");
    assert_eq!(
        backend.tombstone_horizon().expect("horizon"),
        latest_before_gc,
        "the horizon must be clamped to the newest committed version"
    );

    // A client that completed a pull right after that GC sits at
    // max(next_cursor, horizon) == latest_before_gc. A row created AFTER the
    // GC must still reach it.
    let response = backend
        .apply_push(
            SCOPE,
            &push(
                "device-a",
                vec![change(
                    "00000000-0000-4000-8000-000000000006",
                    "n3",
                    Op::Upsert,
                    Some(json!({"title": "post-gc"})),
                )],
            ),
            &resolver,
        )
        .expect("post-gc push");
    let ChangeOutcome::Applied { version } = response.outcomes[0] else {
        panic!("expected Applied, got {:?}", response.outcomes[0]);
    };
    assert!(
        version > latest_before_gc,
        "new versions must land above the clamped horizon"
    );
    let PullResponse::Ok { rows, .. } = backend
        .pull_since(SCOPE, latest_before_gc, 100, latest_before_gc)
        .expect("post-gc pull")
    else {
        panic!("a cursor at the horizon never requires a resync");
    };
    assert!(
        rows.iter().any(|r| r.pk == "n3"),
        "a row created after a huge-up_to GC must be delivered, got {rows:?}"
    );

    // ── Upserts without a payload are protocol violations ────────────────
    // An upsert with payload = None would create a live row with a NULL
    // payload: clients materialize an invisible row (store get/list treat a
    // missing payload as absent) while still advancing their cursor. The
    // whole batch must be rejected atomically — including valid changes
    // sharing the batch — and leave no trace (no version burn is asserted
    // loosely: latest_version must not move).
    let latest_before_reject = backend.latest_version().expect("latest");
    let bad_batch = push(
        "device-a",
        vec![
            change(
                "00000000-0000-4000-8000-00000000000a",
                "n4",
                Op::Upsert,
                Some(json!({"title": "valid sibling"})),
            ),
            change(
                "00000000-0000-4000-8000-00000000000b",
                "n5",
                Op::Upsert,
                None,
            ),
        ],
    );
    let err = backend
        .apply_push(SCOPE, &bad_batch, &resolver)
        .expect_err("an upsert without a payload must be rejected");
    assert!(
        matches!(err, autumn_web::sync::SyncError::Protocol(_)),
        "payload-less upserts are protocol errors, got {err:?}"
    );
    assert_eq!(
        backend.latest_version().expect("latest"),
        latest_before_reject,
        "a rejected batch must not assign versions"
    );
    let PullResponse::Ok { rows, .. } = backend
        .pull_since(SCOPE, latest_before_reject, 100, latest_before_reject)
        .expect("pull after rejected push")
    else {
        panic!("a caught-up cursor never requires a resync");
    };
    assert!(
        rows.is_empty(),
        "nothing from a rejected batch may be applied, got {rows:?}"
    );
    // The valid sibling was not dedup-recorded either: re-pushing it alone
    // applies cleanly instead of echoing AlreadyApplied.
    let response = backend
        .apply_push(
            SCOPE,
            &push("device-a", vec![bad_batch.changes[0].clone()]),
            &resolver,
        )
        .expect("re-push of the valid sibling");
    assert!(
        matches!(response.outcomes[0], ChangeOutcome::Applied { .. }),
        "the valid sibling of a rejected batch must apply on retry, got {:?}",
        response.outcomes[0]
    );

    // ── Offline-past-GC pushes cannot silently resurrect deleted rows ────
    // n2 was deleted and its tombstone GC'd. A device offline since before
    // the GC can still push an edit of n2 based on its old version — and
    // pushes run BEFORE the pull that would demand a full resync. The row
    // is absent server-side, but the pre-horizon base_version claim dates
    // the edit: the outcome is DETERMINISTICALLY server-winning (the
    // resolver is bypassed — its input would be fabricated, and clock-based
    // policies could be gamed), never a clean apply.
    let latest_before_stale = backend.latest_version().expect("latest");
    let mut stale_edit = change(
        "00000000-0000-4000-8000-00000000000c",
        "n2",
        Op::Upsert,
        Some(json!({"title": "resurrected?"})),
    );
    stale_edit.base_version = versions[1]; // n2's pre-delete version (< horizon)
    stale_edit.updated_at = Utc::now() - Duration::seconds(3600);
    let stale_request = push("device-offline", vec![stale_edit]);
    let response = backend
        .apply_push(SCOPE, &stale_request, &resolver)
        .expect("stale push");
    let ChangeOutcome::Resolved { row } = &response.outcomes[0] else {
        panic!(
            "an offline-past-GC edit must resolve, not clean-apply, got {:?}",
            response.outcomes[0]
        );
    };
    assert!(
        row.deleted && row.payload.is_none(),
        "the GC'd deletion must win under LWW, got {row:?}"
    );
    assert!(
        row.version > latest_before_stale,
        "the resolution gets a fresh version so every device converges"
    );
    let stale_tombstone = row.clone();

    // A retry of the stale push (lost response) replays the SAME
    // server-winning tombstone — never a clean-looking AlreadyApplied.
    let replay = backend
        .apply_push(SCOPE, &stale_request, &resolver)
        .expect("stale replay");
    let ChangeOutcome::Resolved { row } = &replay.outcomes[0] else {
        panic!(
            "a retry of the stale push must replay Resolved, got {:?}",
            replay.outcomes[0]
        );
    };
    assert_eq!(
        row, &stale_tombstone,
        "the replay must carry the original tombstone"
    );
    let PullResponse::Ok { rows, .. } = backend
        .pull_since(SCOPE, latest_before_stale, 100, latest_before_stale)
        .expect("pull after stale push")
    else {
        panic!("an at-horizon cursor never requires a resync");
    };
    assert!(
        rows.iter().all(|r| r.pk != "n2" || r.deleted),
        "n2 must not come back as a live row: {rows:?}"
    );

    // Clock skew cannot resurrect either: GC the fresh tombstone away
    // again, then push an edit stamped FAR in the future. Under the default
    // LWW resolver a fast client clock would win a normal conflict — but
    // the GC'd-tombstone shape bypasses the resolver entirely, so the
    // deletion still sticks.
    let removed = backend
        .gc_tombstones(backend.latest_version().expect("latest"))
        .expect("re-gc the fresh tombstone");
    assert_eq!(removed, 1, "the resolution tombstone is GC'd again");
    let mut skewed_edit = change(
        "00000000-0000-4000-8000-00000000000e",
        "n2",
        Op::Upsert,
        Some(json!({"title": "clock cheat"})),
    );
    skewed_edit.base_version = versions[1];
    skewed_edit.updated_at = Utc::now() + Duration::days(365);
    let response = backend
        .apply_push(SCOPE, &push("device-skewed", vec![skewed_edit]), &resolver)
        .expect("skewed push");
    let ChangeOutcome::Resolved { row } = &response.outcomes[0] else {
        panic!(
            "a future-clocked stale edit must still resolve, got {:?}",
            response.outcomes[0]
        );
    };
    assert!(
        row.deleted && row.payload.is_none(),
        "a clock ahead of the server must not resurrect the GC'd row, got {row:?}"
    );

    // Repeat stale pushes hit the MATERIALIZED tombstone, not the absent
    // row — they must take the same server-winning bypass, or a clock-based
    // resolver would resurrect the row on the second attempt (the first
    // stale push wrote a real tombstone row, so the absent-row arm no
    // longer matches).
    for (attempt, change_id) in [
        "00000000-0000-4000-8000-000000000011",
        "00000000-0000-4000-8000-000000000012",
    ]
    .iter()
    .enumerate()
    {
        let mut repeat_edit = change(
            change_id,
            "n2",
            Op::Upsert,
            Some(json!({"title": "resurrected via repeat?"})),
        );
        repeat_edit.base_version = versions[1];
        repeat_edit.updated_at = Utc::now() + Duration::days(365);
        let response = backend
            .apply_push(SCOPE, &push("device-skewed", vec![repeat_edit]), &resolver)
            .expect("repeat stale push");
        let ChangeOutcome::Resolved { row } = &response.outcomes[0] else {
            panic!(
                "repeat stale push #{attempt} must resolve, got {:?}",
                response.outcomes[0]
            );
        };
        assert!(
            row.deleted && row.payload.is_none(),
            "repeat stale push #{attempt} must stay server-winning even with \
             a fast clock, got {row:?}"
        );
    }

    // A genuinely NEW insert from the same offline device (base_version = 0,
    // pk the server never saw) must still clean-apply — the rejection keys
    // on the base-version claim, not on mere absence.
    let response = backend
        .apply_push(
            SCOPE,
            &push(
                "device-offline",
                vec![change(
                    "00000000-0000-4000-8000-00000000000d",
                    "brand-new",
                    Op::Upsert,
                    Some(json!({"title": "fresh insert"})),
                )],
            ),
            &resolver,
        )
        .expect("fresh insert push");
    assert!(
        matches!(response.outcomes[0], ChangeOutcome::Applied { .. }),
        "a base-0 insert must stay a clean apply, got {:?}",
        response.outcomes[0]
    );

    // ── Explicit-null payloads are real documents ─────────────────────────
    // `store.put(&None::<T>)` journals the JSON document `null`. On the
    // wire that is a PRESENT null — it must clean-apply (not be rejected as
    // payload-omitted) and pull back intact as Some(Null), not materialize
    // as an absent row.
    let response = backend
        .apply_push(
            SCOPE,
            &push(
                "device-a",
                vec![change(
                    "00000000-0000-4000-8000-00000000000f",
                    "nullable",
                    Op::Upsert,
                    Some(serde_json::Value::Null),
                )],
            ),
            &resolver,
        )
        .expect("null-payload push");
    let ChangeOutcome::Applied {
        version: null_version,
    } = response.outcomes[0]
    else {
        panic!(
            "a null-payload upsert is a valid document, got {:?}",
            response.outcomes[0]
        );
    };
    let PullResponse::Ok { rows, .. } = backend
        .pull_since(SCOPE, null_version - 1, 100, null_version - 1)
        .expect("pull null row")
    else {
        panic!("an at-feed cursor never requires a resync");
    };
    let null_row = rows
        .iter()
        .find(|r| r.pk == "nullable")
        .expect("the null-payload row must be pulled");
    assert!(!null_row.deleted, "a null payload is not a tombstone");
    assert_eq!(
        null_row.payload,
        Some(serde_json::Value::Null),
        "the null document must survive the wire intact"
    );

    // And a Resolved outcome carrying a null payload replays intact too
    // (exercises the dedup snapshot's JSONB round-trip on Postgres).
    let mut null_conflict = change(
        "00000000-0000-4000-8000-000000000010",
        "nullable",
        Op::Upsert,
        Some(serde_json::Value::Null),
    );
    null_conflict.base_version = 0; // stale: the row is at null_version
    null_conflict.updated_at = Utc::now() + Duration::seconds(60);
    let null_request = push("device-b", vec![null_conflict]);
    let response = backend
        .apply_push(SCOPE, &null_request, &resolver)
        .expect("null conflict push");
    let ChangeOutcome::Resolved { row } = &response.outcomes[0] else {
        panic!("stale base must resolve, got {:?}", response.outcomes[0]);
    };
    assert_eq!(row.payload, Some(serde_json::Value::Null));
    let resolved_null_row = row.clone();
    let replay = backend
        .apply_push(SCOPE, &null_request, &resolver)
        .expect("null replay");
    let ChangeOutcome::Resolved { row } = &replay.outcomes[0] else {
        panic!(
            "the retry must replay Resolved, got {:?}",
            replay.outcomes[0]
        );
    };
    assert_eq!(
        row, &resolved_null_row,
        "the dedup snapshot must round-trip a null payload"
    );

    // ── Scope isolation: tenants never see or touch each other's data ────
    // Everything above ran in the single-tenant GLOBAL scope. The scope is
    // derived server-side from the authenticated request (never
    // client-supplied), and it partitions rows, tombstones, AND dedup
    // records: the same (collection, pk) in two scopes is two independent
    // rows, and the same client-supplied (device_id, change_id) in two
    // scopes is two independent changes.
    let tenant_change = |change_id: &str, title: &str| Change {
        collection: "scoped".to_owned(),
        ..change(change_id, "s1", Op::Upsert, Some(json!({"title": title})))
    };
    let shared_change_id = "00000000-0000-4000-8000-000000000020";
    let response = backend
        .apply_push(
            "tenant-a",
            &push("device-a", vec![tenant_change(shared_change_id, "alpha")]),
            &resolver,
        )
        .expect("tenant-a push");
    let ChangeOutcome::Applied { version: version_a } = response.outcomes[0] else {
        panic!(
            "tenant-a push must clean-apply, got {:?}",
            response.outcomes[0]
        );
    };
    // Same device_id AND change_id, same collection/pk — but ANOTHER scope:
    // a fresh change (scoped dedup — no AlreadyApplied echo of tenant-a's
    // record) creating a fresh row (scoped pk — no conflict against
    // tenant-a's row, whose version differs from this base-0 claim).
    let response = backend
        .apply_push(
            "tenant-b",
            &push("device-a", vec![tenant_change(shared_change_id, "beta")]),
            &resolver,
        )
        .expect("tenant-b push");
    let ChangeOutcome::Applied { version: version_b } = response.outcomes[0] else {
        panic!(
            "the same device/change/pk in another scope is a distinct fresh \
             row and change, got {:?}",
            response.outcomes[0]
        );
    };
    assert!(version_b > version_a, "one global sequence spans scopes");

    // Pull from cursor 0 returns ONLY the requesting scope's rows — never
    // the GLOBAL-scope rows above, never the sibling tenant's row.
    for (scope, version, title) in [
        ("tenant-a", version_a, "alpha"),
        ("tenant-b", version_b, "beta"),
    ] {
        let PullResponse::Ok {
            rows, next_cursor, ..
        } = backend.pull_since(scope, 0, 100, 0).expect("tenant pull")
        else {
            panic!("cursor 0 never requires a resync");
        };
        assert_eq!(
            rows.len(),
            1,
            "{scope} must see exactly its own row, got {rows:?}"
        );
        assert_eq!(rows[0].version, version);
        assert_eq!(
            rows[0]
                .payload
                .as_ref()
                .and_then(|p| p.get("title"))
                .and_then(|v| v.as_str()),
            Some(title),
            "{scope} must pull its own payload"
        );
        assert_eq!(next_cursor, version);
    }

    // A dedup replay stays within its scope: retrying tenant-a's change
    // echoes tenant-a's original version, not tenant-b's.
    let replay = backend
        .apply_push(
            "tenant-a",
            &push("device-a", vec![tenant_change(shared_change_id, "alpha")]),
            &resolver,
        )
        .expect("tenant-a replay");
    assert!(
        matches!(
            replay.outcomes[0],
            ChangeOutcome::AlreadyApplied { version } if version == version_a
        ),
        "the dedup record must be scope-local, got {:?}",
        replay.outcomes[0]
    );

    // A push can never touch another scope's row: a conflicting edit in
    // tenant-b (its base_version even names tenant-a's version) resolves
    // against tenant-b's OWN row only.
    let mut cross = tenant_change("00000000-0000-4000-8000-000000000021", "tenant-b wins");
    cross.base_version = version_a; // stale IN TENANT-B (its row is at version_b)
    cross.updated_at = Utc::now() + Duration::seconds(60);
    let response = backend
        .apply_push("tenant-b", &push("device-x", vec![cross]), &resolver)
        .expect("cross-scope-shaped push");
    let ChangeOutcome::Resolved { row } = &response.outcomes[0] else {
        panic!(
            "a stale edit must resolve within its own scope, got {:?}",
            response.outcomes[0]
        );
    };
    let tenant_b_version = row.version;

    // Deletes are scoped tombstones: deleting s1 in tenant-b leaves
    // tenant-a's s1 live.
    let del = Change {
        collection: "scoped".to_owned(),
        base_version: tenant_b_version,
        ..change(
            "00000000-0000-4000-8000-000000000022",
            "s1",
            Op::Delete,
            None,
        )
    };
    let response = backend
        .apply_push("tenant-b", &push("device-a", vec![del]), &resolver)
        .expect("tenant-b delete");
    assert!(matches!(
        response.outcomes[0],
        ChangeOutcome::Applied { .. }
    ));
    let PullResponse::Ok { rows, .. } = backend
        .pull_since("tenant-b", 0, 100, 0)
        .expect("tenant-b post-delete pull")
    else {
        panic!("cursor 0 never requires a resync");
    };
    assert!(
        rows.len() == 1 && rows[0].deleted,
        "tenant-b's row is a tombstone, got {rows:?}"
    );
    let PullResponse::Ok { rows, .. } = backend
        .pull_since("tenant-a", 0, 100, 0)
        .expect("tenant-a post-delete pull")
    else {
        panic!("cursor 0 never requires a resync");
    };
    assert!(
        rows.len() == 1
            && !rows[0].deleted
            && rows[0].version == version_a
            && rows[0]
                .payload
                .as_ref()
                .and_then(|p| p.get("title"))
                .and_then(|v| v.as_str())
                == Some("alpha"),
        "tenant-a's row must be untouched by tenant-b's writes, got {rows:?}"
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
