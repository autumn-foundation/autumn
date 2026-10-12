### Fixed

- **upgrade:** a successor that dies right after an in-place upgrade no longer
  loses writes it acknowledged (issue #2350). It journals each write to the
  handoff directory. The predecessor resumes from that journal.
