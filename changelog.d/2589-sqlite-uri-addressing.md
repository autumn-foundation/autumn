### Fixed

- **db:** `autumn db backup` opens a SQLite source with `mode=rw` and its own URI
  options (such as `vfs`), so it never creates an empty database and its locks
  match the app's (issue #2589). Non-UTF-8 sources and `--dir` paths, and a
  `--dir` that starts with `file:`, are used exactly.
- **db:** the SQLite URL resolver reads options as SQLite does: it decodes them,
  the last `mode` and `vfs` win, and `vfs=memdb` is in memory. On Windows,
  `file:///C:/…` names a drive path (issue #2589).
- **db:** `autumn db restore` stops before the rename when the old owner could not
  read and write the restored file (issue #2589).
- **deploy:** `autumn deploy plan` shows the SQLite data-file steps. When
  `app_dir` is a symlink, `deploy up` now records `sqlite-data-adopted` on the
  first deploy, and creates `shared/data` on a fresh host (issue #2589).
