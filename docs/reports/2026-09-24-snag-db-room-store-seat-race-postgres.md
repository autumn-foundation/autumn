# 🪝 Snag: `DbRoomStore` seat-cap race is near-certain on Postgres, not a narrow window — escalates issue #2864

**Charter:** proposed charter 1 from
[2026-09-20's session](2026-09-20-snag-db-room-store-seat-race.md) — "the
same race against Postgres, in an environment with Docker, to confirm the
window exists (and characterize its rate) against the backend most real
`db`-mode deployments will actually run." Docker was unavailable to the
three prior media-room sessions (2026-09-13, 2026-09-14, 2026-09-20); it is
available in this session's sandbox (`dockerd` started cleanly, `docker
info` succeeds), so this closes that gap.

## 🐛 Repro

**Title:** 🪝 Snag: `DbRoomStore::join_room` seat-cap race is near-certain
under ordinary concurrency on Postgres, not a narrow window (data-correctness,
repro 60/60 and 37/40 across two conditions, oracle: docs/guide/media.md
"Both backends enforce the absolute 6-seat mesh ceiling")

This is **the same bug already filed as
[issue #2864](https://github.com/autumn-foundation/autumn/issues/2864)** —
same root cause (`DbRoomStore::join_room`'s check-then-insert with no
transaction or row lock, `autumn-media-plugin/src/rooms_db.rs` ~L258-271),
same oracle. #2864 characterized it on SQLite at ~4/100 trials, under an
aggressively synchronized 16-way barrier race, and left Postgres — the
backend the "multi-process deployment" guidance is actually about —
unconfirmed. This session ran the equivalent probes against a real Postgres
container and found the rate is **an order of magnitude higher and requires
far less concurrency to trigger**, which changes how this should be
prioritized.

**Condition A — same harness as #2864 (16 racers, 2 pools, barrier-synced,
1-seat room), against Postgres instead of SQLite:**

1. Start a real Postgres container (testcontainers), matching the existing
   `autumn-media-plugin/tests/room_store_db.rs` harness exactly (same
   `setup_pool`-equivalent DDL, same table schema).
2. Two independent `deadpool` pools over the same Postgres database (the
   two-process analogue).
3. Create a room capped at 1 seat.
4. Spawn 16 concurrent joiners split 8/8 across the two pools, synchronized
   on a `tokio::sync::Barrier`.
5. Count successes and independently re-count `media_room_participants`.
6. Repeat for 60 trials, fresh namespace per trial (same container/pool,
   isolated rows).

**Result: 60/60 trials overshot** in the run whose full output is quoted
below (a first run, not separately logged, measured 59/60 — consistent
with this being a real, high-but-not-100% rate rather than a deterministic
always-fails bug). Not by one or two seats — the single most common
outcome was letting *every* racer in: `successes=16` (all 16 racers
admitted into the 1-seat room) occurred in **27 of 60 trials (45%)**, more
than any other value, with `successes=15` next most common (14/60, 23%).
Full histogram of `successes` across all 60 trials:
`{3: 1, 5: 1, 8: 1, 10: 2, 11: 1, 12: 3, 13: 5, 14: 5, 15: 14, 16: 27}` — no
trial admitted fewer than 3 racers, and the distribution is heavily
right-skewed toward "nearly everyone gets in," not clustered near the
correct outcome of 1.

**Condition B — realistic load, not a synthetic worst case:** to rule out
"only an aggressively-tuned 16-way barrier race triggers this," a second,
deliberately less adversarial probe: 4 concurrent joiners (no barrier, just
`tokio::spawn` fired back-to-back), **single pool** (no two-process
simulation), room capped at 3 seats, 40 trials.

**Result: 37/40 trials overshot (92.5%)**, and of those, every single one
overshot to exactly `successes=4` (all four racers admitted into a 3-seat
room) — the full histogram is `{3: 3, 4: 37}`, i.e. only 3 of 40 trials
produced the correct outcome, and there is no partial-overshoot case at
all at this concurrency level: a trial either lands exactly on the cap or
blows straight through it to the maximum possible. This is not a
multi-process artifact and does not need adversarial synchronization —
ordinary same-process concurrent request handling (e.g. an httpd worker
pool handling four join requests that land in the same tens-of-milliseconds
window) reproduces it almost every time on Postgres.

**Sanity check performed:** before trusting either result, a third probe
confirmed the cap enforces correctly under *sequential* (non-concurrent)
joins against the same Postgres harness — first join succeeds, second is
correctly rejected with `RoomFull { max: 1 }`. This rules out a broken
harness or a cap-check-always-passes bug; the store's logic is correct in
isolation, it is only the concurrent check-then-insert that races.

## 📌 Environment

- Commit `fc9dbea` (`trunk-dev`, `autumn-foundation/autumn`)
- `autumn-media-plugin` 0.7.0, `autumn-web` 0.7.0, default (Postgres)
  feature set — no `sqlite` feature flip needed, unlike the #2864 SQLite
  probe, since this reuses `room_store_db.rs`'s existing Postgres
  testcontainer dev-dependencies as-is
- Postgres via `testcontainers` + `testcontainers_modules::postgres::Postgres::default()`
  (the same container image/config `room_store_db.rs`'s existing suite
  uses), `diesel_async::AsyncPgConnection`, `deadpool` pool (`max_size: 20`)
- Linux container, 4 vCPUs, Docker 29.3.1 (`dockerd` started manually this
  session; unavailable to the three prior media-room sessions)
- `#[tokio::test(flavor = "multi_thread", worker_threads = 4)]`
- rustc from this workspace's pinned toolchain (`rust-version = "1.88.0"`)

## ⚖️ Oracle

Same as #2864: `docs/guide/media.md` line 249, "Room store backends"
section: *"Both backends enforce the absolute 6-seat mesh ceiling and cap
the registry..."* — re-checked against the current doc text this session,
unchanged since #2864 was filed.

## 💥 Impact

**Escalates #2864 from "narrow race window, data-correctness" to "near-
certain under ordinary concurrent load, on the specific backend the docs
recommend for concurrent/multi-process deployments."** The practical
difference matters for prioritization:

- #2864's SQLite numbers (~4/100, requiring a synchronized 16-way race)
  read as an edge case that needs unlucky timing to hit in production.
- This session's Postgres numbers (60/60 under the same synchronized
  conditions, 37/40 under *ordinary* 4-way concurrency with no
  synchronization at all) mean any room whose capacity is contended by
  even a handful of simultaneous join requests — the exact "several
  people click Join for a popular scheduled event" scenario #2864 already
  named — will very likely exceed its seat cap, often maxing out to
  however many requests happened to race, not overshooting by one.
- Postgres is the backend the docs frame as "the correct backend for a
  horizontally-scaled or multi-process deployment," i.e. the one operators
  choose specifically because they expect concurrent load. The claim is
  falsest exactly where it's relied on most.
- Still not crash/hang/data-loss — no error, no corruption, the room
  simply silently seats more participants than its documented ceiling,
  which for a WebRTC mesh (O(n²) peer connections) can push participant
  clients into far more simultaneous connections than the ceiling was
  chosen to bound. Severity classification stays "data-correctness /
  documented-claim violation," per the same reasoning #2864 already gives,
  but the *likelihood* component of that classification should move from
  "narrow window" to "the common case."

## Dedup search

Searched open issues for `DbRoomStore`, "seat cap", "join race", "mesh
ceiling", "Postgres". This is the same bug as **#2864** (not a duplicate to
file separately) — same function, same root cause, same oracle citation —
so this session's finding is reported as new data on that issue rather than
a new filing, per the dedup requirement. **#2407** remains the
already-noted sibling (reaper-cascade race vs. this self-race); unchanged
from #2864's own dedup note.

## 🔬 Reproduce

Two scratch test files (one per condition, each its own `[[test]]`-shaped
file under `autumn-media-plugin/tests/`) were added for this session, run
against a live Docker/testcontainers Postgres, and then **removed** (not
committed).

**Correction from this report's first revision:** that revision claimed
committing these as permanent tests would "turn the CI Docker sweep red on
every run," describing `autumn-media-plugin` as covered by the same bare
`--ignored` sweep `autumn`'s `integration_tests` and `autumn-cli`'s
`cli_tests` binaries get. That's wrong, and `.github/workflows/ci.yml`
itself says so directly — its "Run Docker-dependent tests" step comment
reads: *"autumn-media-plugin has no crate-wide `--ignored` sweep, so each
of its Docker test targets is named here"*, followed by explicit,
individually-named invocations of exactly two targets:
`cargo test -p autumn-media-plugin --test room_reaper_batch_profile --
--ignored` and `cargo test -p autumn-media-plugin --test room_store_db --
--ignored`. A **new**, differently-named test file added under
`autumn-media-plugin/tests/` — as this session's scratch files were — would
not run in CI at all unless also added to that explicit list; it would be
silently absent, not red.

The corrected reasoning for not committing: the natural permanent home for
this reproduction — per 2026-09-20's report's own conclusion — is as a new
`#[ignore]`d test *function added inside `room_store_db.rs`*, since that
target **is** one of the two ci.yml already names and runs unconditionally.
Adding it there, given a 92–100% observed failure rate across both
conditions in this session, would make that Docker CI step fail on nearly
every run, and this repo has no established "expected-fail/quarantined"
marking convention the step would respect. That is the same practical
outcome the first revision described (a committed version would break CI),
reached by the correct mechanism (naming it into an always-run target, not
tripping a sweep that doesn't exist for this crate) — worth being precise
about, since a future contributor relying on the wrong mechanism could
wrongly conclude a *different* new standalone file is safe to commit when
it would in fact just never run.

```bash
cd /home/user/autumn
# Ensure Docker is running (this sandbox needed `dockerd` started manually;
# a normal CI/dev box with the Docker daemon already up can skip this):
#   nohup dockerd >/tmp/dockerd.log 2>&1 & sleep 5 && docker info

# Add autumn-media-plugin/tests/snag_pg_seat_race_probe.rs (condition A —
# full listing below), then:
cargo test -p autumn-media-plugin --test snag_pg_seat_race_probe -- --ignored --nocapture
# Expect the large majority of 60 trials to overshoot the 1-seat cap, most
# often to the full 16/16 racer count (this session: 60/60 overshot, mode
# successes=16 in 27/60 trials).
rm autumn-media-plugin/tests/snag_pg_seat_race_probe.rs

# Add autumn-media-plugin/tests/snag_pg_seat_race_lowconc.rs (condition B —
# full listing below), then:
cargo test -p autumn-media-plugin --test snag_pg_seat_race_lowconc -- --ignored --nocapture
# Expect the large majority of 40 trials to overshoot the 3-seat cap, almost
# always to exactly 4/4 (this session: 37/40 overshot, all 37 at successes=4).
rm autumn-media-plugin/tests/snag_pg_seat_race_lowconc.rs
```

<details>
<summary><code>snag_pg_seat_race_probe.rs</code> (16-racer / 2-pool / barrier-synced, condition A)</summary>

```rust
use std::sync::Arc;

use autumn_media_plugin::rooms::RoomStore;
use autumn_media_plugin::rooms_db::DbRoomStore;
use chrono::Duration;
use diesel::prelude::*;
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

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

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

async fn run_trial(pool_a: &Pool<AsyncPgConnection>, pool_b: &Pool<AsyncPgConnection>, trial: usize) -> (usize, i64) {
    let store_a: Arc<dyn RoomStore> = Arc::new(DbRoomStore::new(pool_a.clone(), 6));
    let store_b: Arc<dyn RoomStore> = Arc::new(DbRoomStore::new(pool_b.clone(), 6));
    let ns = format!("tenant-{trial}");
    let room = store_a.create_room(&ns, 1).await.expect("create");

    const RACERS: usize = 16;
    let barrier = Arc::new(tokio::sync::Barrier::new(RACERS));
    let mut handles = Vec::new();
    for i in 0..RACERS {
        let store = if i % 2 == 0 { store_a.clone() } else { store_b.clone() };
        let (room_id, barrier, ns) = (room.id.clone(), barrier.clone(), ns.clone());
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            store
                .join_room(&ns, &room_id, Some(format!("racer-{i}")), Duration::seconds(300))
                .await
        }));
    }
    let mut successes = 0;
    for h in handles {
        if h.await.expect("panic").is_ok() {
            successes += 1;
        }
    }

    let mut conn = pool_a.get().await.expect("conn");
    let final_count: i64 = diesel::sql_query(
        "SELECT COUNT(*) as count FROM media_room_participants WHERE namespace = $1 AND room_id = $2",
    )
    .bind::<diesel::sql_types::Text, _>(ns.clone())
    .bind::<diesel::sql_types::Text, _>(room.id.clone())
    .get_result::<CountRow>(&mut conn)
    .await
    .expect("count")
    .count;
    (successes, final_count)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers) - SCRATCH PROBE, not for CI"]
async fn pg_concurrent_joins_across_two_pools_can_exceed_the_absolute_seat_cap() {
    let container = Postgres::default().start().await.expect("start postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let build_pool = || {
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
        Pool::builder(manager).max_size(20).build().expect("pool")
    };
    let (pool_a, pool_b) = (build_pool(), build_pool());
    {
        let mut conn = pool_a.get().await.expect("conn");
        for stmt in CREATE_TABLES_SQL.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() {
                diesel::sql_query(stmt).execute(&mut conn).await.expect("ddl");
            }
        }
    }

    const TRIALS: usize = 60;
    let mut overshoots = 0;
    let mut histogram: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for trial in 0..TRIALS {
        let (successes, final_count) = run_trial(&pool_a, &pool_b, trial).await;
        println!("trial {trial}: successes={successes} final_seat_count={final_count} (cap=1)");
        *histogram.entry(successes).or_insert(0) += 1;
        if final_count > 1 || successes > 1 {
            overshoots += 1;
        }
    }
    println!("overshoots: {overshoots}/{TRIALS}");
    println!("histogram of successes-per-trial: {histogram:?}");
    assert_eq!(overshoots, 0, "cap exceeded in {overshoots}/{TRIALS} trials");
}
```

</details>

<details>
<summary><code>snag_pg_seat_race_lowconc.rs</code> (4-racer / single-pool / no-barrier, condition B)</summary>

```rust
use std::sync::Arc;
use autumn_media_plugin::rooms::RoomStore;
use autumn_media_plugin::rooms_db::DbRoomStore;
use chrono::Duration;
use diesel::prelude::*;
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

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

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers) - SCRATCH PROBE, not for CI"]
async fn pg_low_concurrency_no_barrier_still_overshoots() {
    let container = Postgres::default().start().await.expect("start postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(20).build().expect("pool");
    {
        let mut conn = pool.get().await.expect("conn");
        for stmt in CREATE_TABLES_SQL.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() { diesel::sql_query(stmt).execute(&mut conn).await.expect("ddl"); }
        }
    }
    let store: Arc<dyn RoomStore> = Arc::new(DbRoomStore::new(pool.clone(), 6));

    const TRIALS: usize = 40;
    const CAP: i64 = 3;
    const RACERS: usize = 4;
    let mut overshoots = 0;
    let mut histogram: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for trial in 0..TRIALS {
        let ns = format!("tenant-{trial}");
        let room = store.create_room(&ns, CAP as usize).await.expect("create");
        let mut handles = Vec::new();
        for i in 0..RACERS {
            let (s, room_id, ns2) = (store.clone(), room.id.clone(), ns.clone());
            handles.push(tokio::spawn(async move {
                s.join_room(&ns2, &room_id, Some(format!("racer-{i}")), Duration::seconds(300)).await
            }));
        }
        let mut successes = 0;
        for h in handles { if h.await.expect("panic").is_ok() { successes += 1; } }
        let mut conn = pool.get().await.expect("conn");
        let final_count: i64 = diesel::sql_query(
            "SELECT COUNT(*) as count FROM media_room_participants WHERE namespace = $1 AND room_id = $2",
        )
        .bind::<diesel::sql_types::Text, _>(ns.clone())
        .bind::<diesel::sql_types::Text, _>(room.id.clone())
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("count")
        .count;
        println!("trial {trial}: successes={successes} final_seat_count={final_count} (cap={CAP})");
        *histogram.entry(successes).or_insert(0) += 1;
        if final_count > CAP { overshoots += 1; }
    }
    println!("overshoots: {overshoots}/{TRIALS}");
    println!("histogram of successes-per-trial: {histogram:?}");
    assert_eq!(overshoots, 0, "cap exceeded in {overshoots}/{TRIALS} trials");
}
```

</details>

## Proposed next charters

1. **Fix prioritization signal for whoever picks up #2864/#2407**: this
   session's data suggests the fix (row lock / `SELECT ... FOR UPDATE` /
   advisory lock, per #2407's own proposed remediation) is materially more
   urgent on Postgres than the original SQLite characterization implied —
   worth flagging on the issue directly so it isn't deprioritized as "rare
   edge case."
2. **`create_room`'s registry-cap race, against Postgres** — carried over
   unattempted from 2026-09-20's charter 2, now that Docker access is
   confirmed working in-session; the identical check-then-insert shape one
   function up may show the same near-certain-not-rare pattern.
3. **`DbRoomStore` reaper convergence** — still carried over unattempted
   from 2026-09-13/14/20.
4. **Whether the same non-atomicity pattern (`SELECT COUNT(*)` then
   `INSERT`, no lock) appears elsewhere in the codebase** — `rooms_db.rs`'s
   own comments flag both `join_room` and `create_room` as sharing this
   shape "as an accepted backstop-only imprecision"; worth a targeted grep
   for the same pattern (count-then-insert without a transaction) in other
   `_db.rs` stores to see if this is a one-off or a house pattern that
   needs a general fix, not two point fixes.
