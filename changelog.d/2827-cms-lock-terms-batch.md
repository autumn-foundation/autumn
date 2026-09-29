### Fixed

- **🗃️ Ledger: batch `examples/cms`'s `set_post_terms` `lock_terms` loop
  (statements N→1):** `content::set_post_terms` — called from the ordinary
  editor "Update" save on `POST /admin/content/{post_type}/{id}` whenever a
  post carries any taxonomy terms, not an admin bulk tool — computes the
  union of a post's previous and newly-submitted term ids and locks them
  before recounting. `lock_terms` locked that set one id at a time: N ids, N
  round trips, each its own `SELECT "terms"."id" ... WHERE "terms"."id" = $1
  FOR UPDATE`. `recount_term`'s own first statement generates the *exact
  same* SQL to re-lock the same row before counting it — unchanged here,
  since it is `pub` and load-bearing at two other call sites with no prior
  batch lock — so the two together paid this shape `2N` times per save. Now
  `lock_terms` issues one `WHERE "terms"."id" = ANY($1) ORDER BY
  "terms"."id" ASC FOR UPDATE` query instead of looping, still locking in
  ascending id order — the deadlock-avoidance property the loop existed
  for — which `EXPLAIN` confirms holds even when a 65-id set is handed to
  the planner in descending order: Postgres satisfies the ordering from the
  `terms_pkey` index scan itself, with no separate `Sort` node needed.
  Profiled through the real update route at three tiers of a post's
  category selection changing (7/27/65 affected terms, previous and wanted
  sets partially overlapping): the single-row lock shape drops by exactly N
  per tier (14→7, 54→27, 130→65 — `recount_term`'s own unchanged N calls
  are what remains), and the whole request's statement count drops by N-1
  (79→73, 210→184, 467→403). `recount_term`'s own 3-statement-per-term
  recount is unchanged and out of scope — this fixes the `lock_terms` phase
  specifically, not the whole chain. No behavior change: the harness
  confirms the exact same `post_terms` rows and `terms.post_count` values
  result, and the existing `cargo test -p cms` suite (99 unit tests +
  existing integration suite) passes unchanged. See
  `docs/reports/2026-09-16-ledger-cms-set-post-terms-lock-batch/`.
