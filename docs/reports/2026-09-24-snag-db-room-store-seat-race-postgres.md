# 🪝 Snag: `DbRoomStore` seat-cap race confirmed on Postgres — near-deterministic on a warm production pool, indistinguishable from SQLite's rate once cold-start methodology is fully matched

**Charter:** proposed charter 1 from
[2026-09-20's session](2026-09-20-snag-db-room-store-seat-race.md) — "the
same race against Postgres, in an environment with Docker, to confirm the
window exists (and characterize its rate) against the backend most real
`db`-mode deployments will actually run." Docker was unavailable to the
three prior media-room sessions (2026-09-13, 2026-09-14, 2026-09-20); it is
available in this session's sandbox (`dockerd` started cleanly, `docker
info` succeeds), so this closes that gap.

## 🐛 Repro

**Title:** 🪝 Snag: `DbRoomStore::join_room` seat-cap race is near-
deterministic on a properly-warmed Postgres pool for a room's last seat
(59/59 and 40/40 once two measurement artifacts are corrected); under a
fully cold-start-matched comparison to #2864's SQLite baseline it is no
longer clearly distinguishable from SQLite's own ~4/100 (data-correctness,
oracle: docs/guide/media.md "Both backends enforce the absolute 6-seat mesh
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

**Second correction, from a fourth review round:** two more measurement
artifacts were found and fixed after the above was written — both real,
both confirmed against the actual per-trial data before accepting them,
and both change headline numbers again:

1. **A1 still wasn't a fair comparison to SQLite.** #2864's SQLite
   `run_trial()` opens a **brand-new tempfile database** every trial, so
   its tables are always empty. A1's fresh-*pools*-per-trial fix (above)
   left the *tables* shared across all 60 trials on one Postgres database,
   so row/index state accumulated as trials progressed (up to ~1000 rows by
   trial 60) — a remaining unmatched variable. Rerunning with the tables
   also dropped and recreated inside every trial (true schema isolation,
   matching SQLite's per-trial freshness) **dropped the rate further, to
   2/60 (3.3%) and 8/60 (13.3%) across two independent runs** — both far
   below the original "matched" 17/60 (28%), and now in the same
   neighborhood as SQLite's own ~4/100 rather than clearly above it. The
   two runs disagree with each other by 4x (2/60 vs 8/60), which is itself
   informative: at these low base rates, 60-trial samples carry enough
   noise that neither this report nor #2864's own single 4/100 SQLite
   measurement should be read as a precise rate — only as "low, but real
   and nonzero on both backends." **The "~7x SQLite" comparative claim in
   this report's second revision does not hold up under full methodology
   matching and is retracted;** see condition A1 below for the corrected
   numbers.
2. **A2 and B1's "warm pool" trials weren't all actually warm, for two
   different reasons.** A2 ran its one DDL setup query through `pool_a`
   directly — the same pool object its racers later use — leaving exactly
   one of its connections idle-but-returned before trial 0. B1 runs DDL
   through a separate, temporary pool built and dropped just for that
   query; its *measured* pool starts with zero warm connections, and the
   one connection idle by the time trial 0's racers spawn comes from
   `create_room` and the two sequential preseed `join_room` calls that
   immediately precede them, reusing the same connection. Either way, the
   result is the same shape: trial 0 raced over a pool with only 1 of the
   N connections its racers would need actually established, while every
   later trial raced over a fully warm pool. Checked directly against the
   saved per-trial logs: **A2's sole below-average outcome (`successes=3`, the minimum in
   its histogram) was trial 0, and B1's sole non-overshoot trial
   (`successes=1`) was also trial 0** — both exactly consistent with an
   incomplete-warm-up artifact rather than a genuine race outcome, not
   coincidence. Corrected below by excluding the contaminated trial 0 from
   A2's existing data (explicitly sanctioned as an option by the review
   comment that raised this) and by rerunning B1 with the pool properly
   prewarmed (racers-worth of connections explicitly acquired and released
   before the trial loop starts) rather than relying on incidental DDL
   warm-up.

**Condition A1 — methodology matched to #2864 (fresh pools AND fresh,
dropped-and-recreated tables built inside each trial, 16 racers, 2 pools,
barrier-synced, 1-seat room), Postgres instead of SQLite:**

1. Start a real Postgres container (testcontainers) once.
2. **Inside each trial:** build two fresh `deadpool` pools (`pool_a`,
   `pool_b`) over that same running database, then `DROP TABLE`+
   `CREATE TABLE` both tables through `pool_a` (mirroring #2864's
   `run_trial()` exactly, including which pool runs the DDL — a fresh
   Postgres *container* per trial would give the same table-freshness
   guarantee but is prohibitively slow, so a fresh schema on the existing
   container is the practical equivalent of SQLite's fresh database file).
   This leaves `pool_a` with one warm connection and `pool_b` fully cold
   at trial start, the same asymmetry #2864's own SQLite baseline has —
   not a new confound this report introduces, but one inherited
   intentionally to keep the comparison apples-to-apples.
3. Create a room capped at 1 seat.
4. Spawn 16 concurrent joiners split 8/8 across the two pools, synchronized
   on a `tokio::sync::Barrier`.
5. Count successes and independently re-count `media_room_participants`.
6. Repeat for 60 trials.

**Result: 2/60 (3.3%) and 8/60 (13.3%) overshot across two independent
60-trial runs** (histograms `{1: 58, 2: 1, 3: 1}` and
`{1: 52, 2: 3, 3: 4, 4: 1}` respectively) — combined, 10/120 (8.3%). This is
no longer clearly distinguishable from #2864's own ~4/100 SQLite rate given
the sample sizes and the 4x spread between the two Postgres runs
themselves; the earlier "~7x SQLite, modest but real backend difference"
claim was itself downstream of the unmatched table-freshness confound and
does not survive fixing it. The honest conclusion from A1 is: **once
methodology is fully matched (fresh pools, fresh schema, same
barrier-synced 16-racer harness), this race's cold-start rate is low and
roughly comparable between Postgres and SQLite — not dramatically worse on
either backend.**

**Condition A2 — warm, pre-established pools reused across all 60 trials**
(this session's original condition A, kept and reframed rather than
discarded): identical to A1's harness except the tables and the two pools
are **not** rebuilt per trial — tables persist and the two pools are built
**once**, before the trial loop, and reused warm for all 60 trials —
representative of a real server's persistent connection pool and
long-lived tables under sustained concurrent load, not a fair
single-variable comparison to #2864's SQLite numbers.

**Result: 60/60 trials overshot**, full histogram
`{3: 1, 5: 1, 8: 1, 10: 2, 11: 1, 12: 3, 13: 5, 14: 5, 15: 14, 16: 27}`
— **but trial 0 (the sole `successes=3` outcome, the histogram's minimum)
is the warm-up artifact described above**, not a genuine measurement:
checked directly against the saved log, it's the very first trial, right
after the single DDL-setup connection was the only warm one available.
Excluding it: **59/59 trials overshot (100%)** among genuinely-warm trials,
with `successes=16` (every racer admitted) the single most common
individual outcome at 27/59 (45.8%). A separate first run of this same
condition (not logged in full) measured 59/60, consistent with a real
high rate with some run-to-run noise. This condition says: **once a
Postgres-backed deployment's connection pools are actually warmed up under
real traffic, a burst of simultaneous join requests for a room's last seat
has essentially zero chance of being correctly capped** — the more
operationally relevant number for an already-running production app, even
though it isn't a fair backend-vs-backend comparison.

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
no partial-overshoot case at this concurrency level. **Checked for the same
trial-0 warm-up artifact found in A2 and B1** (this listing also opens only
one DDL connection before the trial loop, with no explicit prewarm): trial
0 *is* one of the three `successes=3` outcomes in the saved log. Excluding
it: **37/39 overshot (94.9%)**. Unlike A2 and B1, though, the other **two**
non-overshoot trials are *not* trial 0 — genuine variance survives even
after removing the one confirmed-contaminated trial, so B0 is not fully
explained by the warm-up artifact the way A2 and B1 were. **Rerun with a
fresh pool built inside each trial** (the same A1-vs-A2 check, applied to
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
`{1: 1, 2: 1, 3: 2, 4: 36}`. **The sole non-overshoot trial (`successes=1`)
is, again, trial 0** — checked directly against the saved log — the same
incomplete-warm-up shape identified in condition A2 above, though from a
different source here: this harness's DDL runs through a separate,
temporary pool that's dropped right after, so the *measured* pool starts
fully cold; the one connection that's idle-but-returned by the time trial
0's racers spawn comes from `create_room` and the two sequential preseed
`join_room` calls immediately before them (all three reuse the same
connection in sequence), not from DDL. **Rerunning with the pool
explicitly prewarmed first** (acquiring and releasing 4
connections — one per racer — before the trial loop starts, instead of
relying on incidental DDL warm-up): **40/40 trials overshot (100%)**,
histogram `{2: 2, 3: 2, 4: 36}` — genuinely deterministic once the pool is
actually warm before measurement begins, not 39/40 with an artifact mixed
in.

**Rerun (of the original, non-prewarmed version) with a fresh pool built
inside each trial: 0/40 overshot** — every single trial correctly admitted
exactly 1 racer and rejected the other 3 (confirmed reproducible across two
independent 40-trial runs). This is **not** evidence the bug disappears
under fresh connections, and citing it that way would be its own overclaim:
the two *sequential* pre-seed joins that ran immediately before the race,
on the same freshly-built pool, leave that pool with exactly one
already-warm idle connection at the moment the 4 racers spawn. One racer
reuses it and reaches the count-then-insert window essentially instantly;
the other three each pay a fresh connection-establishment cost first. In
this specific harness that gap is apparently enough for the fast racer to
complete its entire check-then-insert and commit before any of the other
three even issue their `SELECT COUNT`, so they correctly see the now-full
room and get rejected — every time.

This does **not** mean a warm production pool is immune the way this
harness's pathological single-warm-connection case is — it means the
outcome depends on how much *idle capacity* is actually available at the
moment of the burst relative to the burst's size, not simply on whether
the pool object has been "warmed up" at some point. `max_size: 20` is a
ceiling, not a guarantee of 20 idle connections: this report's own B1
data brackets the two extremes directly — 1 idle connection available for
4 racers (this fresh-pool case) produced 0/40 overshoots (only the
connection-holder got in), while B1's prewarmed case, which explicitly
holds and releases 4 connections (matching the burst size) before
measuring, produced 40/40. A production pool that happens to have at
least burst-sized idle capacity free at the moment of the burst would
plausibly look like the prewarmed case; a pool that's mostly saturated
with other work at that moment could look more like this one. This
session did not test intermediate idle-capacity levels (e.g. 2 idle
connections for 4 racers), so the claim below is scoped to "when
burst-sized idle capacity is available," not to warm pools in general.
The fresh-pool number here is reported for completeness and because it's
a real, reproducible measurement of *this specific harness*, not because
it generalizes to "fresh connections prevent the bug."

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

**Confirms #2864 on Postgres. The headline is not "Postgres races more
readily than SQLite" — under full methodology matching that claim doesn't
hold up — it's "under conditions representative of an actually-running
Postgres deployment, this race is essentially deterministic." This session
tested warm-pool conditions only on Postgres; #2864's SQLite numbers are
all cold-start (fresh pool and database every trial), so nothing below
claims to know SQLite's warm-pool rate — it wasn't measured:**

- Under a fully cold-start-matched comparison to #2864's SQLite baseline
  (condition A1: fresh pools *and* fresh schema every trial, matching
  SQLite's fresh-tempfile-database-per-trial exactly), the rate is 2/60 and
  8/60 across two independent runs (3.3%-13.3%, combined 8.3%) vs. SQLite's
  ~4/100 (~4%). **This is no longer clearly distinguishable from SQLite's
  rate** — both are low, both are real and nonzero, and the two Postgres
  runs disagree with each other by 4x, which says more about how much noise
  a 60-trial sample carries at this base rate than about a backend
  difference. This report's earlier claims of "an order of magnitude
  higher" (first revision) and then "~7x higher" (second revision) were
  both artifacts of incompletely-matched methodology (pool warmth, then
  table/schema freshness) and neither is supported once methodology is
  fully matched. **Do not cite this report for "Postgres is worse than
  SQLite for this race" — the honest conclusion is "comparable, both low,
  under matched cold-start conditions."**
- Under conditions representative of an already-running production
  deployment's connection pool that has **at least burst-sized idle
  capacity available at the moment of the burst** (see condition B1's own
  writeup for why that qualifier matters — this session's data shows the
  rate depends on available idle connections relative to burst size, not
  simply on whether the pool has been "warmed up" at some point), **for a
  room's last remaining seat specifically** (condition A2 excluding its
  one warm-up-contaminated trial, cap=1: 59/59; condition B1 properly
  prewarmed with burst-sized idle capacity, seeded to 2/3 before racing
  for the last of 3: 40/40), the failure rate **given that a burst of
  simultaneous join requests occurs** is **100% in both conditions** — not
  "85-100%" or "97.5-100%" as earlier revisions of this report said before
  two measurement artifacts (an incompletely-warmed pool contaminating
  trial 0 in both A2 and B1) were found and corrected. A related but
  distinct scenario — an empty room's
  initial-fill burst rather than contention for its last seat specifically
  (condition B0) — shows a somewhat lower rate: 92.5% with a warm pool
  (94.9% excluding its own confirmed trial-0 warm-up artifact — see
  condition B0 above), 85% with a fresh pool per trial (these are separate
  conditions, not points on one range — see condition B0 above for which
  number is which). None
  of these probes measured real arrival rates or workload patterns, only
  the outcome of a deliberately-launched concurrent burst — so this is a
  statement about what happens *if* such a burst occurs, not an estimate of
  how often one does in a live deployment.
- Postgres is the backend the docs frame as "the correct backend for a
  horizontally-scaled or multi-process deployment," i.e. the one operators
  choose specifically because they expect concurrent load. When a burst of
  simultaneous joins for a room's last seat does occur against an
  already-warm **Postgres** production pool, the claim is essentially
  always false. This session found no evidence that Postgres is
  meaningfully worse than SQLite at resisting the race itself under
  equivalent cold-start conditions — but it also did not test SQLite under
  warm-pool conditions, so it cannot say whether SQLite's warm-pool rate is
  similarly deterministic, lower, or untestable in the same way (SQLite's
  own connection is typically process-local, not pooled the same way
  Postgres's is). The severity claim here is scoped to Postgres, the
  backend actually measured warm.
- Still not crash/hang/data-loss — no error, no corruption, the room
  simply silently seats more participants than its documented ceiling,
  which for a WebRTC mesh (O(n²) peer connections) can push participant
  clients into far more simultaneous connections than the ceiling was
  chosen to bound. Severity classification stays "data-correctness /
  documented-claim violation," per the same reasoning #2864 already gives;
  the *likelihood* component is "the common case" under a warm **Postgres**
  production pool contending for a room's last seat (not claimed for
  SQLite, which this session never tested warm), and "low but real,
  roughly comparable between backends" under the cold-start-matched
  comparison that *was* run on both — not the SQLite-vs-Postgres severity
  gradient earlier revisions of this report claimed.

## Dedup search

Searched open issues for `DbRoomStore`, "seat cap", "join race", "mesh
ceiling", "Postgres". This is the same bug as **#2864** (not a duplicate to
file separately) — same function, same root cause, same oracle citation —
so this session's finding is reported as new data on that issue rather than
a new filing, per the dedup requirement. **#2407** remains the
already-noted sibling (reaper-cascade race vs. this self-race); unchanged
from #2864's own dedup note.

## 🔬 Reproduce

Eight scratch test files, each its own `[[test]]`-shaped file under
`autumn-media-plugin/tests/`, were added across this session's several
rounds of correction — two for condition A1 (an intermediate fresh-pool-
only version, then the final fresh-pool-and-fresh-schema version), one for
A2, two for B0 (warm and fresh-pool), and three for B1 (warm-but-not-
prewarmed, the final explicitly-prewarmed version, and a fresh-pool
rerun) — run against a live Docker/testcontainers Postgres, and then
**removed** (not committed) after each.

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
Adding it there, given how consistently conditions A2/B1 (properly warmed)
fail, would make that Docker CI step fail on nearly every run, and this
repo has no established "expected-fail/quarantined" marking convention the
step would respect. That is the same practical outcome the first revision
described (a committed version would break CI), reached by the correct
mechanism (naming it into an always-run target, not tripping a sweep that
doesn't exist for this crate) — worth being precise about, since a future
contributor relying on the wrong mechanism could wrongly conclude a
*different* new standalone file is safe to commit when it would in fact
just never run.

```bash
cd /home/user/autumn
# Ensure Docker is running (this sandbox needed `dockerd` started manually;
# a normal CI/dev box with the Docker daemon already up can skip this):
#   nohup dockerd >/tmp/dockerd.log 2>&1 & sleep 5 && docker info

# Condition A1, fresh pools only (an intermediate step, NOT the final fair
# comparison — kept for its own sake since it's what exposed the
# pool-warmth confound in the first place) — add
# autumn-media-plugin/tests/snag_pg_seat_race_freshpool.rs (listing below):
cargo test -p autumn-media-plugin --test snag_pg_seat_race_freshpool -- --ignored --nocapture
# This session: 17/60 — see condition A1's writeup above for why this
# number is superseded by the fresh-schema rerun below.
rm autumn-media-plugin/tests/snag_pg_seat_race_freshpool.rs

# Condition A1, fresh pools AND fresh (dropped+recreated) schema per trial
# — the actual fair comparison to #2864's SQLite baseline — add
# autumn-media-plugin/tests/snag_pg_seat_race_freshpool_freshschema.rs
# (listing below):
cargo test -p autumn-media-plugin --test snag_pg_seat_race_freshpool_freshschema -- --ignored --nocapture
# Run it twice — this session: 2/60 then 8/60, i.e. low and noisy, roughly
# comparable to SQLite's ~4/100, not clearly higher.
rm autumn-media-plugin/tests/snag_pg_seat_race_freshpool_freshschema.rs

# Condition A2 — warm pools AND persistent tables reused across trials
# (production-representative, NOT a fair SQLite comparison) — add
# autumn-media-plugin/tests/snag_pg_seat_race_probe.rs (listing below):
cargo test -p autumn-media-plugin --test snag_pg_seat_race_probe -- --ignored --nocapture
# Expect trial 0 to be an outlier (incomplete pool warm-up — see condition
# A2's writeup above) and every trial after it to overshoot (this session:
# 60/60 including trial 0, 59/59 excluding it, mode successes=16 at 27/59).
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

# Condition B1, warm pool but NOT explicitly prewarmed (shows the same
# trial-0 artifact as condition A2 — kept to demonstrate it) — add
# autumn-media-plugin/tests/snag_pg_lastseat_lowconc.rs (listing below):
cargo test -p autumn-media-plugin --test snag_pg_lastseat_lowconc -- --ignored --nocapture
# This session: 39/40, with trial 0 as the sole non-overshoot — see
# condition B1's writeup above.
rm autumn-media-plugin/tests/snag_pg_lastseat_lowconc.rs

# Condition B1, explicitly prewarmed (acquire-and-release RACERS
# connections before the trial loop) — the corrected number — add
# autumn-media-plugin/tests/snag_pg_lastseat_prewarmed.rs (listing below):
cargo test -p autumn-media-plugin --test snag_pg_lastseat_prewarmed -- --ignored --nocapture
# Expect all 40 trials to overshoot (this session: 40/40, histogram
# {2: 2, 3: 2, 4: 36}).
rm autumn-media-plugin/tests/snag_pg_lastseat_prewarmed.rs

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
<summary><code>snag_pg_seat_race_freshpool.rs</code> (16-racer / 2-pool / barrier-synced, fresh pools per trial only — intermediate step, superseded by the fresh-schema version below)</summary>

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
<summary><code>snag_pg_seat_race_freshpool_freshschema.rs</code> (16-racer / 2-pool / barrier-synced, fresh pools AND fresh dropped+recreated tables per trial — the actual fair comparison to #2864, condition A1 final)</summary>

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

const DROP_TABLES_SQL: &str = "
    DROP TABLE IF EXISTS media_room_participants;
    DROP TABLE IF EXISTS media_rooms;
";
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

async fn run_ddl(pool: &Pool<AsyncPgConnection>, sql: &str) {
    let mut conn = pool.get().await.expect("conn");
    for stmt in sql.split(';') {
        let stmt = stmt.trim();
        if !stmt.is_empty() {
            diesel::sql_query(stmt).execute(&mut conn).await.expect("ddl");
        }
    }
}

// Fresh pools AND fresh (dropped+recreated) tables inside every trial.
async fn run_trial(url: &str, trial: usize) -> (usize, i64) {
    let (pool_a, pool_b) = (build_pool(url), build_pool(url));
    run_ddl(&pool_a, DROP_TABLES_SQL).await;
    run_ddl(&pool_a, CREATE_TABLES_SQL).await;

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
async fn pg_fresh_pools_and_fresh_schema_per_trial() {
    let container = Postgres::default().start().await.expect("start postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

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
<summary><code>snag_pg_seat_race_probe.rs</code> (16-racer / 2-pool / barrier-synced, warm pools AND persistent tables reused across trials, condition A2)</summary>

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
<summary><code>snag_pg_lastseat_lowconc.rs</code> (3-seat room seeded to 2/3 occupied, 4 racers for the last seat, warm pool but not explicitly prewarmed — shows the trial-0 artifact, condition B1 intermediate)</summary>

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
<summary><code>snag_pg_lastseat_prewarmed.rs</code> (same as above, but explicitly prewarms RACERS connections before the trial loop — the corrected number, condition B1 final)</summary>

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
async fn pg_last_seat_contention_warm_pool_explicitly_prewarmed() {
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

    const RACERS: usize = 4;
    // Explicitly prewarm: acquire RACERS connections concurrently, then
    // release them all, so the pool actually has RACERS warm connections
    // ready before trial 0 (not just the 1 left warm by create_room and
    // the sequential preseed joins below).
    {
        let mut conns = Vec::new();
        for _ in 0..RACERS {
            conns.push(pool.get().await.expect("prewarm conn"));
        }
        drop(conns);
    }

    const TRIALS: usize = 40;
    const CAP: i64 = 3;
    const PRESEED: usize = 2;
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
   too, and that the deciding factor for urgency is deployment shape, not
   backend choice — under a fully cold-start-matched comparison (the only
   axis tested on both backends) Postgres and SQLite are roughly comparable
   and both low (condition A1, 3.3-13.3% vs. SQLite's ~4%), but under
   conditions representative of an already-running deployment's warm
   connection pool, contention for a room's last seat is essentially
   deterministic **on Postgres** (condition A2/B1, 100% — SQLite's
   warm-pool rate was never measured, by this session or #2864's own, so
   this is not a claim about "either backend") — worth citing the
   Postgres warm-pool number as the operationally relevant one, since
   "Postgres is worse than SQLite" is not what this session's
   fully-corrected data supports.
2. **`create_room`'s registry-cap race, against Postgres** — carried over
   unattempted from 2026-09-20's charter 2, now that Docker access is
   confirmed working in-session; worth probing with *all three*
   methodology axes from the start this time (pool freshness, schema
   freshness, and explicit prewarming for the "warm" condition), given how
   many rounds of review it took this session to find and control for each
   one in `join_room`'s equivalent race.
3. **`DbRoomStore` reaper convergence** — still carried over unattempted
   from 2026-09-13/14/20.
4. **Whether the same non-atomicity pattern (`SELECT COUNT(*)` then
   `INSERT`, no lock) appears elsewhere in the codebase** — `rooms_db.rs`'s
   own comments flag both `join_room` and `create_room` as sharing this
   shape "as an accepted backstop-only imprecision"; worth a targeted grep
   for the same pattern (count-then-insert without a transaction) in other
   `_db.rs` stores to see if this is a one-off or a house pattern that
   needs a general fix, not two point fixes.
