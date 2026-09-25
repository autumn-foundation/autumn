# 🪝 Snag: exploratory QA session — `#[collaborative]` fields / `examples/collab-notes`, 2026-09-25

## Corrections (from Codex review on this PR)

This report's first revision made four claims that don't hold up, all caught
by an automated Codex review on the PR and independently verified before
accepting each one. Fixed in place below rather than left standing, per the
charter's own VERIFY step:

1. **The charter premise — "no prior Snag session" — was wrong, and wrong for
   an avoidable reason.** The original dedup search
   (`git log --all --grep=collab -i`) was run against a stale local clone: it
   returned only 4 commits, missing **#2851** ("🪝 Snag: stop collab-notes
   dropping a keystroke typed during a round trip", closing issue #2843) —
   a real, substantial prior Snag session on this exact example. Running
   `git fetch origin --prune` and re-running the identical search afterward
   returns 12 commits, #2851 among them. The lesson, not just the fix: a
   dedup search is only as good as the clone it runs against, and this
   session should have fetched first. See "What #2851 already found and
   fixed" below for what that session covered and how this one relates to it.
2. **The Chromium acceptance test was not "run for the first time in a QA
   session."** `git show 90661bc8 --stat` shows #2851 is what *added*
   `examples/collab-notes/tests/system/smoke.rs`'s round-trip tests
   (`a_keystroke_typed_during_a_round_trip_survives`,
   `a_backspace_typed_during_a_round_trip_survives`) — as the regression
   coverage for the exact bug it found and fixed. That session necessarily
   ran them (a fix PR that adds tests without running them once isn't done).
   This session's run of them is a **re-confirmation that the fix still
   holds**, not a first run.
3. **The malformed-input claim overstated what a `delete` of an unknown id
   does.** The original text said an insert or delete referencing a
   never-issued id "each produced a specific error." Re-checked
   `CollabHub::handle`'s `Delete` arm (`autumn/src/collab/hub.rs:1161-1179`):
   it calls `remove_known`, which the type's own doc comment says drops an
   unknown id rather than buffering or erroring, and returns `Ok` unconditionally
   (past the size check). Re-ran the wire probe with a corrected harness (the
   original used `Array.find`, which can return a stale, already-buffered
   message instead of waiting for a new one — the same class of bug, worth
   naming so it isn't repeated): sending `{"type":"delete","ids":["999999@nonexistent-actor"]}`
   produces **no reply at all** within 1.5s — a silent, successful no-op —
   confirmed against a fresh connection's snapshot showing nothing changed.
   Only an unknown-anchor `insert` produces `UnknownCharacter`.
4. **The `replace`-atomicity wire test was mislabeled.** The original
   "exercised over the wire" claim for `replace` was actually a plain
   `insert` at the `max_document_chars` boundary — real data, but not a test
   of `replace`'s delete-then-insert atomicity. Re-ran with an actual
   `replace`: filled a document to exactly 10 000/10 000, then sent
   `{"type":"replace","ids":[<3 ids>],"after":null,"text":"REPLACED-AT-CAP"}`
   (a 3-for-16 replace, which would grow the document past the cap) — refused
   whole (`DocumentFull`), the 3 target characters confirmed still present
   and undeleted from a fresh connection. Went one step further than the
   suggested fix: also tried a **same-size** 3-for-3 replace
   (`{"ids":[<3 ids>],"text":"yyy"}`, net zero growth) at the same full
   document — **also refused**, with the identical `DocumentFull` message.
   That is not a bug; it is exactly what the source comment says: *"The
   insert is charged in full, with no credit for what the delete removes —
   a tombstone costs what a character costs. So a document at the limit
   refuses this too."* A genuinely net-neutral edit is still refused at the
   cap, which the doc's own prose already prepares the reader for
   ("`max_document_chars` does not cover this" — see
   `docs/guide/collaboration.md`'s wire-protocol section on `replace`). Worth
   stating plainly since it's easy to misread as a bug on a first pass.

The rest of the report is corrected in place (not left as strikethrough) so
it reads as one coherent record; this section exists so the correction
itself is visible, per this repo's convention for review-driven fixes on a
Snag report (see e.g. `docs/reports/2026-09-24-snag-db-room-store-seat-race-postgres.md`).

## What #2851 already found and fixed

Worth summarizing since the original charter selection missed it: #2851
found that `collab.js` set `editor.readOnly = !settled()` for the whole
round trip between sending a character and receiving its echo. A read-only
`<textarea>` still fires `keydown`, but the browser suppresses the value
change, so a keystroke typed during any in-flight round trip was silently
dropped — reproduced 4/10 at ordinary typing cadence, a real data-loss bug
matching the docs' own "no keystroke is lost" claim. The fix removed the
lock entirely and rewrote the client to splice every keystroke into its
local replica immediately as a placeholder, queuing the *message* rather
than gating the *keystroke*. This session's driving (below) is downstream of
that fix — the interrupt-tour and round-trip scenarios it exercises are
re-verifying #2851's fix holds and probing adjacent ground (boundary limits,
malformed input, a three-way concurrent race, the `replace` atomicity
contract) that #2851's own charter didn't cover, not discovering a fresh
problem area.

## 🎯 Charter

*Persona × workflow*: two people editing the same note at once — convergence,
boundary, and interrupt concerns for `autumn-web`'s CRDT-backed
`#[collaborative]` field (issue #1806), driven through `examples/collab-notes`.
Chosen believing it had no prior Snag session — **wrong, see "Corrections"
above; #2851 already covered the round-trip/keystroke-drop angle**. The
docs (`docs/guide/collaboration.md`) still make an unusually large, concrete
claims inventory worth testing on their own terms: convergence, causal
safety, idempotence, intention preservation, atomic `replace`, and four
named `CollabLimits` — most of which #2851's own round-trip-focused charter
didn't touch, which is this session's actual (narrower, but real) value.

Time-boxed to one sitting (~2 hours). Driven three ways, in order of
increasing realism:

1. Direct reading of `autumn/src/collab/{text,hub}.rs` and the existing test
   suite (`autumn/tests/integration/collab_session.rs`, 44 tests) to build the
   claims inventory and see what was already covered — not itself admissible
   as a finding source, but necessary to avoid re-filing something already
   fixed or already tested. (This step's own dedup search was run against a
   stale clone and missed #2851 — see "Corrections" above.)
2. Real black-box driving: built `collab-notes` and drove it with genuine
   concurrent WebSocket connections (Node 22's built-in `WebSocket`, no
   browser), reimplementing `collab.js`'s exact tie-break logic in the test
   harness so a reported "divergence" couldn't be an artifact of a naive
   reconstruction.
3. The repo's own `#[ignore]`d two-Chromium-tab acceptance test
   (`examples/collab-notes/tests/system/smoke.rs`) — added by #2851, re-run
   here to confirm its fix still holds, not run for the first time.

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
  newly fail relative to the first. Also directly exercised over the wire
  with an actual `replace` message (not a plain `insert` — see
  "Corrections" above for where the first pass got this wrong): filled a
  document to exactly 10 000/10 000, then sent a 3-for-16 `replace`
  (would grow the document) — refused whole (`DocumentFull`), the 3 target
  characters confirmed still present and undeleted from a **fresh**
  connection's snapshot (an independent oracle, not a replica's own
  bookkeeping). Then tried a same-size 3-for-3 `replace` (net zero growth)
  at the same full document — **also refused**, matching the source
  comment's explicit design ("a tombstone costs what a character costs... a
  document at the limit refuses this too"), not a bug. Already covered
  in-process by the existing suite's
  `a_replacement_at_the_document_limit_leaves_the_text_alone`; this
  session's version corroborates it end-to-end through the socket.
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
- **Malformed and adversarial wire input never crashes or wedges the
  connection.** Unparseable JSON, an unknown message `type`, a missing
  required field, and an `insert` referencing a never-issued anchor id each
  produced a specific `{"type":"error","message":...}`. A `delete` naming a
  never-issued id is different, and the first pass mischaracterized it (see
  "Corrections" above): it is a **silent, successful no-op**, not an error —
  `remove_known` drops an unknown id rather than erroring, by design (its own
  doc comment: buffering it "would tombstone that character the moment
  somebody typed it"). Confirmed with a corrected harness that waits for a
  genuinely new message rather than trusting whatever is already buffered:
  no reply within 1.5s, and a fresh connection's snapshot shows nothing
  changed. Every case left the connection usable afterward (confirmed by
  sending a normal insert immediately after and having it succeed).
- **Data safety across an unclean disconnect**: sent an insert and closed
  the socket without waiting for the acknowledgement; reconnecting
  afterward showed the character had landed. (This exercises the
  send-then-immediately-navigate-away case, not a true TCP-level abrupt
  drop, which this sandbox's tooling — Node's WHATWG `WebSocket`, no raw
  socket access — cannot simulate; see "Not toured" below.)
- **The repo's own two-Chromium-tab acceptance suite**
  (`examples/collab-notes/tests/system/smoke.rs`, `#[ignore]`d, needs
  Chromium) passed in full when re-run this session:
  `two_browser_sessions_converge_on_the_same_text`,
  `a_keystroke_typed_during_a_round_trip_survives`, and
  `a_backspace_typed_during_a_round_trip_survives` — 3/3, real browsers, real
  WebSockets, typing at both ends while a round trip is still in flight.
  These two round-trip tests were **added by #2851** as the regression
  coverage for the keystroke-drop bug it fixed (see "What #2851 already
  found and fixed" above) — this run confirms that fix still holds on the
  current `trunk-dev` tip, not a first-ever run.

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
atomicity (including the same-size-replace-at-cap edge case), the
`max_document_chars` boundary specifically, malformed-input handling, and
the close/finalize write-window race — held up under both code-level tracing
and live black-box driving, including a genuine three-way concurrent race
across real async connections and a re-run of the project's own two-browser
acceptance test (added by, and confirming, #2851's earlier fix). This session
drove `max_document_chars` directly at the wire level; `max_insert_chars` and
`max_delete_ids` are covered by code tracing and the existing in-process
suite rather than an independent wire-level probe this session, and
`max_documents`/`RegistryFull` was not reachable through this example's
public routes at all (see "Not toured" below) — so "the four `CollabLimits`"
is not a claim this session can make in full; scoped to what was actually
driven, it holds. This is a "toured and solid" result for what it covers,
not an absence of effort: three different driving methods (hand-rolled
WebSocket client, real Chromium, and reading the existing 44-test suite)
converged on the same conclusion, and it builds on — rather than duplicates —
#2851's own earlier, narrower charter.

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
