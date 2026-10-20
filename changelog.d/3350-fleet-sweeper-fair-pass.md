### Fixed

- **db:** busy open databases no longer take every pass of the SQLite fleet
  sweeper (issue #3350). A pass takes rows in the order they became due. When
  two rows became due at the same time, the older row goes first. Half of
  each pass goes first to the open databases, so a fleet-wide backfill does
  not stop their outbox. A pass takes no more closed databases than the
  fleet has room for, and no database of another host. A new mark is due
  when it is written, on every clock.
