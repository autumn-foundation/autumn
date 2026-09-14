# 🪝 Snag: exploratory QA session report — examples/media-room registry cap + reaper

**Charter:** operator running a single-node `media-room` deployment at
sustained load — drive `InMemoryRoomStore`'s documented 10,000-room registry
cap and background reaper live over HTTP, closing out charter 3 from
[2026-09-13's media-room session](2026-09-13-snag-media-room-session.md)
("Registry-full backstop at scale").

**Time spent:** ~45 minutes (build + live drive), well under the 30-CI-minute
generative-sweep budget — this was a direct HTTP drive, not a fuzz/property
sweep.

**Environment:** `examples/media-room` built from commit `e87bbde3`, run as
the real compiled binary (`cargo build -p media-room`), `127.0.0.1:3100`,
Linux container, no Docker (unavailable in this session's sandbox — confirmed
`dockerd` cannot start, `ulimit: error setting limit (Operation not
permitted)`). `media-room` needs no database (in-memory `RoomStore` by
default), so this was reachable without it. Reaper tunables set via the
documented env overrides to compress the wait:
`AUTUMN_MEDIA__ROOM_REAPER_INTERVAL_SECONDS=5`,
`AUTUMN_MEDIA__ROOM_IDLE_TTL_SECONDS=8` (defaults are 60s / 900s). `MAX_ROOMS`
itself (10,000) is not configurable in the shipped example, so the real cap
was driven, not a scaled-down stand-in.

## Oracle

`docs/guide/media.md` (the "Room store backends" section): *"An
`InMemoryRoomStore` caps the registry at 10,000 rooms as a defense-in-depth
backstop, and a background reaper reclaims idle rooms."* Two testable claims
in one sentence: an exact cap, and a live drain mechanism.

## Method

A small threaded Python driver (`http.client` + `ThreadPoolExecutor`-style
manual threads, no extra deps available in this sandbox) fired concurrent
`POST /api/media/rooms` requests and tallied status codes:

1. Burst of 500 → all `200`.
2. Burst of 9,700 more → `9,499×200`, `201×503`, first `503` at request index
   7,505 of that burst — but interspersed with later `200`s, not a clean
   cutoff. **Not** reaper interleaving (see Findings for why the first draft
   of this report was wrong about that): under concurrent dispatch, a
   request's index in the submission list is not its arrival order at the
   store's lock, so a nominally-later request can claim one of the last free
   slots before a nominally-earlier one arrives — that alone produces an
   interleaved boundary near the cap with no reaping involved.
3. Single manual request → `200` (room #10,000 confirmed by exact count, see
   below).
4. Next manual request, fired immediately after step 3 → `503`, HTML body
   confirms `room registry is at capacity (10,000 rooms); try again later`.
   This is the control that rules out reaping during steps 1-4: if even one
   of the 10,000 rooms already created had been reaped by this point, this
   request would have succeeded instead.
5. Idle period: 15s of **zero** traffic (> reap interval + idle TTL, so any
   room from steps 1-4 that was never joined ages out).
6. Fresh burst of 10,500 → **exactly** `10,000×200` then `500×503`, first
   `503` at request index 5,022 of *that* burst. This is consistent with the
   registry having drained to ~0 during the idle period, but — a second
   Codex catch on this PR, on the rerun below — is not, by itself, proof:
   the reaper keeps ticking every 5s throughout a multi-second burst too, so
   the *same* aggregate 10,000/500 split could in principle arise from a
   registry that still held leftover rooms when the burst started, took some
   early `503`s, and only opened back up once a mid-burst reaper tick
   reaped them. The aggregate counts don't preserve enough ordering
   information to rule that out on their own.
7. Unambiguous drain check for individual rooms, run as a rerun after the
   first Codex review on this PR flagged the original version of this check
   (`GET .../{id}`) as inconclusive (see Findings): created 3 fresh,
   never-joined rooms, waited 15s (idle period again), then `POST`ed
   `.../join` (not `GET .../{id}`) on each. `join_room`
   (`rooms.rs:1099-1107`) fails with `RoomNotFound` purely on registry
   membership — unlike the roster route, it needs no bearer token, so a
   `404` here cannot be explained away as "room exists but I'm not a
   member." All 3 came back `404`. A control room created and joined
   immediately afterward, with no wait, succeeded (`200` + a session
   token), confirming `join` behaves normally against a room that does
   exist and the `404`s above are specifically about the room being gone.
8. Closing the gap the second Codex catch (step 6) identified — direct,
   pre-burst confirmation that the registry is actually empty, not just
   consistent with being empty: a **third** rerun, combining steps 6 and 7
   into one ordered experiment. Created 1,000 rooms, saved every id, waited
   15s, then `join`-probed **5** of them (first, 25th/50th/75th-percentile,
   last of the batch) — all 5 came back `404`, confirmed **before** sending
   a single new create. Only then was the 10,500-request burst fired, and
   it reproduced the same split exactly: `10,000×200` then `500×503`.
9. A third Codex catch on this PR, on step 8: probing only 5 of the 1,000
   saved ids doesn't establish the other 995 were gone too, so step 8's
   "registry's emptiness was checked" conclusion was still broader than the
   evidence — a handful of surviving rooms among the unprobed 995 could in
   principle still be reaped mid-burst and produce the identical
   10,000/500 split (the same class of gap as step 6, just narrowed from
   "all rooms" to "995 of them"). A **fourth** rerun closes it exhaustively:
   created 1,000 rooms, waited 15s, then `join`-probed **all 1,000** saved
   ids (not a sample) — **all 1,000** came back `404`, zero survivors,
   confirmed before a single new create was sent. The following burst
   again produced exactly `10,000×200` then `500×503`. This is the version
   of the result with no remaining sampling gap: every room that existed
   before the burst was individually confirmed gone, not inferred, not
   estimated from a subset.

## Findings

**No bug.** Both halves of the documented claim held under live HTTP drive:

- The cap is enforced at **exactly** 10,000 rooms, four times independently
  now (step 3/4's manual boundary check, and steps 6, 8, and 9's bursts) —
  no off-by-one in either direction, and `RegistryFull` maps to `503` as
  `RoomError::into_autumn` documents.
- The reaper **fully** drains an idle registry, not just partially — and, as
  of step 9, this is exhaustively verified rather than sampled or inferred:
  **all 1,000** rooms in a batch confirmed reaped (`join` →
  `404 RoomNotFound`, zero survivors) *before* a single new create was
  sent, and the following burst still produced exactly 10,000 successes.
  Steps 7 and 8 corroborate the same mechanism at smaller (3-room) and
  sampled (5-of-1,000) scale.

Four corrections, all from Codex reviews on this PR, across three rounds:

- The first draft attributed step 2's interleaved `200`/`503` boundary to the
  reaper racing the creation burst. It doesn't hold up against step 3/4's own
  result: if any of the first 10,000 rooms had already been reaped by the
  time step 4 fired, that request would have succeeded instead of `503`ing
  immediately. The real explanation is request-concurrency ordering (see
  step 2's note above) — no reaping happened during steps 1-4 at all, only
  starting once real idle time (step 5) elapsed.
- The first draft's step 7 spot-check used `GET /api/media/rooms/{id}` (the
  member-gated roster) against a room this session never joined. That route
  fails closed with the *same* `404` whether the room is gone **or** it
  exists but the caller holds no valid membership token — so that check
  could not actually distinguish "reaped" from "exists, but I'm not a
  member," and the original conclusion was unsupported by that evidence.
  Reran it properly (current step 7) against `join`, which is not
  member-gated, giving an unambiguous result.
- On a second review round, after the two fixes above were already pushed,
  Codex correctly pointed out that step 6's aggregate
  `10,000×200`/`500×503` count is also consistent with the burst starting on
  a *non-empty* registry that got reaped mid-burst — the reaper's 5s tick
  doesn't pause just because a burst is in flight, and the aggregate counts
  don't carry timing information. Step 8 closes this by probing the
  registry directly before the burst starts, rather than inferring its
  state from the burst's own output.
- On a third review round, Codex pointed out that step 8's own fix was
  incomplete: probing only 5 of the 1,000 saved ids leaves the other 995
  unverified, so a handful of survivors among them could in principle still
  be reaped mid-burst and produce the same aggregate split — a narrower
  version of the exact gap step 8 was meant to close. Step 9 probes all
  1,000, not a sample, removing the gap entirely rather than shrinking it.

**Solid area** (toured, held up): `InMemoryRoomStore`'s 10,000-room registry
cap and the idle-reaper's live drain-back-down, now confirmed at the real
documented scale over HTTP (previously only unit-tested at a small
configured cap, e.g. `create_room_rejects_once_registry_is_at_capacity`, and
confirmed live only for participant/room reaping at small scale in
2026-09-13's session).

## Correction to a prior proposed charter

2026-09-13's session proposed, as charter 2, "Cross-namespace isolation,
live — configure two `room_namespace` values ... and drive an explicit
cross-namespace token-reuse attempt over HTTP." Reading `MediaConfig` and
`MediaPlugin` before attempting this: `room_namespace` is a single
process-wide value fixed at plugin construction
(`MediaPlugin::room_namespace()` / `[media] room_namespace`), not a
per-request or per-tenant value threaded through `RoomService` calls. Two
*processes* configured with different namespaces have separate in-memory
stores by construction — a token from one literally cannot reach the other's
store, which would prove nothing about the namespace-keying mechanism itself
(any two independent processes are isolated, namespace or not). The
`(namespace, room_id)` HashMap keying that actually matters is only
meaningfully exercisable as a **live cross-namespace test within one
process** — which would require either extending the example to expose two
namespaces from one `RoomService`/store (it currently doesn't), or testing
`InMemoryRoomStore` directly below the HTTP layer (already covered by
`reap_never_crosses_namespaces` and friends in `rooms.rs`'s own unit tests).
Narrowing rather than dropping: this charter is better scoped as "add a
second `RoomService`, from a different `room_namespace`, **sharing the same
underlying `Arc<InMemoryRoomStore>`** as the first — two separate store
instances would reproduce this session's own mistake, since two isolated
stores prove nothing about `(namespace, room_id)` keying regardless of how
many `RoomService`s wrap them — and prove no HTTP-reachable path lets a
namespace-A room id collide with namespace-B's" (also a Codex correction on
this PR). Worth doing only if `media-room` (or another example) is extended
to host multiple tenants in one process; as shipped, there's no such surface
to drive.

## Proposed next charters

1. **`DbRoomStore` parity** (carried over from 2026-09-13, still untouched;
   needs Postgres or SQLite — Postgres needs Docker, unavailable in this
   session's sandbox; SQLite doesn't per `rooms_db.rs`'s own module doc, so
   this is worth a retry in an environment with Docker or a direct SQLite
   file, specifically targeting the documented cross-process "last-write-wins"
   reaper-convergence claim).
2. **Registry-full during active (non-empty) rooms** — this session's cap
   test used only created-but-never-joined rooms (cheap to spin up 10k of).
   Untested: cap behavior when the registry is full of rooms with live
   participants (heartbeating, well under idle TTL) — does `RegistryFull`
   still correctly block new *creates* while leaving existing `join`s on
   already-created rooms unaffected? The code path suggests yes (the cap
   only gates `create`), but this session didn't drive it live.
3. **The broadcast half of `autumn-media-plugin`** — still untouched (needs
   MediaMTX/FFmpeg, not available here either).

## Reproduce

```bash
cargo build -p media-room --bin media-room
AUTUMN_SERVER__PORT=3100 \
AUTUMN_MEDIA__ROOM_REAPER_INTERVAL_SECONDS=5 \
AUTUMN_MEDIA__ROOM_IDLE_TTL_SECONDS=8 \
  ./target/debug/media-room &
# burst-create rooms via POST /api/media/rooms (any concurrent HTTP client);
# confirm the 10,001st create in a tight window returns 503. Then, to confirm
# drain-back-down without relying on aggregate counts alone: after a quiet
# period >= idle_ttl + reaper_interval, POST a saved room id's .../join first
# (expect 404 RoomNotFound) to verify the registry is empty *before* sending
# any new creates — only then fire a fresh burst and confirm it again reaches
# exactly 10,000 successes.
```
