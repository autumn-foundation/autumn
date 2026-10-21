### Fixed

- **doctor, dev:** In a workspace, `autumn doctor` and `autumn dev` now read
  the outermost `deny.toml`, up to the `.git` folder (issue #2570). This is the
  file CI reads. A `deny.toml` in a member crate no longer replaces it. A nested
  workspace keeps its own policy. Without a `.git` folder, the nearest policy
  is used.
