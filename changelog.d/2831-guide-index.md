### Added

- **A guide index, and a gate that keeps it complete:** `docs/guide/` had no
  index of its own, so its pages were discovered through the hand-maintained
  `## Documentation` list in `README.md` and the agent skill indexes. When
  this was measured, 92 of the guide's 147 pages — 63% of it — appeared in no
  reader-facing index at all, including `middleware.md`, `testing.md`,
  `migrations.md`, `repositories.md`, `jobs.md`, `authorization.md`,
  `pagination.md`, `websockets.md`, `rate-limiting.md`, `i18n.md`, `events.md`
  and `oauth.md`. A cold retrieval test over the README list found the
  answering page absent for 11 of 15 ordinary reader questions, while every
  drift gate was green over those same pages: the answers were correct and
  unfindable. Added `docs/guide/index.md`, which lists every guide page
  grouped by reader task and carries no answers of its own, and
  `scripts/check-docs-guide-index.sh`, which fails the build when a guide page
  is missing from the index, listed twice, linked to nothing, placed outside a
  section, or when `README.md` stops linking the index. No page moved and no
  URL changed. `README.md`'s curated highlights list is unchanged apart from a
  pointer to the index. Wired into CI's docs-only job beside the existing
  reachability gate.
