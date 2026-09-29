### Performance

- **⚡ Bolt: `CollabText` resolves an append anchor in O(1) instead of scanning
  the document (instructions -83.4%):** `CollabText::integrate`
  (`autumn/src/collab/text.rs`) resolved every `Insert` operation's anchor by
  scanning the whole `elems` array for the character it names
  (`slot_after` → `position_of`), so typing a document of n characters
  end to end — the shape `CollabHub`'s live-edit message loop drives, one
  keystroke per `insert_after` call — cost O(n²) overall. Sequential typing
  anchors every keystroke but the first on the character just inserted,
  which is always the document's current last element, so `slot_after` now
  checks `elems.last()` first and returns without scanning; a mid-document
  edit still falls through to the original scan, with an identical result
  either way. Measured on `autumn/benches/collab_edit.rs` (6,000 keystrokes
  typed into one document, `valgrind --tool=callgrind`): instructions
  153,218,683 → 25,462,389 (**-83.4%**), with `CollabText::integrate`
  dropping from 83.01% to 3.55% of the profile. Behavior is unchanged: all
  44 `collab::text` unit tests and all 38 `collab_session` integration
  tests pass unmodified.
