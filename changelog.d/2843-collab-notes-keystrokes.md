### Fixed

- **🪝 Snag: `collab-notes` no longer drops a keystroke typed during an
  outstanding round trip (issue #2843):** `static/collab.js` set
  `editor.readOnly = !settled()` for the whole round trip between sending a
  character and receiving its echo. A read-only `<textarea>` still fires
  `keydown`, but the browser suppresses the value change and the `input`
  event, so the app was never told the character arrived — it was gone from
  both editors' views, with no error and the status line still reading
  "connected". The report reproduced it 4 times in 10 at an ordinary ~150ms
  typing cadence, which makes the README's own claim ("every character both
  people type survives") false.
  The lock is gone. The client now splices every keystroke into its replica
  as a placeholder the moment it is typed, so a redraw can never overwrite
  it, and queues the *message* rather than the character. Unsent
  placeholders form runs; when the edit before them is echoed nothing is in
  flight, so the character to the left of a run is one the server knows —
  the anchor an insert needs — and the run goes out whole, which is what
  makes the server chain its ids and keep the typed order. (Two inserts
  sharing one anchor are ordered by descending id, so sending them
  separately would land `ab` as `ba`. That reversal, not the missing id, is
  what the read-only lock was really protecting against; the placeholder
  mechanism the client already had covers the redraw hazard the code comment
  named.) A backspace over a character still in flight is marked and
  deleted once the echo names it, and typing over a selection still goes out
  as one atomic `replace`.
  Framework-side this is one rustdoc correction, on
  `CollabServerMessage::Snapshot::actor`, which described the old
  wait-before-diffing strategy. Two `#[ignore]`d Chromium tests in
  `examples/collab-notes/tests/system/smoke.rs` cover it, both typing in a
  single JavaScript tick — which turns the report's 4-in-10 race into a
  deterministic failure — and both asserting a second session sees the same
  text, so the characters are proved to reach the server rather than only
  the first textarea.
