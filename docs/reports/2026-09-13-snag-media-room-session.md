# 🪝 Snag: exploratory QA session — `examples/media-room`, 2026-09-13

## 🎯 Charter

*Persona × workflow*: an unauthenticated client drives the `autumn-media-plugin`
room-signaling lifecycle end-to-end over HTTP — create a room, join it to
capacity, leave, heartbeat, and poll the member-gated roster — the way a real
WebRTC mesh-call client (or an attacker probing an unauthenticated surface,
since the plugin's own docs say these routes ship with none) would. Concerns:
the documented capacity ceiling (`DEFAULT_ROOM_MAX_PARTICIPANTS` = 6), the
fail-closed claims on heartbeat/roster/leave, the "advisory, never enforced"
token-expiry claim, and the idle-participant reaper's actual end-to-end
behavior (not just its unit tests). Chose this example over the Postgres-backed
ones (`teams`, `saas`, `wiki`, `bookmarks`) because this session's sandbox has
no Docker daemon (`dockerd` not running, `service docker start` fails with a
`ulimit`/permission error) — `media-room` needs no database, so it was
buildable and runnable here.

Time-boxed to one sitting (~1.5 hours wall clock, most of it a genuinely slow
`cargo build -p media-room` cold-cache compile).

## 📌 Environment

- Commit: `6e71bfb` (branch `claude/brave-goldberg-7vwcn7`, `trunk-dev` tip at
  session start)
- Platform: Linux container (no Docker daemon available), rustc/cargo 1.94.1
- `media-room` run directly via `cargo build -p media-room` then the built
  binary, `AUTUMN_SERVER__PORT` overridden per run, no `[media]` TOML overrides
  except two live runs with `AUTUMN_MEDIA__ROOM_REAPER_INTERVAL_SECONDS=2` /
  `AUTUMN_MEDIA__ROOM_IDLE_TTL_SECONDS=3` to observe the reaper on a human
  timescale instead of its 60s/900s defaults
- Driven via `curl` and small `urllib`-based Python scripts (no cookies/session
  needed — the plugin's room routes are token-in-body/header, not
  cookie-based)

## 🔬 Coverage record

**Toured, and held up against a named oracle:**

- **Capacity ceiling, boundary N/N+1** (oracle: `docs/guide/media.md` — "the
  room is hard-capped at `DEFAULT_ROOM_MAX_PARTICIPANTS` (6)"). Joined a fresh
  room to exactly 6 members; the 7th (and every subsequent) join returned
  `409 Conflict` with a `RFC 7807` problem body naming `"room is full (max 6
  participants)"`. Held under real concurrency too: 5 trials of 40 concurrent
  join requests fired at a fresh room each landed exactly 6 successes / 34
  `409`s — the store's check-then-insert is correctly serialized under one
  write-lock hold, no TOCTOU over-admission.
- **Fail-closed auth on leave/heartbeat/roster** (oracle: the plugin's own
  module doc and `docs/guide/media.md`: "an unknown room, unknown participant
  and wrong token are one indistinguishable `404`" for heartbeat; roster
  "gets the same `404` as a nonexistent room"). Verified all live over HTTP:
  wrong-token `leave` → `401` (leave's own contract is `401 Unauthorized`, not
  the roster/heartbeat `404` — confirmed against the unit test
  `room_error_maps_to_expected_http_status`, so this is correct, not a
  drift); no-`Authorization`-header roster → `404`; malformed (`Bearer`-less)
  `Authorization` header roster → `404`; heartbeat with a stale (already-left)
  token → `404`; double-`leave` (same participant, same token, called twice)
  → `200` then `404` once the room empties and is dropped.
- **Advisory token expiry, live** (oracle: `docs/guide/media.md` line 178 /
  the module's "Known limitations" doc: token expiry is "advisory... never
  enforced"). Not separately re-verified this session beyond the reap test
  below — the doc's own wording already matches the code path read
  (`verify_token` is value-only, ignores `token_expires_at`), and this is the
  same claim the shipped unit tests already pin.
- **Idle-participant reaper, end-to-end** (oracle: `docs/guide/media.md`'s
  reaper section — a client that neither heartbeats nor polls the roster for
  a full idle TTL "loses its signaling record"). This is unit-tested in
  `rooms.rs` but I hadn't seen it fire through a real running process, so I
  restarted the app with `AUTUMN_MEDIA__ROOM_REAPER_INTERVAL_SECONDS=2` /
  `AUTUMN_MEDIA__ROOM_IDLE_TTL_SECONDS=3` (both are documented
  env-overridable constants, not app config): joined a room, did nothing for
  6s, then confirmed the roster (with the now-stale token) and a heartbeat
  both returned `404`, and — since that was the room's only participant — the
  room itself was gone (rejoining the same `room_id` also `404`s). Matches
  the claim exactly; no lingering "seat" survived past the TTL.
- **Data tour** — emoji, an RTL-override control character (`U+202E`), an
  embedded NUL byte, empty string, whitespace-only, a `DROP TABLE`-shaped
  string, and a literal `<script>` tag as `display_name`: every one
  round-tripped byte-for-byte through join → the room snapshot the join
  response carries, with no corruption, truncation, or injection (this is a
  JSON API with no server-side HTML templating of the value anywhere in this
  example, so there is no reflected-HTML surface to begin with). Malformed
  input handling (missing `content-type`, invalid JSON, missing required
  field, wrong JSON value type) produced the expected `415`/`400`/`422`
  Autumn defaults, not a `500` or a panic.
- **Crash tour**: no panic, no `500`, and no server exit across ~150 requests
  spanning all of the above, including the 2 MB `display_name` case below.

**Investigated and ruled out (would have been a lead, but isn't a bug):**

- **Unbounded `display_name` size.** A 2 MB JSON string for `display_name`
  joined successfully (`200`, byte-exact round-trip) — there is no length
  validation on this field anywhere in `InMemoryRoomStore::join_room`. This
  looked like a resource-exhaustion lead at first, especially next to the
  module's own comment that the 10,000-room registry cap exists to guard
  "against unbounded memory growth from a create loop." But
  `docs/guide/media.md` already carries an explicit, unambiguous warning
  directly above this surface: create/join "ship **no** built-in
  authentication or rate limiting... they **must** be mounted behind your
  application's own auth / rate-limit middleware," and names the room cap as
  "defense-in-depth," explicitly "neither substitutes for your auth layer."
  A large-`display_name` amplification is the same already-disclosed
  unauthenticated-abuse risk the docs tell an integrator to gate externally,
  just a bigger per-request multiplier — not an undisclosed hole, and the
  6-seats-per-room hard cap bounds the blast radius to `6 ×`
  request-body-limit (32 MiB default) per room regardless. Not filed; see
  digest below for the narrower, actionable slice of this.
- **Home page's "Rooms" list going stale after a reap.** The example's own
  `RoomLog` (a demo-only, append-only list backing the home page's table and
  `/api/rooms`) keeps listing a room forever, even after the plugin's real
  `RoomStore` reaps and removes it — a user clicking through would find a
  room the API now reports `404` for. Ruled out as a bug: the source comment
  on `RoomLog` is explicit that "the plugin owns the real room state; this
  log is only the demo's own 'list rooms' surface... the plugin exposes
  create/join/leave/roster, not a list-all endpoint," so nothing here claims
  live sync. It is a demo-app limitation the code discloses to its own
  reader, not a gap between a claim and the implementation.
- **The `POST /rooms` create-room form ships no CSRF/submit-token hidden
  field.** The startup log advertises "One-time submit-token protection
  enabled" for the app, which read at first like every POST should need one.
  Checked: the framework's submit-token guard is opt-in per form (via the
  `SubmitToken`/`_submit_token` mechanism documented in
  `docs/guide/submit-tokens.md`), not implicitly blanket; `media-room`'s
  create-room form never opts in, and `curl -X POST /rooms` with no token
  succeeds (`303` to `/`) exactly as the plain, non-protected form it is.
  Consistent with the same missing-`SubmitToken` pattern already triaged as
  a non-bug (no broken uniqueness invariant — room creation is intentionally
  unconstrained, every create mints a fresh UUID) in the `reddit-clone` and
  `cms` Snag sessions.

**Not reached this session** (candidates for a follow-up charter):

- The `db` room-store backend (`DbRoomStore`) — not attempted this session.
  **Correction**: an earlier draft of this bullet said this needed Postgres
  and was therefore unreachable without Docker; that's wrong.
  `rooms_db.rs`'s own module doc and `docs/guide/media.md` both say the
  backend is written portably against `RuntimeConnection`/`RuntimeBackend`
  (Postgres by default, SQLite under `autumn-web/sqlite`) with no
  Postgres-only SQL, so a SQLite file needs no Docker at all — a Codex
  review on this PR correctly caught the misstatement. The real reason this
  wasn't reached is simply that this session's time box went to the
  in-memory store instead, not a missing dependency. `docs/guide/media.md`
  claims parity with the in-memory store ("both backends enforce the
  absolute 6-seat mesh ceiling and cap the registry") and a "last-write-wins"
  cross-process reaper convergence claim that is a strictly better
  differential-oracle target than anything reachable against a single
  in-memory process.
- `room_namespace` tenant isolation — the shipped example never sets one, so
  cross-namespace leakage (a namespace-A token used against a namespace-B
  room id) was read in code (fails closed by construction, keyed into the
  map) but never driven over HTTP against two live namespaces.
- The broadcast half of `autumn-media-plugin` (`with_broadcast()`,
  `MediaWorkflows` durable encode jobs, `MediaMtxClient`) — untouched;
  needs a `MediaMTX`/`FFmpeg` presence this session never set up.
- Registry-full (`RegistryFull`, 10,000-room cap) and reaper-under-load
  behavior at realistic scale — creating 10,000 rooms to hit the backstop
  live was judged out of a ~30-CI-minute-equivalent budget for this session.

## Findings summary

- **Bugs filed:** none. Every oracle checked this session (capacity ceiling
  under load and under concurrency, fail-closed leave/heartbeat/roster, live
  idle-reaping, data-tour round-tripping) matched the implementation and the
  documentation.
- **Digest (oracle-less, not filed as bugs):**
  - `display_name` has no length cap in `InMemoryRoomStore::join_room`. Not a
    documented-claim violation (the already-disclosed "no auth/rate-limit"
    warning covers unauthenticated abuse of this surface generally), but a
    cheap, narrow hardening a maintainer may still want: a modest
    server-side cap (a few hundred bytes is plenty for a display name) would
    shrink the per-request amplification factor without needing the
    integrator to have their own middleware in place yet. Worth a fix-on-touch
    if `rooms.rs` is edited for another reason; not worth a dedicated PR on
    its own given the existing disclosure.
  - The demo's home-page room list (`RoomLog`) has no way to reflect a room
    the reaper removed — cosmetic/demo-scope only, already self-disclosed in
    the source comment.
- **Solid areas** (toured, held up, no further attention needed absent new
  changes to the surface): room capacity enforcement (including under
  concurrent load), fail-closed auth on leave/heartbeat/roster, the
  idle-participant reaper's live end-to-end behavior, and `display_name`
  data-tour round-tripping (unicode/emoji/control characters/injection
  strings).

## Proposed next charters

1. **`DbRoomStore` parity** — repeat this charter against the `db` room-store
   backend, backed by SQLite (no Docker needed — see the correction above)
   or Postgres, and specifically target the documented cross-process
   "last-write-wins" reaper-convergence claim with two app processes sharing
   one database file/instance.
2. **Cross-namespace isolation, live** — configure two `room_namespace`
   values (or two plugin instances) and drive an explicit cross-namespace
   token-reuse attempt over HTTP, rather than relying on the code-level
   `(namespace, room_id)` keying read during this session.
3. **Registry-full backstop at scale** — actually drive `RoomError::RegistryFull`
   (10,000 rooms) and confirm the reaper drains it back down, as a resource-meter
   oracle over a longer soak than this session's budget allowed.
