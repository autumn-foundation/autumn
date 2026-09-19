### Fixed

- **collab-notes example:** typing ahead of the server echo no longer drops
  keystrokes (issue #2843). The editor used to go `readOnly` while an edit
  was in flight; a keystroke in that window never reached the textarea, so it
  vanished silently. The textarea now stays editable: keystrokes typed during
  the round trip are sent when the echo lands, remote operations that arrive
  in the meantime are folded in after the queue settles, and nothing is lost
  or redrawn away.
