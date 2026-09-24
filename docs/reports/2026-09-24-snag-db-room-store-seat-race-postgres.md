# 🪝 Snag: `DbRoomStore` seat-cap race confirmed on Postgres — rate is harness-dependent, escalates issue #2864 more modestly than first measured

**Charter:** proposed charter 1 from
[2026-09-20's session](2026-09-20-snag-db-room-store-seat-race.md) — "the
same race against Postgres, in an environment with Docker, to confirm the
window exists (and characterize its rate) against the backend most real
`db`-mode deployments will actually run." Docker was unavailable to the
three prior media-room sessions (2026-09-13, 2026-09-14, 2026-09-20); it is
available in this session's sandbox (`dockerd` started cleanly, `docker
info` succeeds), so this closes that gap.

## 🐛 Repro

**Title:** 🪝 Snag: `DbRoomStore::join_room` seat-cap race reproduces on
Postgres under both a matched-methodology comparison to #2864's SQLite
baseline (17/60) and a warm-connection-pool / no-barrier condition closer to
steady-state production load, specifically for a room's last remaining
seat (60/60 and 39/40) (data-correctness, oracle:
docs/guide/media.md "Both backends enforce the absolute 6-seat mesh
ceiling")

This is **the same bug already filed as
[issue #2864](https://github.com/autumn-foundation/autumn/issues/2864)** —
same root cause (`DbRoomStore::join_room`'s check-then-insert with no
transaction or row lock, `autumn-media-plugin/src/rooms_db.rs` ~L258-271),
same oracle. #2864 characterized it on SQLite at ~4/100 trials, under an
aggressively synchronized 16-way barrier race, and left Postgres — the
backend the "multi-process deployment" guidance is actually about —
unconfirmed.

**Correction from this report's first two revisions:** the first revision
reported condition A as "the same harness as #2864" and measured 59-60/60
overshoots, framing this as "an order of magnitude higher [rate], near-
certain." A PR review comment on #2941 (verified by re-reading #2864's own
`run_trial()`, `docs/reports/2026-09-20-snag-db-room-store-seat-race.md`
lines 190-198) correctly pointed out this wasn't true: the SQLite `run_trial`
builds **two fresh connection pools inside every trial call**; this
session's original condition A built the two pools **once, outside the
trial loop, and reused them warm across all 60 trials**. That is a real,
uncontrolled confound — a cold pool's first `.get()` after the barrier has
to establish a TCP connection (and, for the SQLite case, open a fresh
tempfile database) before it can even attempt the count-then-insert, which
stretches out and desynchronizes when each racer actually reaches the race
window; a warm, pre-established pool has no such delay, so all 16 racers
hit the critical section within microseconds of each other — closer to a
best-case exploit than to steady-state load. Rerunning condition A with
pools built fresh inside each trial, matching #2864's structure exactly,
**dropped the rate from 60/60 to 17/60** — the bug is still real and still
worse than SQLite's ~4/100, but "an order of magnitude, near-certain" was an
artifact of the harness mismatch, not a backend property. Both variants are
reported below rather than picking one, since the warm-pool condition is
itself a legitimate and arguably more production-representative scenario
(a real deployment's connection pool is built once at startup, not
recreated per request) — it's a different, useful experiment, not a
replacement for the properly-matched baseline comparison, and conflating
the two was the actual mistake.

**Condition A1 — methodology matched to #2864 (fresh pools built inside
each trial, 16 racers, 2 pools, barrier-synced, 1-seat room), Postgres
instead of SQLite:**

1. Start a real Postgres container (testcontainers) once; create its tables
   once.
2. **Inside each trial:** build two fresh `deadpool` pools over that same
   running database (mirroring #2864's `run_trial()` building two fresh
   pools per call — the confound being controlled for is pool/connection
   warmth, not database freshness, since spinning a fresh *container* per
   trial is prohibitively slow).
3. Create a room capped at 1 seat.
4. Spawn 16 concurrent joiners split 8/8 across the two pools, synchronized
   on a `tokio::sync::Barrier`.
5. Count successes and independently re-count `media_room_participants`.
6. Repeat for 60 trials, fresh namespace per trial.

**Result: 17/60 trials overshot the 1-seat cap (28%).** Histogram of
`successes` across all 60 trials:
`{1: 43, 2: 7, 3: 5, 4: 3, 5: 2}` — the modal, and majority, outcome (43/60,
72%) is the *correct* one (exactly one racer admitted); of the 17
overshoots, 12/17 (71%) admitted only 1-2 extra racers (`successes` 2 or 3),
not the near-total admission condition A2 (below) shows. Still ~7x #2864's
SQLite rate
(28% vs. ~4%), so Postgres does appear to race somewhat more readily than
SQLite even under matched methodology — plausibly because SQLite's
`busy_timeout`+WAL locking serializes writers to some degree that
Postgres's default read-committed MVCC does not — but this is a modest,
not order-of-magnitude, difference, and not the headline this report
originally claimed.

**Condition A2 — warm, pre-established pools reused across all 60 trials**
(this session's original condition A, kept and reframed rather than
discarded): identical to A1 except the two pools are built **once**, before
the trial loop, and reused warm for all 60 trials — representative of a
real server's persistent connection pool under sustained concurrent load,
rather than a fair single-variable comparison to #2864's SQLite numbers.

**Result: 60/60 trials overshot**, with `successes=16` (every racer
admitted into the 1-seat room) as the single most common value —
27/60 (45%) — full histogram
`{3: 1, 5: 1, 8: 1, 10: 2, 11: 1, 12: 3, 13: 5, 14: 5, 15: 14, 16: 27}`. A
first run of this same condition (not separately logged) measured 59/60,
consistent with a real high-but-not-necessarily-100% rate. This condition
says: **once a Postgres-backed deployment's connection pools are warmed up
under real traffic, a burst of simultaneous join requests for a room's last
seat has close to zero chance of being correctly capped** — which is
arguably the more operationally relevant number for an already-running
production app, even though it isn't a fair backend-vs-backend comparison.

**Condition B0 — empty-room initial-fill burst, no barrier, no two-process
simulation:** 4 concurrent joiners (`tokio::spawn` fired back-to-back, no
synchronization primitive) race to join a **freshly-created, empty** room
capped at 3 seats, 40 trials, single warm pool reused across trials.

**Correction:** a PR review comment pointed out this condition doesn't
actually model "a room's last seat is contended" — it models 4 requests
racing to fill an *empty* room's 3 open seats, a related but distinct
scenario (e.g. several people joining a freshly-opened scheduled call at
once, rather than piling onto a room that's already nearly full). Kept
below as its own condition, correctly labeled, rather than conflated with
the seeded last-seat condition B1 that follows.

**Result: 37/40 trials overshot (92.5%)**, and every overshoot landed at
exactly `successes=4` (all four racers admitted) — histogram `{3: 3, 4: 37}`,
no partial-overshoot case at this concurrency level. **Rerun with a fresh
pool built inside each trial** (the same A1-vs-A2 check, applied to
condition B0): **34/40 overshot (85%)**, histogram `{3: 6, 4: 34}` — a small
drop, not the ~4x collapse condition A showed. Condition B0's result is
therefore largely robust to the pool-warmth confound; the barrier in
condition A was specifically what made that condition sensitive to it
(removing pool setup as the only remaining source of arrival jitter, once
the barrier already forces synchronization, sharply narrows the race
window). Condition B0 needs no barrier because ordinary `tokio::spawn`
scheduling already puts these 4 tasks close enough together in time to
race reliably either way.

**Condition B1 — seeded last-seat contention:** the scenario condition B0
was meant to model. Same 3-seat room and 4 racers, but the room is first
seeded to **2/3 occupied** via two *sequential, awaited* `join_room` calls
(not concurrent — these aren't part of the race) before the 4 racers spawn
concurrently for the single remaining seat, 40 trials, warm pool reused
across trials.

**Result: 39/40 trials overshot (97.5%)**, and 36/40 (90%) let all 4 racers
into the single remaining seat — histogram of racer successes:
`{1: 1, 2: 1, 3: 2, 4: 36}`. Even more lopsided than B0.

**Rerun with a fresh pool built inside each trial: 0/40 overshot** — every
single trial correctly admitted exactly 1 racer and rejected the other 3
(confirmed reproducible across two independent 40-trial runs). This is
**not** evidence the bug disappears under fresh connections, and citing it
that way would be its own overclaim: the two *sequential* pre-seed joins
that ran immediately before the race, on the same freshly-built pool, leave
that pool with exactly one already-warm idle connection at the moment the
4 racers spawn. One racer reuses it and reaches the count-then-insert
window essentially instantly; the other three each pay a fresh
connection-establishment cost first. In this specific harness that gap is
apparently enough for the fast racer to complete its entire check-then-
insert and commit before any of the other three even issue their `SELECT
COUNT`, so they correctly see the now-full room and get rejected — every
time. A real production pool under load keeps *several* idle connections
ready (this harness's `max_size` is 20), so four simultaneous requests
would typically all grab already-warm connections with no such asymmetric
head start, closer to condition B1's warm-pool result (39/40) than to this
artifact. The fresh-pool number is reported for completeness and because
it's a real, reproducible measurement of *this specific harness*, not
because it generalizes to "fresh connections prevent the bug."

**Sanity check performed:** before trusting any of the above, a probe
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

**Confirms and moderately escalates #2864 on Postgres — the size of the
escalation depends on which condition is cited, and both are reported
rather than leading with the more dramatic one:**

- Under methodology matched to #2864's SQLite baseline (condition A1), the
  rate is 17/60 (28%) vs. SQLite's ~4/100 (~4%) — roughly 7x higher, a real
  but modest difference, plausibly attributable to Postgres's default
  read-committed MVCC racing more readily than SQLite's WAL+`busy_timeout`
  locking under an otherwise-identical check-then-insert. This is the
  number to cite for "is Postgres worse than SQLite for this specific
  race," and it does not support "order of magnitude" or "near-certain."
- Under a warm-connection-pool condition representative of an
  already-running production deployment's pool, **for a room's last
  remaining seat specifically** (condition A2, cap=1: 60/60; condition B1,
  seeded to 2/3 before racing for the last of 3: 39/40), the failure rate
  **given that a burst of simultaneous join requests occurs** is 97.5-100%,
  and both conditions show full admission (every racer let in) as the
  single most common individual outcome (condition A2: 27/60, 45%;
  condition B1: 36/40, 90%) — not a majority in A2's case, a majority in
  B1's. A related but distinct scenario — an empty room's initial-fill
  burst rather than its last seat specifically (condition B0) — shows a
  similar 85-92.5% warm-pool rate. None of these probes measured real
  arrival rates or workload patterns, only the outcome of a
  deliberately-launched concurrent burst — so this is a statement about
  what happens *if* such a burst occurs, not an estimate of how often one
  does in a live deployment.
- Postgres is the backend the docs frame as "the correct backend for a
  horizontally-scaled or multi-process deployment," i.e. the one operators
  choose specifically because they expect concurrent load. When a burst of
  simultaneous joins for a room's last seat(s) does occur, the claim is
  falsest exactly where it's relied on most (production-representative
  condition) to measurably-but-moderately false even under the more
  conservative matched-methodology condition (17/60, 28%) — but how often
  such a burst occurs in any given deployment is outside what this session
  measured.
- Still not crash/hang/data-loss — no error, no corruption, the room
  simply silently seats more participants than its documented ceiling,
  which for a WebRTC mesh (O(n²) peer connections) can push participant
  clients into far more simultaneous connections than the ceiling was
  chosen to bound. Severity classification stays "data-correctness /
  documented-claim violation," per the same reasoning #2864 already gives;
  the *likelihood* component moves from "narrow window" (SQLite's
  characterization) to somewhere between "roughly 7x more likely" and "the
  common case," depending on deployment shape, rather than uniformly "the
  common case" as an earlier revision of this report claimed.

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

# Condition A1 — matched to #2864's SQLite methodology (fresh pools/trial):
# add autumn-media-plugin/tests/snag_pg_seat_race_freshpool.rs (listing
# below), then:
cargo test -p autumn-media-plugin --test snag_pg_seat_race_freshpool -- --ignored --nocapture
# Expect a minority of 60 trials to overshoot the 1-seat cap (this session:
# 17/60, histogram {1: 43, 2: 7, 3: 5, 4: 3, 5: 2}).
rm autumn-media-plugin/tests/snag_pg_seat_race_freshpool.rs

# Condition A2 — warm pools reused across trials (production-representative,
# NOT a fair SQLite comparison): add
# autumn-media-plugin/tests/snag_pg_seat_race_probe.rs (listing below), then:
cargo test -p autumn-media-plugin --test snag_pg_seat_race_probe -- --ignored --nocapture
# Expect nearly all 60 trials to overshoot, most often to the full 16/16
# racer count (this session: 60/60 overshot, mode successes=16 in 27/60).
rm autumn-media-plugin/tests/snag_pg_seat_race_probe.rs

# Condition B0 (empty-room initial-fill burst, warm pool) — add
# autumn-media-plugin/tests/snag_pg_seat_race_lowconc.rs (listing below):
cargo test -p autumn-media-plugin --test snag_pg_seat_race_lowconc -- --ignored --nocapture
# Expect the large majority of 40 trials to overshoot the 3-seat cap, almost
# always to exactly 4/4 (this session: 37/40 overshot, all 37 at successes=4).
rm autumn-media-plugin/tests/snag_pg_seat_race_lowconc.rs

# Condition B0 (fresh pool/trial, confirms B0 is largely robust to the
# A1-vs-A2 confound) — add
# autumn-media-plugin/tests/snag_pg_seat_race_lowconc_freshpool.rs (listing
# below):
cargo test -p autumn-media-plugin --test snag_pg_seat_race_lowconc_freshpool -- --ignored --nocapture
# Expect a similarly large majority to overshoot (this session: 34/40,
# histogram {3: 6, 4: 34} — a small drop from B0's warm-pool 37/40, not the
# ~4x collapse condition A showed).
rm autumn-media-plugin/tests/snag_pg_seat_race_lowconc_freshpool.rs

# Condition B1 (seeded last-seat contention, warm pool — the scenario B0
# was meant to model) — add
# autumn-media-plugin/tests/snag_pg_lastseat_lowconc.rs (listing below):
cargo test -p autumn-media-plugin --test snag_pg_lastseat_lowconc -- --ignored --nocapture
# Expect the large majority of 40 trials to overshoot the 3-seat cap (this
# session: 39/40 overshot, 36/40 at racer_successes=4).
rm autumn-media-plugin/tests/snag_pg_lastseat_lowconc.rs

# Condition B1 (fresh pool/trial) — add
# autumn-media-plugin/tests/snag_pg_lastseat_freshpool.rs (listing below).
# CAUTION: this one reproducibly shows 0/40 overshoot, but read the
# condition B1 writeup above before trusting that at face value — it's a
# connection-establishment artifact from the two sequential preseed joins,
# not evidence the bug is absent:
cargo test -p autumn-media-plugin --test snag_pg_lastseat_freshpool -- --ignored --nocapture
# Expect 0/40 overshoot, reproducible across repeated runs (confirmed twice
# this session) — see the explanation above, not a clean negative result.
rm autumn-media-plugin/tests/snag_pg_lastseat_freshpool.rs
```

<details>
<summary><code>snag_pg_seat_race_freshpool.rs</code> (16-racer / 2-pool / barrier-synced, fresh pools per trial, condition A1 — matched to #2864)</summary>

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

fn build_pool(url: &str) -> Pool<AsyncPgConnection> {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    Pool::builder(manager).max_size(20).build().expect("pool")
}

// Fresh pools built INSIDE run_trial, matching #2864's SQLite run_trial()
// exactly in structure (only the DB/container is shared here, since a
// fresh Postgres container per trial is prohibitively slow — the confound
// under test is pool/connection warmth, not database freshness).
async fn run_trial(url: &str, trial: usize) -> (usize, i64) {
    let (pool_a, pool_b) = (build_pool(url), build_pool(url));
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
async fn pg_fresh_pools_per_trial_still_overshoots() {
    let container = Postgres::default().start().await.expect("start postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    {
        let mut conn = build_pool(&url).get().await.expect("conn");
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
        let (successes, final_count) = run_trial(&url, trial).await;
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
<summary><code>snag_pg_seat_race_probe.rs</code> (16-racer / 2-pool / barrier-synced, warm pools reused across trials, condition A2)</summary>

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
<summary><code>snag_pg_seat_race_lowconc.rs</code> (4-racer / single warm pool reused across trials / no-barrier, empty-room burst, condition B0)</summary>

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

<details>
<summary><code>snag_pg_seat_race_lowconc_freshpool.rs</code> (4-racer / fresh pool built per trial / no-barrier, empty-room burst, condition B0 fresh-pool rerun)</summary>

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

fn build_pool(url: &str) -> Pool<AsyncPgConnection> {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    Pool::builder(manager).max_size(20).build().expect("pool")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers) - SCRATCH PROBE, not for CI"]
async fn pg_low_concurrency_fresh_pool_per_trial() {
    let container = Postgres::default().start().await.expect("start postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    {
        let mut conn = build_pool(&url).get().await.expect("conn");
        for stmt in CREATE_TABLES_SQL.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() { diesel::sql_query(stmt).execute(&mut conn).await.expect("ddl"); }
        }
    }

    const TRIALS: usize = 40;
    const CAP: i64 = 3;
    const RACERS: usize = 4;
    let mut overshoots = 0;
    let mut histogram: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for trial in 0..TRIALS {
        let pool = build_pool(&url);
        let store: Arc<dyn RoomStore> = Arc::new(DbRoomStore::new(pool.clone(), 6));
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

<details>
<summary><code>snag_pg_lastseat_lowconc.rs</code> (3-seat room seeded to 2/3 occupied, 4 racers for the last seat, warm pool, condition B1)</summary>

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

fn build_pool(url: &str) -> Pool<AsyncPgConnection> {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    Pool::builder(manager).max_size(20).build().expect("pool")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers) - SCRATCH PROBE, not for CI"]
async fn pg_last_seat_contention_warm_pool() {
    let container = Postgres::default().start().await.expect("start postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    {
        let mut conn = build_pool(&url).get().await.expect("conn");
        for stmt in CREATE_TABLES_SQL.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() { diesel::sql_query(stmt).execute(&mut conn).await.expect("ddl"); }
        }
    }
    let pool = build_pool(&url);
    let store: Arc<dyn RoomStore> = Arc::new(DbRoomStore::new(pool.clone(), 6));

    const TRIALS: usize = 40;
    const CAP: i64 = 3;
    const PRESEED: usize = 2; // occupy 2 of 3 seats before racing for the last one
    const RACERS: usize = 4;  // 4 requests race for the single remaining seat
    let mut overshoots = 0;
    let mut histogram: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for trial in 0..TRIALS {
        let ns = format!("tenant-{trial}");
        let room = store.create_room(&ns, CAP as usize).await.expect("create");
        for p in 0..PRESEED {
            store
                .join_room(&ns, &room.id, Some(format!("preseed-{p}")), Duration::seconds(300))
                .await
                .expect("preseed join");
        }
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
        println!("trial {trial}: racer_successes={successes} final_seat_count={final_count} (cap={CAP}, preseeded={PRESEED})");
        *histogram.entry(successes).or_insert(0) += 1;
        if final_count > CAP { overshoots += 1; }
    }
    println!("overshoots: {overshoots}/{TRIALS}");
    println!("histogram of racer_successes-per-trial: {histogram:?}");
    assert_eq!(overshoots, 0, "cap exceeded in {overshoots}/{TRIALS} trials");
}
```

</details>

<details>
<summary><code>snag_pg_lastseat_freshpool.rs</code> (same as above, fresh pool built per trial, condition B1 fresh-pool rerun — reproducibly 0/40, see explanation above before citing)</summary>

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

fn build_pool(url: &str) -> Pool<AsyncPgConnection> {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    Pool::builder(manager).max_size(20).build().expect("pool")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Docker (testcontainers) - SCRATCH PROBE, not for CI"]
async fn pg_last_seat_contention_fresh_pool_per_trial() {
    let container = Postgres::default().start().await.expect("start postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    {
        let mut conn = build_pool(&url).get().await.expect("conn");
        for stmt in CREATE_TABLES_SQL.split(';') {
            let stmt = stmt.trim();
            if !stmt.is_empty() { diesel::sql_query(stmt).execute(&mut conn).await.expect("ddl"); }
        }
    }

    const TRIALS: usize = 40;
    const CAP: i64 = 3;
    const PRESEED: usize = 2;
    const RACERS: usize = 4;
    let mut overshoots = 0;
    let mut histogram: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for trial in 0..TRIALS {
        let pool = build_pool(&url);
        let store: Arc<dyn RoomStore> = Arc::new(DbRoomStore::new(pool.clone(), 6));
        let ns = format!("tenant-{trial}");
        let room = store.create_room(&ns, CAP as usize).await.expect("create");
        for p in 0..PRESEED {
            store
                .join_room(&ns, &room.id, Some(format!("preseed-{p}")), Duration::seconds(300))
                .await
                .expect("preseed join");
        }
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
        println!("trial {trial}: racer_successes={successes} final_seat_count={final_count} (cap={CAP}, preseeded={PRESEED})");
        *histogram.entry(successes).or_insert(0) += 1;
        if final_count > CAP { overshoots += 1; }
    }
    println!("overshoots: {overshoots}/{TRIALS}");
    println!("histogram of racer_successes-per-trial: {histogram:?}");
    assert_eq!(overshoots, 0, "cap exceeded in {overshoots}/{TRIALS} trials");
}
```

</details>

## Proposed next charters

1. **Fix prioritization signal for whoever picks up #2864/#2407**: this
   session's data confirms the fix (row lock / `SELECT ... FOR UPDATE` /
   advisory lock, per #2407's own proposed remediation) applies to Postgres
   too, at a rate that's modestly higher than SQLite's under matched
   methodology (condition A1, ~7x) and much higher under a
   production-representative warm-pool condition, specifically for a
   room's last seat (condition A2/B1, 97.5-100%) — worth citing both
   numbers on the issue rather than either alone, so priority isn't set
   from just the more dramatic one.
2. **`create_room`'s registry-cap race, against Postgres** — carried over
   unattempted from 2026-09-20's charter 2, now that Docker access is
   confirmed working in-session; worth probing with *both* the matched
   (fresh-pool) and warm-pool methodologies from the start this time,
   given how much the pool-warmth confound mattered for the barrier-synced
   condition here.
3. **`DbRoomStore` reaper convergence** — still carried over unattempted
   from 2026-09-13/14/20.
4. **Whether the same non-atomicity pattern (`SELECT COUNT(*)` then
   `INSERT`, no lock) appears elsewhere in the codebase** — `rooms_db.rs`'s
   own comments flag both `join_room` and `create_room` as sharing this
   shape "as an accepted backstop-only imprecision"; worth a targeted grep
   for the same pattern (count-then-insert without a transaction) in other
   `_db.rs` stores to see if this is a one-off or a house pattern that
   needs a general fix, not two point fixes.
