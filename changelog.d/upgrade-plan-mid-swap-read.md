### Fixed

- **upgrade:** `autumn upgrade` no longer reports a scaffold file as a
  conflict ("unreadable") when its planning read lands while another upgrade
  is renaming that file back into place. On Windows such a read fails instead
  of finding nothing; it now waits out the swap like a missing file does. A
  file that cannot be read, with no swap in flight, is still a conflict.
