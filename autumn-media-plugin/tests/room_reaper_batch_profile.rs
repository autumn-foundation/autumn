//! Ledger profiling harness for `DbRoomStore::reap_stale`'s phase-2 loop
//! (`autumn-media-plugin/src/rooms_db.rs`), driven through the real
//! `RoomStore::reap_stale` entry point — the same call the background reaper
//! (`spawn_room_reaper_loop`, `rooms.rs`) makes once per tick against every
//! `DbRoomStore`-backed deployment.
//!
//! Phase 2 first loads every room whose `created_at` is older than the
//! idle-TTL cutoff (one query), then — for EACH candidate — runs a separate
//! `SELECT COUNT(*)` against `media_room_participants` and, if the count is
//! zero, a separate `DELETE`. That is 1 + 2N statements for N stale-room
//! candidates: individually trivial (a composite-PK-indexed point lookup),
//! collectively dominant, and invisible in a `pg_stat_statements` ranking
//! sorted by buffers, exactly the "workflow bookkeeping" shape called out in
//! CLAUDE.md's Ledger process notes. The reaper runs unconditionally every
//! `ROOM_REAPER_INTERVAL_SECONDS` (60s default) in any process wiring in the
//! DB-backed room store, so N is bounded only by how many mesh rooms went
//! stale between ticks — busy multi-tenant deployments see this scale with
//! traffic, not with any operator action.
//!
//! **Requires Docker.** Not currently swept by any CI job — unlike
//! `autumn-web`'s consolidated `integration_tests` binary, autumn-media-plugin
//! has no `--ignored` sweep at all (`room_store_db.rs`'s own Docker tests
//! aren't wired into `ci.yml` either). Run manually with:
//!
//! ```text
//! cargo test -p autumn-media-plugin --test room_reaper_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! 8,700 rooms across 40 namespaces (skewed: 8 "busy" namespaces hold 70% of
//! rooms, the other 32 share the long-tail 30%), split into the five states
//! the reaper's two-phase contract must tell apart:
//!
//! | category | count | `created_at` | participants | expected outcome |
//! |---|---:|---|---|---|
//! | `stale_empty` | 6,000 | `< cutoff` | none | reaped |
//! | `stale_all_participants_stale` | 1,500 | `< cutoff` | 3 each, all `last_seen_at < cutoff` | phase 1 empties it, phase 2 reaps it |
//! | `stale_with_fresh_participant` | 500 | `< cutoff` | 1 stale + 1 fresh (`last_seen_at >= cutoff`) | kept (still occupied) |
//! | `fresh_empty` | 300 | `>= cutoff` | none | kept (create→first-join window) |
//! | `fresh_with_participants` | 400 | `>= cutoff` | 2 each, mixed staleness | kept (room itself not a candidate) |
//!
//! Plus explicit boundary/edge rows seeded individually: a room whose
//! `created_at` sits exactly ON the cutoff (predicate is `.lt`, so it must
//! survive), a stale room with exactly one participant whose `last_seen_at`
//! sits exactly ON the cutoff (`.lt` again — must survive), and a duplicate
//! `room_id` ("general") reused across two different namespaces in opposite
//! states, to prove the composite `(namespace, room_id)` key never lets one
//! tenant's reap decision leak into another's.
//!
//! 8,002 candidate rooms (`stale_*` above, plus 2 stale boundary rows) drive
//! the N+1: baseline issues 1 (candidate scan) + 8,002 (one `COUNT(*)` per
//! candidate) + up to 7,501 (one `DELETE` per now-empty candidate) = up to
//! 15,504 statements in phase 2 alone.
//!
//! Real dead-tuple ratio: every `stale_all_participants_stale` participant
//! row gets a second `last_seen_at`-touching `UPDATE` after the initial seed
//! (simulating a heartbeat bump before the row eventually goes stale), and
//! `ANALYZE` runs without an intervening `VACUUM`, so planner stats reflect
//! the dead tuples rather than a pristine table.

#![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]

use std::sync::Arc;

use autumn_media_plugin::rooms::RoomStore;
use autumn_media_plugin::rooms_db::DbRoomStore;
use chrono::{DateTime, Duration, Utc};
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName, RunQueryDsl as _};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// Matches `migrations/20260720000000_media_rooms/up.sql` exactly.
const CREATE_TABLES_SQL: &str = "
    CREATE TABLE media_rooms (
        namespace         TEXT      NOT NULL,
        room_id           TEXT      NOT NULL,
        max_participants  INTEGER   NOT NULL,
        created_at        TIMESTAMP NOT NULL,
        PRIMARY KEY (namespace, room_id)
    );
    CREATE TABLE media_room_participants (
        namespace         TEXT      NOT NULL,
        room_id           TEXT      NOT NULL,
        participant_id    TEXT      NOT NULL,
        display_name      TEXT,
        token             TEXT      NOT NULL,
        joined_at         TIMESTAMP NOT NULL,
        token_expires_at  TIMESTAMP NOT NULL,
        last_seen_at      TIMESTAMP NOT NULL,
        PRIMARY KEY (namespace, room_id, participant_id),
        FOREIGN KEY (namespace, room_id)
            REFERENCES media_rooms (namespace, room_id) ON DELETE CASCADE
    );
    CREATE INDEX media_room_participants_last_seen_idx
        ON media_room_participants (last_seen_at);
    CREATE INDEX media_rooms_created_at_idx
        ON media_rooms (created_at);
";

const IDLE_TTL_MINUTES: i64 = 30;

fn namespace_for(i: usize) -> String {
    // 8 busy namespaces take 70% of rooms (roughly, via modulo weighting),
    // the other 32 share the long tail — same shape as the token-admin and
    // export-csv fixtures' customer/principal skew.
    if i % 10 < 7 {
        format!("tenant-busy-{}", i % 8)
    } else {
        format!("tenant-tail-{}", i % 32)
    }
}

fn fmt_ts(ts: DateTime<Utc>) -> String {
    ts.naive_utc().format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

struct Fixture {
    now: DateTime<Utc>,
    cutoff: DateTime<Utc>,
    stale_candidate_rooms: i64,
    expected_reaped_rooms: i64,
    expected_surviving_rooms: i64,
}

/// Seeds the fixture via batched multi-row `INSERT`s (seeding cost is not the
/// measured workload, so it's kept fast rather than round-trip-realistic).
#[allow(clippy::too_many_lines)]
fn seed_fixture(conn: &mut PgConnection) -> Fixture {
    let now = DateTime::parse_from_rfc3339("2026-06-01T12:00:00Z")
        .expect("fixed reference time")
        .with_timezone(&Utc);
    let cutoff = now - Duration::minutes(IDLE_TTL_MINUTES);
    let stale = cutoff - Duration::minutes(5);
    let fresh = cutoff + Duration::minutes(5);

    let mut room_rows: Vec<String> = Vec::new();
    let mut participant_rows: Vec<String> = Vec::new();
    let mut heartbeat_touch_ids: Vec<(String, String, String)> = Vec::new();

    let mut push_room = |ns: &str, room_id: &str, created: DateTime<Utc>| {
        room_rows.push(format!(
            "('{}', '{}', 6, '{}')",
            ns,
            room_id,
            fmt_ts(created)
        ));
    };
    let mut push_participant =
        |ns: &str, room_id: &str, pid: &str, last_seen: DateTime<Utc>, display_name: bool| {
            let name = if display_name { "'guest'" } else { "NULL" };
            participant_rows.push(format!(
                "('{}', '{}', '{}', {}, 'tok-{}-{}-{}', '{}', '{}', '{}')",
                ns,
                room_id,
                pid,
                name,
                ns,
                room_id,
                pid,
                fmt_ts(last_seen),
                fmt_ts(last_seen + Duration::hours(1)),
                fmt_ts(last_seen)
            ));
        };

    // stale_empty: 6,000 rooms, created before cutoff, never joined.
    for i in 0..6_000usize {
        let ns = namespace_for(i);
        push_room(&ns, &format!("stale-empty-{i}"), stale);
    }

    // stale_all_participants_stale: 1,500 rooms x 3 participants, all stale.
    for i in 0..1_500usize {
        let ns = namespace_for(i);
        let room_id = format!("stale-emptied-{i}");
        push_room(&ns, &room_id, stale);
        for p in 0..3 {
            let pid = format!("p{p}");
            push_participant(&ns, &room_id, &pid, stale, p % 2 == 0);
            heartbeat_touch_ids.push((ns.clone(), room_id.clone(), pid));
        }
    }

    // stale_with_fresh_participant: 500 rooms, 1 stale + 1 fresh participant
    // each — must survive (still occupied).
    for i in 0..500usize {
        let ns = namespace_for(i);
        let room_id = format!("stale-occupied-{i}");
        push_room(&ns, &room_id, stale);
        push_participant(&ns, &room_id, "stale-seat", stale, false);
        push_participant(&ns, &room_id, "fresh-seat", fresh, true);
    }

    // fresh_empty: 300 rooms, created after cutoff, no joins yet.
    for i in 0..300usize {
        let ns = namespace_for(i);
        push_room(&ns, &format!("fresh-empty-{i}"), fresh);
    }

    // fresh_with_participants: 400 rooms x 2 participants, mixed staleness
    // (irrelevant to the room's own fate: it isn't a phase-2 candidate).
    for i in 0..400usize {
        let ns = namespace_for(i);
        let room_id = format!("fresh-occupied-{i}");
        push_room(&ns, &room_id, fresh);
        push_participant(&ns, &room_id, "seat-a", stale, false);
        push_participant(&ns, &room_id, "seat-b", fresh, true);
    }

    // --- Boundary / edge rows -------------------------------------------
    // Room whose created_at sits exactly ON the cutoff: `.lt(cutoff)` must
    // exclude it, so it survives even though it has no participants.
    push_room("tenant-boundary", "on-cutoff-room", cutoff);

    // Stale room with exactly one participant whose last_seen_at sits
    // exactly ON the cutoff: phase 1's `.lt(cutoff)` must not delete it, so
    // the room stays occupied and must survive.
    push_room("tenant-boundary", "on-cutoff-participant", stale);
    push_participant(
        "tenant-boundary",
        "on-cutoff-participant",
        "seat",
        cutoff,
        false,
    );

    // Duplicate room_id "general" in two different namespaces, opposite
    // fates: proves the composite (namespace, room_id) key never crosses
    // tenants.
    push_room("tenant-dup-a", "general", stale); // must be reaped
    push_room("tenant-dup-b", "general", fresh); // must survive
    push_participant("tenant-dup-b", "general", "seat", fresh, true);

    conn.batch_execute(&format!(
        "INSERT INTO media_rooms (namespace, room_id, max_participants, created_at) VALUES {}",
        room_rows.join(",")
    ))
    .expect("seed rooms");
    conn.batch_execute(&format!(
        "INSERT INTO media_room_participants \
         (namespace, room_id, participant_id, display_name, token, joined_at, token_expires_at, last_seen_at) \
         VALUES {}",
        participant_rows.join(",")
    ))
    .expect("seed participants");

    // Real dead-tuple ratio: a heartbeat-style UPDATE on every
    // stale_all_participants_stale participant (4,500 rows) before ANALYZE,
    // with no intervening VACUUM.
    for (ns, room_id, pid) in &heartbeat_touch_ids {
        conn.batch_execute(&format!(
            "UPDATE media_room_participants SET last_seen_at = last_seen_at \
             WHERE namespace = '{ns}' AND room_id = '{room_id}' AND participant_id = '{pid}'"
        ))
        .expect("heartbeat touch");
    }
    conn.batch_execute("ANALYZE media_rooms, media_room_participants")
        .expect("analyze");

    let stale_candidate_rooms = 6_000 + 1_500 + 500 + 2; // + the two boundary-adjacent stale rows
    let expected_reaped_rooms = 6_000 + 1_500 + 1; // stale_empty + stale_all_participants_stale + tenant-dup-a/general
    // Survivors among the stale candidates (500 stale_with_fresh_participant +
    // 1 on-cutoff-participant) plus the non-candidate rooms that always
    // survive (fresh_empty, fresh_with_participants, on-cutoff-room by
    // `created_at`, and tenant-dup-b/general).
    let expected_surviving_rooms =
        (stale_candidate_rooms - expected_reaped_rooms) + 300 + 400 + 1 + 1;

    Fixture {
        now,
        cutoff,
        stale_candidate_rooms,
        expected_reaped_rooms,
        expected_surviving_rooms,
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
    #[diesel(sql_type = BigInt)]
    temp_read: i64,
    #[diesel(sql_type = BigInt)]
    temp_written: i64,
}

fn reset_stats(conn: &mut PgConnection) {
    conn.batch_execute("SELECT pg_stat_statements_reset()")
        .expect("reset pg_stat_statements");
}

/// One `pg_stat_statements` run, bucketed by the statement shapes
/// `reap_stale` issues (or, for `cascade_*`, that the FK fires on its behalf).
#[derive(Default)]
struct Profile {
    candidate_scan_calls: i64,
    candidate_scan_buffers: i64,
    count_calls: i64,
    count_buffers: i64,
    delete_room_calls: i64,
    delete_room_buffers: i64,
    cascade_calls: i64,
    cascade_buffers: i64,
    sweep_calls: i64,
    sweep_buffers: i64,
}

/// Prints every `media_room*` statement from this run, bucketed.
fn print_profile(conn: &mut PgConnection, label: &str) -> Profile {
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers, \
                temp_blks_read AS temp_read, temp_blks_written AS temp_written \
         FROM pg_stat_statements \
         WHERE query ILIKE '%media_room%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut candidate_scan_calls, mut candidate_scan_buffers) = (0i64, 0i64);
    let (mut count_calls, mut count_buffers) = (0i64, 0i64);
    let (mut delete_room_calls, mut delete_room_buffers) = (0i64, 0i64);
    let (mut cascade_calls, mut cascade_buffers) = (0i64, 0i64);
    let (mut sweep_calls, mut sweep_buffers) = (0i64, 0i64);
    let (mut temp_read, mut temp_written) = (0i64, 0i64);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} temp_read={:<4} temp_written={:<4} {normalized}",
            row.calls, row.buffers, row.temp_read, row.temp_written
        );
        temp_read += row.temp_read;
        temp_written += row.temp_written;
        // Diesel quotes every identifier; drop the quotes (and the `ONLY
        // public.` the FK cascade's internal RI statement carries) so the
        // match below is against bare table names.
        let bare = normalized
            .replace(['"', '\''], "")
            .replace("ONLY public.", "");
        if bare.starts_with("SELECT") && bare.contains("COUNT(*)") {
            count_calls += row.calls;
            count_buffers += row.buffers;
        } else if bare.starts_with("SELECT") && bare.contains("FROM media_rooms") {
            candidate_scan_calls += row.calls;
            candidate_scan_buffers += row.buffers;
        } else if bare.starts_with("DELETE FROM media_rooms") {
            // Baseline: one per now-empty candidate. Post-fix: the single
            // batched anti-join delete.
            delete_room_calls += row.calls;
            delete_room_buffers += row.buffers;
        } else if bare.starts_with("DELETE FROM media_room_participants")
            && bare.contains("last_seen_at")
        {
            sweep_calls += row.calls;
            sweep_buffers += row.buffers;
        } else if bare.starts_with("DELETE FROM media_room_participants") {
            // The FK's ON DELETE CASCADE referential-integrity statement,
            // fired once per deleted room row either side of the fix.
            cascade_calls += row.calls;
            cascade_buffers += row.buffers;
        }
    }
    println!(
        "-- candidate-scan calls={candidate_scan_calls} -- COUNT(*) calls={count_calls} \
         buffers={count_buffers} -- media_rooms DELETE calls={delete_room_calls} \
         buffers={delete_room_buffers} -- FK cascade calls={cascade_calls} \
         buffers={cascade_buffers} -- phase-1 sweep calls={sweep_calls} \
         buffers={sweep_buffers} -- temp_blks_read={temp_read} \
         temp_blks_written={temp_written} (all media_room statements) --"
    );
    Profile {
        candidate_scan_calls,
        candidate_scan_buffers,
        count_calls,
        count_buffers,
        delete_room_calls,
        delete_room_buffers,
        cascade_calls,
        cascade_buffers,
        sweep_calls,
        sweep_buffers,
    }
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

fn surviving_room_count(conn: &mut PgConnection) -> i64 {
    #[derive(QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = BigInt)]
        n: i64,
    }
    diesel::sql_query("SELECT COUNT(*) AS n FROM media_rooms")
        .get_result::<CountRow>(conn)
        .expect("count surviving rooms")
        .n
}

fn room_exists(conn: &mut PgConnection, namespace: &str, room_id: &str) -> bool {
    #[derive(QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = BigInt)]
        n: i64,
    }
    let n = diesel::sql_query(format!(
        "SELECT COUNT(*) AS n FROM media_rooms WHERE namespace = '{namespace}' AND room_id = '{room_id}'"
    ))
    .get_result::<CountRow>(conn)
    .expect("room lookup")
    .n;
    n > 0
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn room_reaper_batch_profile() {
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

    let mut conn = PgConnection::establish(&url).expect("sync db connection");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .expect("create pg_stat_statements extension");
    conn.batch_execute(CREATE_TABLES_SQL)
        .expect("create tables");

    let fixture = seed_fixture(&mut conn);
    println!(
        "\n=== fixture: {} stale candidate rooms, expect {} reaped / {} surviving ===",
        fixture.stale_candidate_rooms,
        fixture.expected_reaped_rooms,
        fixture.expected_surviving_rooms
    );

    // Plan shapes of the two statement forms, captured BEFORE reap_stale
    // mutates anything (so both scans see the full, pre-reap fixture) —
    // diagnostics only; the scale claim is the pg_stat_statements table
    // printed after the real run below.
    explain(
        &mut conn,
        "occupancy check for one candidate room (baseline: issued once per candidate)",
        "SELECT COUNT(*) FROM media_room_participants \
         WHERE namespace = 'tenant-busy-0' AND room_id = 'stale-empty-0'",
    );
    explain(
        &mut conn,
        "batched anti-join DELETE candidate set (fix: entire phase 2 in one statement)",
        &format!(
            "SELECT namespace, room_id FROM media_rooms \
             WHERE created_at < '{}' \
               AND NOT EXISTS (SELECT 1 FROM media_room_participants p \
                 WHERE p.namespace = media_rooms.namespace AND p.room_id = media_rooms.room_id)",
            fmt_ts(fixture.cutoff)
        ),
    );

    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");
    let store: Arc<dyn RoomStore> = Arc::new(DbRoomStore::new(pool, 6));

    reset_stats(&mut conn);
    let stats = store
        .reap_stale(fixture.now, Duration::minutes(IDLE_TTL_MINUTES))
        .await;
    println!(
        "\n=== reap_stale result: rooms_reaped={} participants_reaped={} ===",
        stats.rooms_reaped, stats.participants_reaped
    );

    let profile = print_profile(&mut conn, "reap_stale phase 2 (candidate rooms)");

    // ---- Result equivalence -------------------------------------------
    assert_eq!(
        stats.rooms_reaped as i64, fixture.expected_reaped_rooms,
        "rooms_reaped must match the analytically-expected reap set"
    );
    let surviving = surviving_room_count(&mut conn);
    assert_eq!(
        surviving, fixture.expected_surviving_rooms,
        "surviving room count must match the analytically-expected keep set"
    );
    assert!(
        room_exists(&mut conn, "tenant-boundary", "on-cutoff-room"),
        "a room created exactly ON the cutoff must survive (predicate is `.lt`)"
    );
    assert!(
        room_exists(&mut conn, "tenant-boundary", "on-cutoff-participant"),
        "a stale room whose sole participant's last_seen_at sits exactly ON the cutoff must survive"
    );
    assert!(
        !room_exists(&mut conn, "tenant-dup-a", "general"),
        "tenant-dup-a/general must be reaped"
    );
    assert!(
        room_exists(&mut conn, "tenant-dup-b", "general"),
        "tenant-dup-b/general (same room_id, different namespace, occupied) must survive"
    );

    // ---- N+1 evidence ---------------------------------------------------
    let phase2_statements =
        profile.candidate_scan_calls + profile.count_calls + profile.delete_room_calls;
    let phase2_buffers =
        profile.candidate_scan_buffers + profile.count_buffers + profile.delete_room_buffers;
    println!(
        "\n=== statement-count summary (candidate rooms: {}) ===\n\
         candidate scan:                {:>6} call(s), {:>7} buffers\n\
         COUNT(*) occupancy checks:     {:>6} call(s), {:>7} buffers\n\
         media_rooms DELETE:            {:>6} call(s), {:>7} buffers\n\
         -- phase-2 client statements:  {:>6}, {:>7} buffers --\n\
         FK ON DELETE CASCADE (internal):{:>5} call(s), {:>7} buffers\n\
         phase-1 participant sweep:     {:>6} call(s), {:>7} buffers (unchanged by this fix)",
        fixture.stale_candidate_rooms,
        profile.candidate_scan_calls,
        profile.candidate_scan_buffers,
        profile.count_calls,
        profile.count_buffers,
        profile.delete_room_calls,
        profile.delete_room_buffers,
        phase2_statements,
        phase2_buffers,
        profile.cascade_calls,
        profile.cascade_buffers,
        profile.sweep_calls,
        profile.sweep_buffers,
    );
    // No shape-specific assertions here on purpose: this harness runs
    // UNCHANGED before and after the fix (baseline issues candidate_scan=1 +
    // count_calls=N + up-to-N deletes; the batched fix issues a single
    // statement and both `candidate_scan_calls` and `count_calls` go to 0).
    // The printed statement-count summary above and the two EXPLAINs
    // captured before the run are the committed evidence; the equivalence
    // assertions above are what must hold in both worlds.
}
