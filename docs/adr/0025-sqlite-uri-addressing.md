# ADR 0025: One URI translation for SQLite backup, restore and deploy

- Status: Accepted
- Date: 2026-10-25
- Deciders: Autumn maintainers
- Tags: sqlite, backup, restore, deploy

## Context

Issue #2589 lists follow-ups from #2577. These items are open:

| Item | Defect |
| --- | --- |
| 1 | `file:app.db?mode=mem%6fry` opens in memory. The resolver returns `app.db`. |
| 2 | `mode=memory&mode=rwc` opens `app.db`. The resolver refuses it. |
| 6 | `vfs=memdb` opens in memory. The resolver returns a path. |
| 9 | `file:///C:/x.db` resolves to `/C:/x.db`. Windows does not read that as a drive path. |
| 7 | The snapshot opens the source with create-on-missing. A race gives an empty "good" backup. |
| 3 | A non-UTF-8 source path cannot be opened. |
| 12 | The snapshot drops the `vfs` of the source URI. |
| 15 | A non-UTF-8 `--dir` is changed by a lossy conversion. |
| 18 | A `--dir` that starts with `file:` becomes a URI in `VACUUM INTO`. |
| 4 | A restore that cannot keep the owner reports success. The owner cannot open the file. |
| 14 | A restore over a `0444` database. |
| 5 | `autumn deploy plan` does not show the SQLite data steps. |
| 19 | The CLI and the host use two different rules for "inside `shared/data`". |

Items 3, 7, 9, 12, 15 and 18 are one question: how do we translate between a
SQLite URI and a file path? Items 1, 2 and 6 are the same question for the
query string.

A spike on SQLite 3.45.1 showed:

- `file:missing.db?mode=rw` fails and creates no file.
- `VACUUM INTO 'file:backups/out%3F.db'` writes `backups/out?.db`. The output
  is a URI.
- `VACUUM INTO 'file:bad%FF.db'` writes the bytes `bad\xFF.db`.
- `mode=rw` on a `0444` file opens it read-only. `integrity_check` is `ok`.
- `mo%64e=memory`, `mode=mem%6fry`, `vfs=mem%64b` and `vfs=memdb` all give
  no file. `mode=memory&mode=rwc` gives a file.

## Decision

1. **One parser.** `replication::database_target(url)` returns the path and
   the ordered query options. It decodes each option name and value. The last
   `mode` and the last `vfs` win. `mode=memory` or `vfs=memdb` is not a file.
2. **One encoder.** `replication::file_uri(path)` makes a `file:` URI from the
   path bytes. It percent-encodes each byte outside a small safe set. It writes
   `file:///C:/…` on Windows. An absolute path always gets `file://`, so `//x`
   cannot become an authority. Decoded names and values stop at a NUL, as in
   the C API.
3. **The source opens through a no-create URI.** `file_uri(path)`, then the
   source options without `mode`, then `mode=rw` (or `mode=ro` when the
   source asks for it). The `vfs` is kept.
4. **The output is a URI too.** `VACUUM INTO` gets `file_uri(out_path)`.
   Non-UTF-8 and `file:` names stay exact.
5. **Restore checks access before the rename.** When `fchown` cannot keep
   the uid, the restore tries to keep the gid. Then, if the old owner cannot
   read and write the staged file through both the group and the other bits,
   the restore stops.
6. **`deploy plan` shows the data steps.** It loads the target profile config
   to classify `[database]`. When the load fails, it says so and continues.
7. **The host is the one authority for `shared/data`.** `record-data-adopted`
   and `check-data-dir` use the same shell resolver. The resolver follows the
   longest prefix that exists, so an absent `shared/data` under a symlinked
   `app_dir` still resolves. A database is in `shared/data` when its parent is
   the resolved `shared/data` (a volume link) or `shared/data` under the
   resolved `app_dir` (an absent volume). Any other place in the app dir is
   refused first, so a `shared/data` link into `releases/` (or to an ancestor)
   admits nothing. A relative database keeps the
   lexical check, because it is in `shared/data` by construction.

## Planning

### Brainstorming

| Idea | Verdict |
| --- | --- |
| Pass the original URL to diesel with `mode=rw` added | Rejected. A `sqlite://` URL is a plain path, not a URI. Two code paths. |
| Build every URI from the decoded path | **Chosen.** One code path for all spellings. |
| Keep `connection_string` and add `?mode=rw` | Rejected. A plain path cannot carry options. |
| Use `sqlite3_open_v2` flags through FFI | Rejected. diesel hides the flags. Unsafe code. |
| Append `mode=rw` and rely on "last wins" | Partly chosen. We also remove old `mode` options, so the URI is clear. |
| Use `readlink -m` on the host | Rejected. Not in BusyBox or BSD. |
| Resolve the longest existing prefix in `sh` | **Chosen.** `readlink -f` only on a path that exists (GNU, BusyBox). |
| Make `deploy plan` read the database URL | **Chosen.** The dry run must match the real run. |

### Reverse brainstorming: how do we lose or leak data?

| Failure | Guard |
| --- | --- |
| The source moves between the check and the open. SQLite creates an empty file. | `mode=rw`. Test: open a missing path through the URI. No file appears. |
| A `%`, `?` or `#` in a name cuts the URI. | Percent-encode them. Test: round trip. |
| A path `//host/x` becomes an authority. | Absolute paths get `file://`. Test. |
| A name that starts with `file:` becomes a URI. | Every name goes through the encoder. Test with `--dir file:…`. |
| A lossy `--dir` writes to a different directory. | Encode raw bytes. Test with `\xFF` on Unix. |
| The source `vfs` is lost, so locks do not match the app. | Keep every option except `mode`. Test the URI text. |
| `mode=memory` hides behind an escape or a later `mode`. | Decode, last wins. Table test. |
| A restore leaves a file the service cannot open. | Access check before rename. Pure unit test. |
| `plan` hides the link step. | Plan test with a `sqlite://` app. |
| The host and the CLI disagree on `shared/data`. | One shell resolver for both ops. Tests run the shell on disk. |

### Six thinking hats

- **White (facts).** The spike results above. diesel opens with
  `SQLITE_OPEN_URI | READWRITE | CREATE`. `VACUUM INTO` reads a URI when
  URIs are on.
- **Red (feeling).** The opener had many patches. We do not trust more
  guards. We trust one translation that all callers use.
- **Black (risks).** Windows cannot be tested on Linux. A wrong encoder breaks
  every backup. `deploy plan` can now fail on a bad config.
- **Yellow (value).** One encoder fixes six items. The backup and the app now
  open the same file with the same VFS.
- **Green (options).** Make the Windows rule a pure function and test both
  forms on all hosts. Add a `#[cfg(windows)]` round trip for CI. Let `plan`
  continue when the config does not load.
- **Blue (process).** RED: a failing test for each item. GREEN: the smallest
  code. REFACTOR: shared helpers, short comments. Then a multi-angle review.

## Consequences

- `connection_string` stays for the replicator. The backup opener does not use
  it.
- The snapshot opens the source with `mode=rw`. A backup can no longer create a
  database.
- `autumn deploy plan` now reads the target profile `[database]` value.
- Item 14 was already safe after #2577 (the handle stays writable). This change
  adds a test so it stays safe.
