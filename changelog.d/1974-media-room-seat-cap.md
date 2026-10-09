### Fixed

- **media:** `DbRoomStore` joins from two processes no longer pass a room's
  seat cap (issues #2864, #3104). Join, leave and the reaper's room delete each
  lock the room row in one transaction. No migration is necessary.
- **media:** on `DbRoomStore`, a join that races the last leave or the reaper
  no longer returns a seat that the room delete then removes (issue #2407). It
  gets `404`, as for a room that is gone.
- **media:** a room join refuses a `display_name` longer than 64 characters
  with `400` (`RoomError::DisplayNameTooLong`), so one caller cannot store a
  very large string in a room (issue #3104).
