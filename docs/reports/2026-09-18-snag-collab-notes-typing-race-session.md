# 🪝 Snag: exploratory QA session — `examples/collab-notes` (issue #1806)

## 🎯 Charter

A user editing a note through `examples/collab-notes` — the shipped, only
runnable demo of `#[collaborative]`/`CollabHub` — driven with real
keystrokes and real WebSocket traffic rather than read from the source.
`#[collaborative]` merged same-day as several other large features
(`b88f78b`), has the freshest churn on trunk of anything not yet toured by
Snag, and ships with an unusually thorough test pyramid already: a 720-way
exhaustive interleaving proof (`sim_collab_convergence`), hub-level and real
two-socket convergence tests (`autumn/tests/integration/collab_session.rs`),
and a two-Chromium-page smoke test. The charter here was specifically to
drive what that pyramid does *not* cover: real keystroke-by-keystroke
typing (the existing Chromium smoke dispatches one whole-string `input`
event per edit, not per character) and the data/naughty-string tour
(unicode, RTL, emoji ZWJ sequences, combining marks) through the actual
compiled example rather than through the CRDT unit tests.

Time spent: ~2.5 hours (claims inventory + source read of
`autumn/src/collab/{hub,text}.rs` and `collab.js`, ~1h; live driving with
Playwright against the real `collab-notes` binary, ~1h; isolating and
root-causing the one finding, ~30m).

## 📌 Environment

- Commit `2aa0e942e284e94c92d47e42bcd883e2949b52fe` (`trunk-dev`), workspace
  version 0.7.0, Linux container, rustc/cargo `1.94.1`.
- `examples/collab-notes` built with `cargo build -p collab-notes` and run
  as the real binary (in-memory note store, no DB, no container).
- Driven with Chromium 141.0.7390.37 via Playwright 1.56.1
  (`/opt/pw-browsers`), headless, `--no-sandbox`, against
  `http://localhost:3000`.

## Method

1. Read `docs/guide/collaboration.md`, `autumn/src/collab/hub.rs` and
   `autumn/src/collab/text.rs` end to end to build a claims inventory:
   convergence, causal safety, idempotence, intention preservation, the
   `Replace` atomicity guarantee, the documented `MAX_ACTOR_LEN` /
   `MAX_WIRE_ELEMENTS` / `MAX_WIRE_PENDING` bounds and their errors
   (`RegistryFull`, `DocumentFull`, `CausalBufferFull`, `UnknownCharacter`),
   and the documented ASCII-actor / UTF-16-vs-UTF-8 ordering caveat.
2. Cross-checked that inventory against
   `autumn/tests/integration/collab_session.rs` and `text.rs`'s own unit
   tests to find what was *not* already exercised, rather than re-deriving
   properties the 720-way convergence proof already pins. Every hub-level
   error path (`RegistryFull`, `UnknownCharacter`, `CausalBufferFull`,
   oversized insert/delete, the close/finalize reconnect race) already has a
   direct test. The one clear gap: `serve_socket`'s `Lagged` /
   `MAX_LAG_RESENDS` backpressure path has no test anywhere. Attempted to
   trigger it deterministically; concluded it depends on OS TCP send-buffer
   sizing (a permanently non-draining client blocks the server's
   `socket.send()`, which is what would need to happen before the broadcast
   channel itself overflows) and isn't reproducible in a fast,
   environment-independent way without either a much larger flood or a
   purpose-built slow-consumer hook. Parking this as a proposed charter
   rather than forcing a flaky repro.
3. Drove two real Chromium pages against the same note (concurrent replace
   over an overlapping selection, reconnect-mid-session via page reload) —
   both converged correctly, no lost text, no console errors.
4. Ran the data tour — Hebrew, Devanagari, emoji ZWJ sequences, combining
   marks, zero-width space — typed via real keystrokes
   (`page.keyboard.type(..., { delay: N })`, not one dispatched event).
   First pass (a long mixed-script string typed fast) showed missing
   characters against the input; the rest of the session is isolating that.

## Findings

**Filed** (issue
[#2843](https://github.com/autumn-foundation/autumn/issues/2843)):
`examples/collab-notes`'s client (`static/collab.js`) silently drops a
keystroke typed while the editor is mid-round-trip on a previous character.
`updateWritability()` sets `editor.readOnly = !settled()` for the entire
gap between sending a character and receiving its echo; a browser gives no
event at all for a keystroke typed into a read-only `<textarea>`, so the
character vanishes with no error, no console warning, and no change to the
`status` line — and since it was never sent, neither editor's copy of the
note ever has it. Isolated with a `keydown`/`readOnly` correlation check
(100% correlation between `readOnly === true` at keydown time and the
character being lost) and a control run against a bare `<textarea>` with
none of the app's JS attached (identical keystrokes, identical timing, 5/5
clean) — ruling out a Playwright/Chromium key-synthesis artifact and
pinning the cause to this app's own round-trip lock. Reproduces 4/10 with
Hebrew text at a 150ms/keystroke cadence in this environment (RTL text's
heavier reflow cost widens the race window; the mechanism itself is generic
to any character whose round trip outlasts the next keystroke, so real
network latency would reproduce it with plain ASCII too — confirmed 10/10
clean for plain ASCII at the same cadence over loopback, where round trips
are fast enough to reliably win the race).

This directly falsifies `examples/collab-notes/README.md`'s own claim
("Every character both people type survives"), and is confined to the
example's own client, not the `CollabHub`/`CollabText` framework primitives
— the WebSocket protocol and CRDT never see the dropped keystroke because
the browser blocks it before any JS runs. Filed as a report, not a fix PR:
the code comment above `updateWritability()` already explains that the
lock exists to avoid a different hazard (a redraw overwriting un-tracked
keystrokes), so removing it isn't a safe small patch — a real fix needs
either the "client-side CRDT" the same comment says is out of scope for
this example, or a properly designed input queue. Both are design
decisions.

**No further bugs filed.** Everything else held:

- Two real Chromium pages typing over an overlapping selection at the same
  time converged correctly (both insertions survived — the CRDT
  intention-preservation claim, not last-write-wins) with no console
  errors.
- A page reload mid-session (simulating a phone waking back up) rejoined
  the live document and both editors' edits (before and after the reload)
  survived and converged.
- Emoji ZWJ sequences, Devanagari conjuncts, and combining marks typed in
  isolation (no interleaved round-trip race) all round-tripped byte-for-byte
  through the real socket, matching the CRDT unit tests' claims.
- The hub's documented bounds (`RegistryFull`, `UnknownCharacter`,
  `CausalBufferFull`, oversized insert/delete, close/finalize under a
  reconnect race) already have direct tests in
  `autumn/tests/integration/collab_session.rs`; reading them against the
  claims inventory found no gap worth re-driving.

**Solid area:** the `CollabHub`/`CollabText` core (the actual framework
feature, as opposed to the demo client) — protocol-level convergence,
causal buffering, and the documented limits all behave as claimed under
real two-socket driving, consistent with the exhaustive interleaving proof
already in the tree.

## Proposed next charters

1. **`serve_socket`'s `Lagged` / `MAX_LAG_RESENDS` path, with a purpose-built
   slow-consumer hook.** Untested anywhere today. Reliably triggering it
   needs either a way to pause a receiving socket's `updates.recv()` polling
   independent of OS TCP buffer sizing, or accepting a flood large enough to
   be deterministic regardless of buffer size — worth a small test-only seam
   (e.g. an injectable delay) rather than a flaky repro.
2. **A production-shaped consumer of `#[collaborative]` with a real
   client-side CRDT**, per the `updateWritability()` comment's own
   acknowledgment that the wait-and-lock strategy is example-only. Nobody
   has driven the "real" intended architecture yet; `examples/collab-notes`
   is the only consumer that exists.
3. **Presence TTL expiry under a genuinely dead connection** (process killed,
   not a clean socket close) — the 30s presence lease and its background
   sweep are documented but this session didn't drive an actual expiry, only
   clean join/leave.
