# 🪝 Snag: `DbRoomStore::join_room` can exceed the documented absolute seat cap under concurrent multi-process joins

Filed as [issue #2864](https://github.com/autumn-foundation/autumn/issues/2864).

**Charter:** operator running `[media] room_store_backend = "db"` across two
app instances (the exact multi-process deployment the backend exists for) —
drive the documented cross-process ceiling/reaper claims live, closing out
charter 1 from
[2026-09-13's](2026-09-13-snag-media-room-session.md) and
[2026-09-14's](2026-09-14-snag-media-room-registry-session.md) media-room
sessions ("`DbRoomStore` parity", "needs Postgres or SQLite — Postgres needs
Docker, unavailable ... SQLite doesn't per `rooms_db.rs`'s own module doc").

## 🐛 Repro

**Title:** 🪝 Snag: `DbRoomStore` join races can exceed the documented seat cap (data-correctness, repro ~4/100, oracle: docs/guide/media.md "Both backends enforce the absolute 6-seat mesh ceiling")

Two independent connection pools over the **same SQLite file** (the direct
analogue of "two app processes sharing the database" — no Docker needed, per
the prior sessions' correction that Postgres, not SQLite, was the one
requiring a container this sandbox doesn't have):

1. Create a room capped at 1 seat (`store_a.create_room(ns, 1)`).
2. Spawn 16 concurrent joiners against that room, split 8/8 across the two
   pools, synchronized on a `tokio::sync::Barrier` so all 16 issue their
   `join_room` call as close to simultaneously as the runtime allows.
3. Count successful joins, and independently re-count the row in
   `media_room_participants` via a third pool.
4. Repeat for N trials with a fresh temp-file database each time.

**Root cause** (`autumn-media-plugin/src/rooms_db.rs`, `DbRoomStore::join_room`,
~L258-271): the capacity check is a `SELECT COUNT(*) ... WHERE namespace = ?
AND room_id = ?` followed by a **separate** `INSERT`, on the same connection
but as two independent auto-commit statements — no explicit transaction, no
row lock, no unique constraint backstopping the count. Two connections can
each read the pre-join count, both see room for one more seat, and both
insert. This is explicitly self-documented in the source as a "non-transactional
backstop" (the same phrase `create_room`'s own comment uses for its registry-cap
race, which that comment calls "an accepted backstop-only imprecision") — but
the *published* guide draws no such distinction:

> `docs/guide/media.md`: "Both backends enforce the absolute 6-seat mesh
> ceiling and cap the registry..."

That claim is true for `InMemoryRoomStore` (`join_room` holds a single
`RwLock::write()` across the whole check-then-insert —
`autumn-media-plugin/src/rooms.rs:710-718` — genuinely atomic, confirmed by
reading the code) and **false** for `DbRoomStore` under concurrent joins from
more than one process, which is precisely the deployment shape the `db`
backend exists for ("the correct backend for a horizontally-scaled or
multi-process deployment").

**Reproduction rate:** 4/100 trials overshot the cap, across three
independent clean runs (1/30, 2/30, 1/40) — all with 16 barrier-synced
racers split across two pools racing for a single remaining seat. One trial
(run 2, trial 28) overshot by 2 extra seats (3 successful joins into a
1-seat room), not just 1, showing the overshoot isn't bounded to "one race
winner slips through" — it scales with how many joiners land in the shared
gap. Conditions: `#[tokio::test(flavor = "multi_thread", worker_threads =
4)]`, 4 vCPUs, SQLite `db` backend with the framework's default WAL +
`busy_timeout = 5000` pragmas (`autumn/src/db.rs`) — i.e., the shipped
production pragma set, not a stripped-down repro fixture. Lower concurrency
(6 or 8 racers, no barrier) did **not** reproduce it in 20+ trials — the
window is real but narrow, which is why 2026-09-13's original single-instance
probe (never actually run, since Docker was unavailable) would likely have
needed exactly this kind of tight synchronization to catch it.

## 📌 Environment

- Commit `7db4fdc` (trunk-dev, `autumn-foundation/autumn`)
- `autumn-media-plugin` 0.7.0, `autumn-web` 0.7.0, `sqlite` feature
- Linux container, 4 vCPUs, no Docker (confirmed unavailable — `dockerd` not
  running)
- SQLite via `diesel_async::sync_connection_wrapper::SyncConnectionWrapper<SqliteConnection>`,
  the framework's own `create_pool` (`autumn_web::db::create_pool`), default
  pragmas
- rustc from this workspace's pinned toolchain (`rust-version = "1.88.0"`)

## ⚖️ Oracle

`docs/guide/media.md`, "Room store backends" section: *"Both backends enforce
the absolute 6-seat mesh ceiling and cap the registry."* An unqualified
enforcement claim, made about *both* backends together, in the section whose
whole purpose is to explain when to pick `db` over `memory` (i.e., exactly
when a reader would rely on it holding under multi-process load).

## 💥 Impact

**Data-correctness / documented-claim violation**, not crash/data-loss. The
"absolute mesh ceiling" exists because a WebRTC mesh room's peer-connection
count grows O(n²) — 6 participants is 15 connections; a 7th or 8th
participant admitted past the ceiling pushes every existing participant's
client into more simultaneous peer connections than the room was designed
and capacity-planned for. Concretely:

- Any deployment following the docs' own guidance ("the correct backend for
  a horizontally-scaled or multi-process deployment") is exposed the moment
  two processes race a join for a room's last seat — e.g., a popular
  scheduled event where several people click "Join" within the same
  ~10-50ms window, hitting different app instances behind a load balancer.
- The overshoot is invisible to the participants who get in: nobody sees an
  error, the room simply silently exceeds its designed capacity. Callers who
  *do* get `RoomFull` have no way to know they lost a race that shouldn't
  have been losable in the first place. This is "wrong answer delivered
  confidently" territory, not a loud failure — nobody detects it without an
  independent participant count, which is exactly what makes it worth the
  impact floor.
- Severity is bounded by the narrow race window (~4% under an aggressively
  synchronized 16-way race; almost certainly lower under organic multi-process
  load, since barrier-synced concurrency is a harder hit than realistic
  arrival jitter) — this is not a crash/hang/loss-class bug, hence "data
  correctness," not higher.

## Dedup search

Searched open/recent issues in `autumn-foundation/autumn` for `DbRoomStore`,
"join capacity race", "seat cap", "mesh ceiling". One near-miss found and
linked:

- **#2407** ("Media rooms: `join_room` and `reap_stale` share no lock, so a
  join can silently vanish") — same root cause (`join_room`'s missing
  transaction/lock) and same file/function, but a **different** failure
  mode: #2407 is `join_room` racing `reap_stale` (a successful join whose
  room gets cascade-deleted out from under it, silently losing the seat).
  This report is `join_room` racing **itself** (two concurrent joins both
  clearing a stale capacity check), which *increases* occupancy past the
  cap rather than losing a seat. They are siblings, not duplicates: #2407's
  own "What a real fix looks like" section (`SELECT ... FOR UPDATE` on the
  room row before inserting, or a per-room advisory lock) would, if
  implemented, very likely close both — worth noting on whichever issue
  someone picks up first so the fix isn't shipped twice.
- No other open or closed issue matched.

## 🔬 Reproduce

Requires a temporary local change (NOT committed — it trips the workspace's
own `sqlite` feature-unification hazard documented in `autumn/src/db.rs`;
`autumn-media-plugin` is deliberately never supposed to enable `sqlite`
itself, so this is scratch-only, reverted after use):

```bash
cd /home/user/autumn
# 0. Back up both files this procedure touches into freshly-generated,
#    collision-proof temp paths (mktemp, not a fixed `.bak` name — a fixed
#    name is itself a repeat-run hazard: a second interrupted attempt would
#    overwrite the first attempt's only clean backup, or restore a stale one
#    over a file that didn't need restoring), with `cp -p` so the backup
#    (and the file the cleanup step restores from it) keeps the original's
#    mode/timestamps instead of inheriting mktemp's 0600. Rather than reverting via
#    `git checkout -- <file>` afterward — that form replaces the whole file
#    with the index version and would silently discard any *other*
#    uncommitted edits already sitting in it (see
#    docs/reports/2026-09-16-onramp-test-sim-compile-gate-negative-result.md
#    for the same failure mode caught there).
cargo_bak="$(mktemp)"
cp -p autumn-media-plugin/Cargo.toml "$cargo_bak"
probe_path=autumn-media-plugin/tests/snag_seat_race_probe.rs
probe_bak=""
[ -e "$probe_path" ] && { probe_bak="$(mktemp)"; cp -p "$probe_path" "$probe_bak"; }

# 1. Temporarily add to autumn-media-plugin/Cargo.toml [dev-dependencies]:
#      autumn-web = { path = "../autumn", features = ["sqlite"] }

# 2. Add autumn-media-plugin/tests/snag_seat_race_probe.rs:
cat > "$probe_path" <<'RUST'
use std::sync::Arc;
use autumn_media_plugin::rooms::RoomStore;
use autumn_media_plugin::rooms_db::DbRoomStore;
use autumn_web::config::DatabaseConfig;
use chrono::Duration;

const CREATE_TABLES_SQL: &str = "
    CREATE TABLE IF NOT EXISTS media_rooms (
        namespace TEXT NOT NULL, room_id TEXT NOT NULL,
        max_participants INTEGER NOT NULL, created_at TIMESTAMP NOT NULL,
        PRIMARY KEY (namespace, room_id));
    CREATE TABLE IF NOT EXISTS media_room_participants (
        namespace TEXT NOT NULL, room_id TEXT NOT NULL, participant_id TEXT NOT NULL,
        display_name TEXT, token TEXT NOT NULL, joined_at TIMESTAMP NOT NULL,
        token_expires_at TIMESTAMP NOT NULL, last_seen_at TIMESTAMP NOT NULL,
        PRIMARY KEY (namespace, room_id, participant_id),
        FOREIGN KEY (namespace, room_id) REFERENCES media_rooms (namespace, room_id) ON DELETE CASCADE);
";

fn build_pool(url: &str) -> autumn_web::db::Pool<autumn_web::RuntimeConnection> {
    let config = DatabaseConfig { url: Some(url.to_owned()), pool_size: 10, ..Default::default() };
    autumn_web::db::create_pool(&config).expect("create_pool").expect("pool present")
}

async fn run_trial() -> (usize, i64) {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite://{}", dir.path().join("rooms.db").display());
    let (pool_a, pool_b) = (build_pool(&url), build_pool(&url));
    {
        use diesel_async::RunQueryDsl;
        let mut conn = pool_a.get().await.expect("conn");
        for stmt in CREATE_TABLES_SQL.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() { diesel::sql_query(stmt).execute(&mut conn).await.expect("ddl"); }
        }
    }
    let store_a: Arc<dyn RoomStore> = Arc::new(DbRoomStore::new(pool_a, 6));
    let store_b: Arc<dyn RoomStore> = Arc::new(DbRoomStore::new(pool_b, 6));
    let room = store_a.create_room("tenant-a", 1).await.expect("create");

    const RACERS: usize = 16;
    let barrier = Arc::new(tokio::sync::Barrier::new(RACERS));
    let mut handles = Vec::new();
    for i in 0..RACERS {
        let store = if i % 2 == 0 { store_a.clone() } else { store_b.clone() };
        let (room_id, barrier) = (room.id.clone(), barrier.clone());
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            store.join_room("tenant-a", &room_id, Some(format!("racer-{i}")), Duration::seconds(300)).await
        }));
    }
    let mut successes = 0;
    for h in handles { if h.await.expect("panic").is_ok() { successes += 1; } }

    let pool_c = build_pool(&url);
    let final_count: i64 = {
        use diesel::prelude::*;
        use diesel_async::RunQueryDsl;
        let mut conn = pool_c.get().await.expect("conn");
        diesel::sql_query("SELECT COUNT(*) as count FROM media_room_participants WHERE room_id = ?")
            .bind::<diesel::sql_types::Text, _>(room.id.clone())
            .get_result::<CountRow>(&mut conn).await.expect("count").count
    };
    (successes, final_count)
}

#[derive(diesel::QueryableByName)]
struct CountRow { #[diesel(sql_type = diesel::sql_types::BigInt)] count: i64 }

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_joins_across_two_pools_can_exceed_the_absolute_seat_cap() {
    const TRIALS: usize = 40;
    let mut overshoots = 0;
    for trial in 0..TRIALS {
        let (successes, final_count) = run_trial().await;
        println!("trial {trial}: successes={successes} final_seat_count={final_count} (cap=1)");
        if final_count > 1 || successes > 1 { overshoots += 1; }
    }
    println!("overshoots: {overshoots}/{TRIALS}");
    assert_eq!(overshoots, 0, "cap exceeded in {overshoots}/{TRIALS} trials");
}
RUST

# 3. Run it (expect intermittent failure across repeated invocations, ~4/100
#    trials aggregated across runs — a single 40-trial run may show 0):
cargo test -p autumn-media-plugin --test snag_seat_race_probe -- --nocapture

# 4. Revert both scratch changes — do not commit them. Restore from the
#    mktemp backups made in step 0 (not `git checkout -- <file>`, which
#    would clobber any unrelated uncommitted edits already in the file):
mv "$cargo_bak" autumn-media-plugin/Cargo.toml
rm "$probe_path"
[ -n "$probe_bak" ] && mv "$probe_bak" "$probe_path"
```

## Why this wasn't committed as a quarantined regression test

The repo's own architecture explicitly forbids enabling `sqlite` from any
workspace crate or dev-dependency other than an end application (see the
"FEATURE-UNIFICATION HAZARD" comment on `RuntimeConnection` in
`autumn/src/db.rs`, and the CI "sqlite-unification gate" that watches for
exactly this). A permanent test living inside `autumn-media-plugin/tests/`
would need to either flip that feature for the whole crate (the hazard the
gate exists to prevent) or move to a genuinely separate, non-workspace-member
crate — more surface than a QA probe should introduce on its own judgment.
Whoever picks up the fix should decide the test's permanent home alongside
it (most likely: extend the existing Postgres testcontainer suite in
`autumn-media-plugin/tests/room_store_db.rs` with an equivalent
barrier-synced race case, since that file already isolates the feature
correctly and Postgres doesn't carry the SQLite unification hazard).

## Proposed next charters

1. **The same race against Postgres**, in an environment with Docker, to
   confirm the window exists (and characterize its rate) against the
   backend most real `db`-mode deployments will actually run — SQLite's
   locking model may not be representative of Postgres's MVCC/lock-wait
   behavior under the same access pattern.
2. **`create_room`'s registry-cap race**, same methodology (this session
   targeted the seat cap specifically; the registry-size backstop shares
   the identical check-then-insert shape one function up, in `create_room`,
   and is explicitly documented in-code as the same kind of accepted
   imprecision — worth confirming it's bounded the same way, or filing
   alongside #2407/this report if a fix ends up touching both).
3. **`DbRoomStore` last-write-wins reaper convergence**, carried over
   unattempted from 2026-09-13/14 — this session used the same SQLite
   two-pool harness for a different claim; the reaper-convergence claim
   is still untouched live.
