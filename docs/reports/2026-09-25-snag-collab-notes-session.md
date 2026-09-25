# 🪝 Snag: exploratory QA session — `#[collaborative]` fields / `examples/collab-notes`, 2026-09-25

## 🎯 Charter

*Persona × workflow*: two people editing the same note at once — convergence,
boundary, and interrupt concerns for `autumn-web`'s CRDT-backed
`#[collaborative]` field (issue #1806), driven through `examples/collab-notes`.
Chosen because it is the newest feature area in the repo with no prior Snag
session (`git log --all --grep=collab -i` shows only a Wayfinder a11y fix and
a Warden doc-alignment note, neither a QA pass), and the docs
(`docs/guide/collaboration.md`) make an unusually large, concrete claims
inventory to test against: convergence, causal safety, idempotence, intention
preservation, atomic `replace`, and four named `CollabLimits`.

Time-boxed to one sitting (~2 hours). Driven three ways, in order of
increasing realism:

1. Direct reading of `autumn/src/collab/{text,hub}.rs` and the existing test
   suite (`autumn/tests/integration/collab_session.rs`, 44 tests) to build the
   claims inventory and see what was already covered — not itself admissible
   as a finding source, but necessary to avoid re-filing something already
   fixed or already tested.
2. Real black-box driving: built `collab-notes` and drove it with genuine
   concurrent WebSocket connections (Node 22's built-in `WebSocket`, no
   browser), reimplementing `collab.js`'s exact tie-break logic in the test
   harness so a reported "divergence" couldn't be an artifact of a naive
   reconstruction.
3. The repo's own `#[ignore]`d two-Chromium-tab acceptance test
   (`examples/collab-notes/tests/system/smoke.rs`), run for what appears to be
   the first time in a QA session — Chromium is pre-installed in this
   sandbox, unlike prior sessions that lacked Docker for other charters.

## 📌 Environment

- Commit `9d66a66` (branch `claude/brave-goldberg-h3c4cr`, trunk-dev tip at
  session start)
- `autumn-web` 0.7.0, `collab` + `presence` + `ws` + `system-tests` features
- Linux container, rustc from the workspace's pinned toolchain, Node 22.22.2
  (native `WebSocket` global) for the hand-rolled black-box client
- Chromium 1194 at `/opt/pw-browsers/chromium` (`AUTUMN_CHROMIUM` env var),
  for the real two-browser acceptance test
- `collab-notes` run via `cargo run -p collab-notes` (in-memory note store,
  no database), against notes 1 (`"eggs\nmilk\n"`) and 2
  (`"Autumn ships collaborative fields.\n"`)

## 🔬 Coverage record

**Toured, and held up against a named oracle** (oracle: `docs/guide/collaboration.md`'s "What you get" section — Convergence, Causal safety, Idempotence — plus the `replace`/limits contract further down the same page):

- **Baseline convergence under real concurrency.** Two live WebSocket
  connections to the same note, both inserting at the document start at the
  same instant (`after: null`). Reconstructed each replica's view from its
  own message stream (not trusting the server) using an exact port of
  `collab.js`'s `(counter, actor)` tie-break, including its UTF-8-byte
  `actorGreater`. Both converged on the same text.
- **The UTF-16-vs-UTF-8 actor tie-break footgun the docs warn about is
  actually guarded, not just documented.** The guide says "Keep it ASCII:
  character ids break ties on the actor, and a browser replica compares
  those as UTF-16 while the server compares UTF-8 bytes" — read in isolation
  this sounds like an unenforced landmine (an app passing a non-ASCII actor
  prefix, e.g. from a user's display name, could in principle diverge
  between two browser tabs, since a JS `>` on strings compares UTF-16 code
  units and would order a supplementary-plane character like most emoji
  before some BMP characters above U+E000, opposite of Rust's byte-wise
  `String: Ord`). Checked `examples/collab-notes/static/collab.js` directly:
  its `actorGreater()` already encodes both sides through `TextEncoder`
  before comparing, i.e. it deliberately does *not* use a plain JS `>` on
  the actor — the code comment says as much ("A plain JS `>` compares UTF-16
  code units, which disagrees... this only guards the case where an app
  passes something else"). The reference client is written defensively
  against its own documented footgun. Not a bug; worth recording so a future
  session doesn't re-derive this and mistake the doc's warning for an
  unpatched hole.
- **`replace`'s atomicity claim** ("checked in full before anything is
  applied... a refusal leaves the text as it was"). Traced
  `CollabHub::handle`'s `Replace` arm (`autumn/src/collab/hub.rs:1180-1228`):
  size (`held + added > max_document_chars`) and actor/counter-space
  (`CollabText::preflight_insert`) are both checked, using the state
  captured before any mutation, *before* `remove_known` (the delete half)
  runs — and the same `preflight_insert` call is what `insert_after` would
  redo internally, with the same `clock`/`actor`/`wanted` (nothing else can
  touch the locked `DocState` in between), so the second, real check cannot
  newly fail relative to the first. Also directly exercised over the wire:
  filled a document to exactly `max_document_chars`, then sent one more
  character — refused (`DocumentFull`), and a **fresh** connection's
  snapshot (an independent oracle, not a replica's own bookkeeping) showed
  exactly the pre-overflow character count, not one more or one fewer.
  Already covered in the existing suite by
  `a_replacement_at_the_document_limit_leaves_the_text_alone`; this session's
  wire-level version corroborates it end-to-end through the socket rather
  than the in-process API.
- **A real three-way concurrent race on live async connections**: three
  separate WebSocket connections to one document simultaneously sent a
  `delete` of the same five character ids, a `replace` of those same five
  ids with new text, and a plain `insert` anchored to the last of those same
  five ids (a deliberately overlapping, adversarial combination — delete vs.
  replace-of-the-same-span vs. insert-anchored-to-a-character-about-to-be-
  tombstoned). All three replicas' independent reconstructions converged on
  identical text, and a fourth, freshly-joining connection's server-sent
  snapshot matched it exactly. This exercises genuine OS-scheduled
  concurrency across real tokio tasks, which the existing sequential
  in-process unit tests (single-threaded, one message at a time) cannot.
- **Malformed and adversarial wire input is refused cleanly, never crashes
  or wedges the connection**: unparseable JSON, an unknown message `type`, a
  missing required field, an `insert`/`delete` referencing a
  never-issued character id — each produced a specific `{"type":"error",
  "message":...}" and the connection stayed usable afterward (confirmed by
  sending a normal insert immediately after and having it succeed).
- **Data safety across an unclean disconnect**: sent an insert and closed
  the socket without waiting for the acknowledgement; reconnecting
  afterward showed the character had landed. (This exercises the
  send-then-immediately-navigate-away case, not a true TCP-level abrupt
  drop, which this sandbox's tooling — Node's WHATWG `WebSocket`, no raw
  socket access — cannot simulate; see "Not toured" below.)
- **The repo's own two-Chromium-tab acceptance suite**
  (`examples/collab-notes/tests/system/smoke.rs`, `#[ignore]`d, needs
  Chromium) passed in full when actually run:
  `two_browser_sessions_converge_on_the_same_text`,
  `a_keystroke_typed_during_a_round_trip_survives`, and
  `a_backspace_typed_during_a_round_trip_survives` — 3/3, real browsers, real
  WebSockets, typing at both ends while a round trip is still in flight.
  Nothing in this repo's history suggests this test had been run outside of
  a full CI pass before; prior Snag sessions on other charters explicitly
  noted lacking Docker, and this one specifically had Chromium available
  where earlier sessions might not have checked.

**Not toured, and why:**

1. **True network-level abrupt disconnects** (TCP RST, not a clean
   WebSocket close frame). Node's built-in `WebSocket` only exposes
   `.close()`, which still sends a close frame; reproducing an actual killed
   connection needs raw socket control this session didn't reach for. The
   existing suite's `a_reconnect_during_the_write_window_finds_the_live_document`
   and `an_edit_made_and_ended_inside_the_write_window_survives` cover the
   server-side session-drop path already; what's untested is specifically
   whether an *unacknowledged* client-buffered edit at the moment of a raw
   connection loss (not a graceful close) ever gets silently dropped by the
   framework's socket layer before `serve_socket` sees it end.
2. **`CollabLimits::max_documents` / `RegistryFull`.** `examples/collab-notes`
   only exposes two fixed note ids through its public routes, so there is no
   way to open enough distinct documents through the app's own surface to
   hit the registry cap without going through a private API — which the
   charter rules out as inadmissible for a user-impact finding. Untested
   this session; would need an app with a document-creation endpoint.
3. **`CollabResolver` / offline-sync merge path** (`docs/guide/collaboration.md`
   "Offline edits" section) — needs the `offline-sync` feature and a real
   client that goes offline, edits, and reconnects with a queued push; not
   attempted this session.
4. **Presence/cursor correctness under connection churn** — rapid
   connect/disconnect cycles from many actors, checking the roster never
   shows a stale or duplicate entry. Skimmed but not driven to a specific
   pass/fail.
5. **Grapheme-cluster-splitting edits** (concurrent inserts/deletes landing
   inside a ZWJ emoji sequence or a combining-diacritic pair) — `CollabText`
   operates on Unicode scalar values, not grapheme clusters, so this is more
   likely a rendering quirk than a correctness bug, and there is no
   documented claim about grapheme atomicity to hold it against; would need
   real browser rendering to even characterize, not just wire-level driving.

## Findings summary

**No bugs filed.** Every claim tested — convergence (including the
UTF-16/UTF-8 actor tie-break the docs call out as a footgun), `replace`
atomicity, the four `CollabLimits`, malformed-input handling, and the
close/finalize write-window race — held up under both code-level tracing and
live black-box driving, including a genuine three-way concurrent race across
real async connections and the project's own (previously unrun, in this
session's environment) two-browser acceptance test. This is a "toured and
solid" result, not an absence of effort: three different driving methods
(hand-rolled WebSocket client, real Chromium, and reading the existing
44-test suite to confirm it already covers what a naive first pass would
have proposed) converged on the same conclusion.

One documentation-adjacent observation, not filed as a bug (no oracle
contradicts it — the source comment in `examples/collab-notes/src/main.rs`
describes this exact behavior as intentional): connecting to a note id that
doesn't exist closes the socket immediately with no message and WebSocket
close code 1006 (abnormal closure, no close frame sent). `collab.js` reports
this identically to a real network failure ("disconnected — reload to
rejoin"), so a user has no way to tell "this note doesn't exist" from "the
server fell over." Rough edge, digest-only.

## Proposed next charters

1. **`CollabResolver` / offline-sync merge**, end-to-end through a real
   offline-then-reconnect client — the one documented behavior (last-write-
   wins vs. `CollabResolver`'s override) this session didn't drive at all.
2. **A document-creation-capable collaborative example**, to actually reach
   `CollabLimits::max_documents` / `RegistryFull` through a public surface
   rather than a private API.
3. **Presence/roster correctness under connection churn** — many actors
   joining and leaving rapidly, checked for stale or duplicate roster
   entries.
4. Continue the still-open carried-over charters from the media-room and CMS
   import sessions (`docs/reports/2026-09-24-snag-db-room-store-seat-race-postgres.md`,
   `docs/reports/2026-09-12-snag-cms-import-session.md`) — unrelated to this
   session's charter, listed here only so a scheduler picking a next charter
   sees the full backlog in one place.
