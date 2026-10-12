### Fixed

- **upgrade:** a successor that dies just after an in-place upgrade no longer
  loses the writes it acknowledged (issue #2350). The successor journals each
  write to the handoff directory. The predecessor resumes from the journal. This
  holds only when the state version is unchanged.
