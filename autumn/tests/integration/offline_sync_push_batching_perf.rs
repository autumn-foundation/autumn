//! Ledger perf harness for `PgSyncBackend::apply_push` — profiles Postgres
//! round trips and buffers against a production-shaped fixture, through the
//! real `SyncBackend::apply_push` entry point (the exact call `server::router`
//! makes from `POST /push`).
//!
//! **Requires Docker.** CI runs it in the Docker-dependent sweep
//! (`-- --ignored`, see CLAUDE.md). Run manually with:
//!
//! ```text
//! cargo test -p autumn-web --features "test-support,offline-sync" \
//!   --test integration_tests -- --ignored offline_sync_push_batching_profile --nocapture
//! ```
//!
//! The committed `docs/perf/sync-push-batching/{baseline,after}.txt` files
//! are this command's stdout, captured before and after the batching change
//! in `autumn/src/sync/server.rs`.
//!
//! ## Fixture
//!
//! 20,000 pre-existing rows in one scope: six collections at a realistic
//! frequency skew (40/20/15/15/7/3%), 5% tombstones, variable-length JSON
//! payloads (some fields NULL), pks `pk-00000001` .. `pk-00020000`. A
//! follow-up `UPDATE` on 20% of rows creates real dead tuples before
//! `ANALYZE`, so the planner sees the same statistics shape a mature table
//! would have — not the day-one shape of a freshly bulk-loaded table.
//!
//! ## Workload
//!
//! Three push batches (50 / 250 / 1000 changes — 1000 is
//! `protocol::MAX_PUSH_CHANGES`) against disjoint slices of the fixture, each
//! shaped like a device catching up after being offline: 65% edits of
//! existing rows, 20% new inserts, 5% deletes, 8% two edits of the *same*
//! existing pk in one batch (queued local edits, both against the last known
//! server version), 2% two edits of the *same brand-new* pk in one batch
//! (create-then-edit before ever syncing — the resolver runs on the second).
//! All timestamps are fixed constants, not wall-clock, so two runs of this
//! harness against identical code produce byte-identical `PushRequest`s and
//! byte-identical resulting rows — that determinism is what makes the
//! before/after row-dump diff in `equivalence` a real proof and not a
//! coincidence.

#![cfg(feature = "offline-sync")]

use autumn_web::sync::{
    Change, ChangeOutcome, LwwResolver, Op, PgSyncBackend, PushRequest, SyncBackend, SyncScope,
};
use chrono::{DateTime, TimeZone, Utc};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sql_types::{BigInt, Text};
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const SCOPE: &str = SyncScope::GLOBAL;
const SEED_ROWS: usize = 20_000;

fn base_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn seeded_pk(i: usize) -> String {
    format!("pk-{i:08}")
}

/// The collection `seed_fixture`'s `CASE` assigns index `i` to — mirrored
/// here so changes referencing an existing seeded pk use the SAME
/// `(collection, pk)` key the fixture actually wrote, not a mismatched pair
/// that reads back as "never existed".
fn seeded_collection(i: usize) -> &'static str {
    match i % 100 {
        0..=39 => "notes",
        40..=59 => "contacts",
        60..=74 => "tasks",
        75..=89 => "files",
        90..=96 => "tags",
        _ => "archive",
    }
}

/// Seed the production-shaped fixture and leave the planner with real
/// statistics (`ANALYZE`, and real dead tuples from a follow-up `UPDATE`).
fn seed_fixture(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO autumn_sync_rows \
         (scope, collection, pk, payload, version, deleted, updated_at, device_id, created_version) \
         SELECT \
           '{SCOPE}', \
           CASE WHEN i % 100 < 40 THEN 'notes' \
                WHEN i % 100 < 60 THEN 'contacts' \
                WHEN i % 100 < 75 THEN 'tasks' \
                WHEN i % 100 < 90 THEN 'files' \
                WHEN i % 100 < 97 THEN 'tags' \
                ELSE 'archive' END, \
           'pk-' || lpad(i::text, 8, '0'), \
           jsonb_build_object( \
             'title', repeat(md5(i::text), 1 + i % 10), \
             'n', i, \
             'note', CASE WHEN i % 7 = 0 THEN NULL ELSE md5((i * 7)::text) END \
           ), \
           i, \
           (i % 20 = 0), \
           TIMESTAMPTZ '2025-01-01 00:00:00Z' + (i || ' seconds')::interval, \
           'seed-device', \
           i \
         FROM generate_series(1, {SEED_ROWS}) AS i"
    ))
    .expect("seed autumn_sync_rows");
    conn.batch_execute(&format!(
        "SELECT setval('autumn_sync_version_seq', {SEED_ROWS}, true)"
    ))
    .expect("advance version sequence past seeded rows");
    // Real dead tuples: supersede 20% of rows' device_id before ANALYZE, the
    // same way a mature (not freshly bulk-loaded) table accumulates them.
    conn.batch_execute("UPDATE autumn_sync_rows SET device_id = 'seed-device-2' WHERE right(pk, 1) IN ('0', '1')")
        .expect("create dead tuples");
    conn.batch_execute("ANALYZE autumn_sync_rows, autumn_sync_applied, autumn_sync_horizons")
        .expect("analyze");
}

/// One push batch shaped like a device catching up after being offline,
/// built from a disjoint slice `[offset+1, offset+need]` of the seeded pk
/// space so concurrent/sequential batches in the same fixture never touch
/// the same row.
struct Batch {
    request: PushRequest,
    /// `(index, expect_conflict)` for every existing-pk change, so the
    /// caller can assert outcomes precisely.
    existing_indices: Vec<usize>,
}

#[allow(clippy::arithmetic_side_effects)] // test-only fixture arithmetic on small constants
fn build_batch(device_id: &str, m: usize, offset: usize) -> Batch {
    // Fixed proportions per the module doc: 65/20/5/8/2, expressed as exact
    // per-size counts (computed once here rather than via integer division
    // at three different `m`, so the split is exact instead of rounded).
    let (updates_n, inserts_n, deletes_n, dup_existing_pairs, dup_new_pairs) = match m {
        50 => (30, 10, 4, 2, 1),
        250 => (160, 50, 12, 10, 4),
        1000 => (650, 200, 50, 40, 10),
        _ => unreachable!("fixed batch sizes only"),
    };
    assert_eq!(
        updates_n + inserts_n + deletes_n + dup_existing_pairs * 2 + dup_new_pairs * 2,
        m,
        "batch bucket sizes must sum to the batch size"
    );

    let base = base_time();
    let mut changes = Vec::with_capacity(m);
    let mut existing_indices = Vec::new();
    let mut next_idx = offset + 1;
    let mut seq = 0u32;
    let mut next_change_id = || {
        seq += 1;
        format!("{device_id}-chg-{seq:06}")
    };
    // `seed_fixture` tombstones every 20th index. Skip those here: a change
    // whose base_version equals a TOMBSTONE's version clean-applies as a
    // revival and starts a NEW incarnation — correct behavior, but it turns
    // a same-pk dup pair (see below) into the deterministic
    // previous-incarnation KeepServer arm instead of the ordinary resolver
    // path, which the plain-update/delete buckets don't need to model. Live
    // rows keep every bucket's expected outcome simple and uniform.
    let mut next_live_idx = || loop {
        let idx = next_idx;
        next_idx += 1;
        if idx % 20 != 0 {
            return idx;
        }
    };

    // 65%: clean edit of an existing row, based on its seeded version.
    for _ in 0..updates_n {
        let idx = next_live_idx();
        existing_indices.push(idx);
        changes.push(Change {
            change_id: next_change_id(),
            collection: seeded_collection(idx).to_owned(),
            pk: seeded_pk(idx),
            op: Op::Upsert,
            payload: Some(serde_json::json!({"edit": "update", "idx": idx})),
            base_version: idx as i64,
            updated_at: base + chrono::Duration::seconds(1_000_000 + idx as i64),
        });
    }
    // 20%: brand-new pk, never seen by the server.
    for k in 0..inserts_n {
        changes.push(Change {
            change_id: next_change_id(),
            collection: "notes".to_owned(),
            pk: format!("new-{device_id}-{k:06}"),
            op: Op::Upsert,
            payload: Some(serde_json::json!({"edit": "insert", "k": k})),
            base_version: 0,
            updated_at: base + chrono::Duration::seconds(2_000_000 + k as i64),
        });
    }
    // 5%: delete of an existing row.
    for _ in 0..deletes_n {
        let idx = next_live_idx();
        existing_indices.push(idx);
        changes.push(Change {
            change_id: next_change_id(),
            collection: seeded_collection(idx).to_owned(),
            pk: seeded_pk(idx),
            op: Op::Delete,
            payload: None,
            base_version: idx as i64,
            updated_at: base + chrono::Duration::seconds(3_000_000 + idx as i64),
        });
    }
    // 8%: two queued local edits of the SAME existing pk in one batch, both
    // against the last-known (seeded) server version — the second collides
    // with the first's freshly-assigned version and the resolver decides.
    for p in 0..dup_existing_pairs {
        let idx = next_live_idx();
        existing_indices.push(idx);
        let pk = seeded_pk(idx);
        let collection = seeded_collection(idx).to_owned();
        changes.push(Change {
            change_id: next_change_id(),
            collection: collection.clone(),
            pk: pk.clone(),
            op: Op::Upsert,
            payload: Some(serde_json::json!({"edit": "dup-existing-1", "p": p})),
            base_version: idx as i64,
            updated_at: base + chrono::Duration::seconds(4_000_000 + p as i64 * 10),
        });
        changes.push(Change {
            change_id: next_change_id(),
            collection,
            pk,
            op: Op::Upsert,
            payload: Some(serde_json::json!({"edit": "dup-existing-2", "p": p})),
            base_version: idx as i64,
            // Strictly later than the first edit's timestamp: LwwResolver
            // deterministically picks this (the client / second edit) as
            // the winner, so the outcome is knowable ahead of time.
            updated_at: base + chrono::Duration::seconds(4_000_000 + p as i64 * 10 + 5),
        });
    }
    // 2%: create-then-edit of a brand-new pk within the same batch.
    for p in 0..dup_new_pairs {
        let pk = format!("newdup-{device_id}-{p:06}");
        changes.push(Change {
            change_id: next_change_id(),
            collection: "notes".to_owned(),
            pk: pk.clone(),
            op: Op::Upsert,
            payload: Some(serde_json::json!({"edit": "dup-new-1", "p": p})),
            base_version: 0,
            updated_at: base + chrono::Duration::seconds(5_000_000 + p as i64 * 10),
        });
        changes.push(Change {
            change_id: next_change_id(),
            collection: "notes".to_owned(),
            pk,
            op: Op::Upsert,
            payload: Some(serde_json::json!({"edit": "dup-new-2", "p": p})),
            base_version: 0,
            updated_at: base + chrono::Duration::seconds(5_000_000 + p as i64 * 10 + 5),
        });
    }

    assert_eq!(changes.len(), m);
    Batch {
        request: PushRequest {
            device_id: device_id.to_owned(),
            changes,
        },
        existing_indices,
    }
}

/// Every outcome for a [`build_batch`] request must be `Applied` (clean
/// create/update/delete) or `Resolved` (the second half of a dup pair,
/// always won by the later-`updated_at` write — see `build_batch`), never
/// `AlreadyApplied` (that only happens on retry, exercised separately by
/// [`replay_is_idempotent`]).
fn assert_clean_outcomes(request: &PushRequest, outcomes: &[ChangeOutcome]) {
    assert_eq!(outcomes.len(), request.changes.len());
    for (change, outcome) in request.changes.iter().zip(outcomes) {
        // build_batch tags the second half of every dup pair with an
        // "edit" payload field ending "-2" — that's the only change in the
        // batch expected to collide with an earlier change's fresh write.
        let is_second_of_dup_pair = change
            .payload
            .as_ref()
            .and_then(|p| p.get("edit"))
            .and_then(|v| v.as_str())
            .is_some_and(|edit| edit.ends_with("-2"));
        match outcome {
            ChangeOutcome::Applied { .. } => {
                assert!(
                    !is_second_of_dup_pair,
                    "expected the second half of a dup pair to conflict (Resolved), \
                     got Applied for {}",
                    change.change_id
                );
            }
            ChangeOutcome::Resolved { row } => {
                assert!(
                    is_second_of_dup_pair,
                    "unexpected Resolved outcome for {}",
                    change.change_id
                );
                assert_eq!(
                    change.payload.as_ref(),
                    row.payload.as_ref(),
                    "LwwResolver must pick the later (client/second) write for {}",
                    change.change_id
                );
            }
            ChangeOutcome::AlreadyApplied { .. } => {
                panic!("unexpected AlreadyApplied on a first push: {}", change.change_id);
            }
        }
    }
}

#[derive(QueryableByName, Debug)]
struct StatementRow {
    #[diesel(sql_type = Text)]
    query: String,
    #[diesel(sql_type = BigInt)]
    calls: i64,
    #[diesel(sql_type = BigInt)]
    buffers: i64,
}

fn reset_stats(conn: &mut PgConnection) {
    conn.batch_execute("SELECT pg_stat_statements_reset()")
        .expect("reset pg_stat_statements");
}

fn print_profile(conn: &mut PgConnection, label: &str) -> i64 {
    println!("\n=== pg_stat_statements: {label} ===");
    let by_calls = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE (query ILIKE '%autumn_sync%' OR query ILIKE '%nextval%' \
                OR query ILIKE '%generate_series%') \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC LIMIT 10",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements by calls");
    let mut total_calls = 0i64;
    let mut total_buffers = 0i64;
    println!("-- top by calls --");
    for row in &by_calls {
        total_calls += row.calls;
        total_buffers += row.buffers;
        println!(
            "calls={:<6} buffers={:<6} {}",
            row.calls,
            row.buffers,
            row.query.split_whitespace().collect::<Vec<_>>().join(" ")
        );
    }
    let by_buffers = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE (query ILIKE '%autumn_sync%' OR query ILIKE '%nextval%' \
                OR query ILIKE '%generate_series%') \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY buffers DESC LIMIT 10",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements by buffers");
    println!("-- top by buffers --");
    for row in &by_buffers {
        println!(
            "buffers={:<6} calls={:<6} {}",
            row.buffers,
            row.calls,
            row.query.split_whitespace().collect::<Vec<_>>().join(" ")
        );
    }
    println!(
        "TOTAL sync-table statement calls={total_calls} buffers={total_buffers} (this batch, all statement shapes)"
    );
    total_calls
}

#[derive(QueryableByName, Debug)]
struct ExplainLine {
    #[diesel(sql_type = Text, column_name = "QUERY PLAN")]
    line: String,
}

fn explain(conn: &mut PgConnection, label: &str, sql: &str) {
    println!("\n=== EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS): {label} ===");
    println!("{sql}");
    let lines = diesel::sql_query(format!(
        "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) {sql}"
    ))
    .load::<ExplainLine>(conn)
    .expect("explain");
    for line in lines {
        println!("{}", line.line);
    }
}

/// Dump the full scope, sorted deterministically (`collection, pk` is the
/// table's primary key suffix — no ties), as the result-equivalence
/// snapshot: run against baseline and after code with the *same* fixture and
/// the *same* pushes, this must be byte-identical.
#[derive(QueryableByName, Debug, PartialEq, Eq)]
struct RowDump {
    #[diesel(sql_type = Text)]
    collection: String,
    #[diesel(sql_type = Text)]
    pk: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<Text>)]
    payload: Option<String>,
    #[diesel(sql_type = BigInt)]
    version: i64,
    #[diesel(sql_type = diesel::sql_types::Bool)]
    deleted: bool,
    #[diesel(sql_type = Text)]
    device_id: String,
    #[diesel(sql_type = BigInt)]
    created_version: i64,
}

fn dump_scope(conn: &mut PgConnection, prefix: &str) -> Vec<RowDump> {
    diesel::sql_query(format!(
        "SELECT collection, pk, payload::text AS payload, version, deleted, device_id, created_version \
         FROM autumn_sync_rows WHERE scope = '{SCOPE}' AND pk LIKE '{prefix}%' \
         ORDER BY collection, pk"
    ))
    .load::<RowDump>(conn)
    .expect("dump scope")
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn offline_sync_push_batching_profile() {
    // `shared_preload_libraries` must be set at server start, so it goes on
    // the container command rather than `CREATE EXTENSION` alone — the
    // module fixes `-c fsync=off`; this overrides the whole cmd, so it's
    // repeated here to keep container startup fast. Pinned to a current
    // major version (the module defaults to 11-alpine, which predates
    // `EXPLAIN (... SETTINGS)`, added in PG12) — 16 matches this host's
    // installed server.
    let container = Postgres::default()
        .with_tag("16-alpine")
        .with_cmd([
            "-c",
            "fsync=off",
            "-c",
            "shared_preload_libraries=pg_stat_statements",
            "-c",
            "pg_stat_statements.track=all",
            "-c",
            "pg_stat_statements.max=2000",
        ])
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let backend = PgSyncBackend::new(url.clone());
    backend.ensure_schema().expect("ensure schema");

    let mut conn = PgConnection::establish(&url).expect("db connection");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .expect("create pg_stat_statements extension");

    seed_fixture(&mut conn);

    let resolver = LwwResolver;
    let sizes_and_offsets = [(50usize, 0usize), (250, 5_000), (1000, 10_000)];
    let mut calls_by_size = Vec::new();

    for (size, offset) in sizes_and_offsets {
        let device_id = format!("device-{size}");
        let batch = build_batch(&device_id, size, offset);
        reset_stats(&mut conn);
        let outcomes = backend
            .apply_push(SCOPE, &batch.request, &resolver)
            .expect("apply_push")
            .outcomes;
        assert_clean_outcomes(&batch.request, &outcomes);
        let total_calls = print_profile(&mut conn, &format!("push batch of {size} changes"));
        calls_by_size.push((size, total_calls));

        // Result-equivalence artifact: dump every row this batch could have
        // touched (new + newdup pks share the device-scoped prefix; existing
        // pks are the disjoint offset slice), sorted deterministically.
        let dump = dump_scope(&mut conn, &format!("new%-{device_id}-"));
        println!("-- row dump (new/newdup pks), {} rows --", dump.len());
        for row in &dump {
            println!("{row:?}");
        }
        let existing_dump: Vec<RowDump> = diesel::sql_query(format!(
            "SELECT collection, pk, payload::text AS payload, version, deleted, device_id, created_version \
             FROM autumn_sync_rows WHERE scope = '{SCOPE}' AND pk = ANY($1) ORDER BY pk"
        ))
        .bind::<diesel::sql_types::Array<Text>, _>(
            batch
                .existing_indices
                .iter()
                .map(|i| seeded_pk(*i))
                .collect::<Vec<_>>(),
        )
        .load(&mut conn)
        .expect("dump existing pks");
        println!(
            "-- row dump (existing pks touched by this batch), {} rows --",
            existing_dump.len()
        );
        for row in &existing_dump {
            println!("{row:?}");
        }
    }

    println!("\n=== statement-count scaling ===");
    for (size, calls) in &calls_by_size {
        println!("batch size={size:<5} total sync-table statement calls={calls}");
    }

    // Retry/idempotency: replay the 250-change batch verbatim. Every outcome
    // must be AlreadyApplied (clean applies) or a replayed Resolved (dup
    // pairs) — never a second Applied, and nothing may be re-written.
    let (replay_size, replay_offset) = (250, 5_000);
    let replay_device = format!("device-{replay_size}");
    let replay_batch = build_batch(&replay_device, replay_size, replay_offset);
    reset_stats(&mut conn);
    let replay_outcomes = backend
        .apply_push(SCOPE, &replay_batch.request, &resolver)
        .expect("replay apply_push")
        .outcomes;
    for (change, outcome) in replay_batch.request.changes.iter().zip(&replay_outcomes) {
        assert!(
            matches!(
                outcome,
                ChangeOutcome::AlreadyApplied { .. } | ChangeOutcome::Resolved { .. }
            ),
            "replay of {} must not apply again, got {outcome:?}",
            change.change_id
        );
    }
    print_profile(&mut conn, "replay of the 250-change batch (idempotency)");

    // Representative EXPLAIN output: the two per-change point lookups that
    // dominate `calls` (dedup check, current-row fetch) — both look trivial
    // in isolation (single-row index lookups), which is exactly why they
    // never show up as expensive in a plan-shape review and only show up in
    // the `calls` ranking.
    explain(
        &mut conn,
        "dedup check (typical case: miss)",
        &format!(
            "SELECT version, resolved_row FROM autumn_sync_applied \
             WHERE scope = '{SCOPE}' AND device_id = 'device-1000' AND change_id = 'device-1000-chg-000001'"
        ),
    );
    explain(
        &mut conn,
        "current-row fetch FOR UPDATE",
        &format!(
            "SELECT collection, pk, payload, version, deleted, updated_at, device_id, created_version \
             FROM autumn_sync_rows WHERE scope = '{SCOPE}' AND collection = '{}' AND pk = '{}' FOR UPDATE",
            seeded_collection(10_001),
            seeded_pk(10_001)
        ),
    );
}
