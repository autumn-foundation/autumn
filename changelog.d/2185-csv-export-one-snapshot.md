### Added

- **repository:** `list_rows(query, limit)` and, with `owner =`,
  `list_scoped_rows(owner_id, query, limit)` (#2185). They load up to `limit`
  rows in `list`/`list_scoped` order, with the same allowlist and the same
  soft-delete, tenant and owner scope. They use one statement and no
  `COUNT(*)`. One statement reads one snapshot, so a concurrent write cannot
  duplicate or skip a row. See the
  [pagination guide](docs/guide/pagination.md#without-a-count-list_rows).

### Fixed

- **generate scaffold:** the CSV export reads its rows with one `list_rows`
  (or `list_scoped_rows`) call, not one `list` page for each 100 rows
  (#2185). The file now comes from one snapshot, and a full export is one
  query, not ~200. This withdraws two caveats of the #1315 entry in 0.7.0:
  "consistency is per batch" and "~100 page queries plus ~100 `COUNT(*)`s".
  The per-IP throttle stays at 6 per minute, because an export still loads
  up to 10 000 rows and builds the file in memory. A scaffold generated
  before this change keeps the old loop. To update it, replace the loop with
  one read, then delete the `CONSISTENCY` and `COST` doc paragraphs:
  - an owner-scoped scaffold (the default):
    `repo.list_scoped_rows(owner_id, &list_query, MAX_EXPORT_ROWS + 1)`.
    Do not use `list_rows` here. It reads the rows of all users.
  - a scaffold with no owner column:
    `repo.list_rows(&list_query, MAX_EXPORT_ROWS + 1)`.
