# 🪝 Snag: exploratory QA session report — `#[confidential]` fields (#1771)

**Charter:** a developer adopting `#[confidential]` (operator-blind fields,
issue #1771) the day after it merged — the freshest, most security-critical
surface on trunk at session start (merged as commit `0f1b0c0`, same day as a
backend-agnostic admin-plugin change and a SQLite migrations fork). Threat
model: the operator of the server. Oracle sources: the feature's own claims
inventory (`docs/guide/confidential-fields.md`'s "Cannot see" / "Can see" /
"Outside the guarantee" tables), its invariants (per-owner isolation of the
blind index, envelope authentication), and differential consistency with the
sibling `#[encrypted]` feature's redaction machinery.

**Time spent:** ~1.5 hours (claims-inventory read, adversarial code reading
across five crates, one driven regression probe, build/lint/verify).

**Environment:** commit `0f1b0c0610c9eda0991cdfbecece3422d23a2cc7` (trunk),
workspace version 0.7.0, Linux container, rustc/cargo `1.94.1`. No Docker
daemon available in this sandbox, so the Postgres-backed paths
(`confidential_red_team`, `confidential_threat_model`, the admin-plugin's
live HTTP routes) were read but not independently re-run; the driven probe
below uses the `sqlite` backend instead, which needs no Docker.

## Method

1. Built the claims inventory from `docs/guide/confidential-fields.md` (the
   admin sink table, the "what the build refuses" table, the envelope/key
   derivation spec) and cross-read it against `autumn/src/confidential.rs`'s
   own module docs and `OPERATOR_BLIND_SINKS`/`OPERATOR_VISIBLE` constants —
   the two are meant to agree, and `confidential_threat_model` already pins
   that agreement in CI.
2. Read the shipped test suite first, to find the seams it doesn't cover
   rather than re-deriving properties it already proves. It's unusually
   thorough for brand-new code: `confidential_sealing.rs` alone already
   drives malformed base64, truncated envelopes, a relabelled version byte,
   whitespace padding, empty/short/long plaintexts, uppercase-hex token
   rejection, and the context-injectivity property. `confidential_model.rs`,
   `confidential_red_team.rs` and the admin-plugin's own template tests cover
   the DB round trip, log/backup/capsule/version-history/admin/CSV redaction.
3. Traced every call site that folds confidential column names into the log
   parameter filter (`router.rs` ×4, `job.rs` ×1) to check none was missed —
   a single skipped site would leak envelopes/tokens into a log surface the
   docs claim is blind. All five extend with
   `confidential::registered_confidential_column_names()`.
4. Read the admin-plugin's redaction call sites (`templates.rs` list/detail/
   readonly/form-widget renderers, `routes.rs` CSV export) and the CSV
   *import* path, on the hypothesis that import — the one admin write path
   the docs don't mention — might let an operator overwrite a confidential
   column without redaction. It doesn't: `import_csv_row` has no
   `#[model]`-generated default (unlike `csv_export_columns`, which does
   filter confidential columns) — it's an app-authored override with no
   built-in behavior, so this isn't a framework gap the docs claim otherwise
   about.
5. Found the one real gap: `tests/compile-pass/confidential_blind_index_finder.rs`
   proves the `#[repository]`-generated `find_by_body_bidx` finder — the
   *only* sanctioned way to query a confidential column, per the guide's
   "Equality lookups" section and the `confidential_find_by` compile-fail
   diagnostic that points developers at it — **compiles** (`fn main() {}`,
   never executed). Nothing in the suite ever *runs* it: `confidential_model.rs`
   queries the blind-index column with a raw Diesel `.filter(...)`, not
   through the generated method. The one property that makes the whole
   design sound — a blind-index lookup for owner A never returns owner B's
   row, even when both hold the identical plaintext — was asserted only at
   the unit level (single connection, single owner, hand-rolled query), never
   through the actual generated, sanctioned entry point against real
   multi-owner data.
6. Closed that gap with a driven probe: `autumn/tests/confidential_repository_bidx.rs`,
   a real (tempfile) `SQLite` pool, a real `#[model]`/`#[repository]` pair,
   200 owners in two plaintext-sharing clusters, real `save()` inserts, and
   the actual generated `find_by_body_bidx(&self, ...)` call — not a
   hand-written filter — checked for (a) exactly one hit per owner's own
   token, (b) the right row, (c) correct plaintext recovery, and (d) that
   owner A's token, run through the real finder, never returns owner C's row
   even though A and C hold the byte-identical plaintext.

## Findings

**No bug filed.** Every property held: the log-filter fold-in is complete
across all five call sites, the admin-plugin's redaction is applied
consistently across list/detail/readonly/form-widget/CSV-export, and — the
one path this session actually drove rather than read — the generated
blind-index finder isolates owners correctly against real data, including
the adversarial same-plaintext-different-owner case. **Solid area.**

**Filed as a test-gap regression, not a bug report** (Acceptable Outcome #4):
`autumn/tests/confidential_repository_bidx.rs`, wired as its own `[[test]]`
target (`cargo test -p autumn-web --features "sqlite,test-support" --test
confidential_repository_bidx`) rather than into the consolidated
`integration_tests` binary — the `sqlite` feature flips
`db::RuntimeConnection` for the whole build graph (the same
feature-unification hazard `sqlite_jobs_scheduler_e2e.rs` documents), so it
cannot share a binary with the Postgres-assuming `db` suite. This closes a
real coverage gap (the sanctioned query path had never been executed) with a
passing, honest result — not a bug, but no longer an unverified claim either.

## Proposed next charters

1. **Admin UI over a `#[confidential]` model, live, with Docker available** —
   this session verified the redaction *functions* (`render_cell_value` /
   `render_detail_value` / `render_form_widget` / CSV export) directly with
   synthetic records, matching the existing unit-test style, but never drove
   a real `GET /admin/{slug}/{id}` response against a model registered the
   way an app actually would (`AdminField::new("body", ...)`, hand-authored
   `fields()`, as `examples/blog/src/admin.rs` shows in practice). No
   divergence was found reading the code, but "read the code and reason it's
   fine" is weaker than "drove it and watched the HTML."
2. **The documented partial-update gap, characterized rather than assumed** —
   the guide states a `PATCH` that sets `body` without `body_bidx` is
   accepted and leaves the token indexing the previous value (a known,
   named limitation, not a bug). This session confirmed the codegen shape
   (`Patch<T>` with `#[serde(default)]`, so an omitted field defaults to
   `Unchanged`) matches that description, but didn't drive an actual PATCH
   request end-to-end to observe the stale-token state directly.
3. **A real consumer** — no shipped example uses `#[confidential]` yet (the
   only occurrences outside the framework's own tests are in this session's
   probe and the framework source). The threat-model and redaction tests are
   all synthetic-schema unit/integration tests; a first real adopter example
   would be the next charter to actually stress the feature under a shape
   nobody chose specifically to test it.

## Reproduce

```sh
cd /home/user/autumn
cargo fmt --all -- --check
cargo clippy -p autumn-web --features "sqlite,test-support" \
  --test confidential_repository_bidx -- -D warnings
cargo test -p autumn-web --features "sqlite,test-support" \
  --test confidential_repository_bidx -- --nocapture
# expect: test a_blind_index_lookup_never_crosses_owners ... ok
```
