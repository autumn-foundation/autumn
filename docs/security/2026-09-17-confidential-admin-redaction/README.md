# 2026-09-17 — `#[confidential]` columns × the admin plugin HTTP surface (negative result)

## 🎯 Surface

`autumn_web::confidential` (issue #1771, landed 2026-09-16 as PR #2819) ×
`autumn-admin-plugin`'s list, detail, edit-form and CSV-export routes
(`GET /{slug}`, `GET /{slug}/{id}`, `GET /{slug}/{id}/edit`,
`GET /{slug}/export.csv`). Entry point investigated: an application that
registers a model with a `#[confidential(blind_index)]` column into the
admin panel via `AdminPlugin::new().register(...)`, exactly as
`docs/guide/confidential-fields.md` and `autumn-admin-plugin`'s own README
show for any other model.

## 🕵️ Threat model (hypothesis)

`autumn/src/confidential.rs`'s own module doc claims a specific
`OPERATOR_BLIND_SINKS` entry for `admin_ui`: "the admin cell renderer
redacts registered confidential columns, and never offers an editable
control for one," plus a CSV-export claim that the export "drops
confidential columns and their blind-index companions." Against an app
that mounts `#[confidential]`-bearing model in the admin panel — the
documented, ordinary way to give an operator CRUD over their own tables —
an operator with only the admin role (no `RootKey`, which the server never
holds by construction) could read the sealed envelope or, worse, the
blind-index token through the admin UI or its CSV export, if any of the
four request-serving code paths that build an `AdminField`-derived view
(list cell, detail cell, edit-form widget, CSV row) forgot the
`confidential::is_confidential_column_name` check its siblings have. That
would be the framework's own contract failing on a surface it explicitly
claims to cover — row 5 of the audit's classification table ("a framework
primitive weaker than its own documented contract").

The blind-index token is the sharper of the two: `confidential.rs` treats
it as a correlation handle ("stable per value per owner... let anyone
reading a log correlate requests") and filters it out of every log/capsule
sink. If the admin CSV export or list/detail views leaked it while the
sealed envelope stayed masked, that would be a real, documented-against
leak — the operator could tell which of an owner's rows share a value
without ever holding the key.

This exact combination — a `#[confidential]` column driven through
`autumn-admin-plugin`'s real HTTP routes rather than through
`templates.rs`'s private render helpers called directly, or through
`AdminModel` methods called directly the way `custom_admin_model.rs`'s
other tests do — had no prior test coverage anywhere in the workspace.
`autumn/tests/integration/confidential_*.rs` cover the crypto core, the
model layer and the framework-owned sinks (DB, log, capsule, version
history); `autumn-admin-plugin`'s own test suite
(`token_admin_db.rs`, `experiment_admin_db.rs`, `custom_admin_model.rs`,
`impersonation_admin.rs`, …) never registers a confidential column at all,
and none of the crate's existing tests exercise `#[encrypted]` redaction
through HTTP either — the admin plugin's masking logic in `templates.rs`
and `traits.rs` was entirely unverified by anything that sends a real
request through the router.

## 🧪 Reproduction attempt → negative result

Test: `autumn-admin-plugin/tests/custom_admin_model.rs::confidential_columns_stay_masked_across_the_admin_http_surface`,
added alongside a new `admin_sealed_notes` fixture table, an
`AdminSealedNote` model (`#[confidential(blind_index)]` on `sealed_body`,
registering it in this test binary's `inventory` registry exactly as
`autumn/tests/integration/confidential_model.rs` does) and a hand-rolled
`SealedNoteAdminModel: AdminModel` reading that table — same shape as the
file's existing `WidgetAdminModel`, so it runs on both backends via
`autumn_web::backend_select!`. Seeds one row with a *real* `Sealed`
envelope and `BlindIndex` token (`RootKey::generate()`, a genuine
`FieldContext`, real AES-256-GCM/HMAC output — not placeholder strings),
mounts it through `AdminPlugin::new().register(...)` on a real
`autumn_web::test::TestApp` router, logs in as an admin session, and
drives all four routes through `TestClient`, asserting the exact envelope
string and the exact blind-index hex token never appear in any response
body, and that the confidential-field mask (`"sealed for its owner"`) does
appear on list/detail.

```
cargo test -p autumn-admin-plugin --features autumn-web/sqlite \
  --test custom_admin_model confidential_columns_stay_masked -- --ignored --nocapture
```

Result: **pass** — no leak on any of the four routes. See `after.txt` for
the full run. The non-confidential `title` column (`"checkup notes"`)
renders normally on the list view in the same response, which rules out
the assertion being vacuously true because nothing rendered at all.

Docker was not available in this sandbox to also run the Postgres arm
locally (`cargo test -p autumn-admin-plugin --test custom_admin_model --
--ignored`, no `--features autumn-web/sqlite`); it runs unmodified in
CI's existing Docker step (`.github/workflows/ci.yml` already names
`--test custom_admin_model` for both the Postgres/Docker lane and the
SQLite lane — see "Blast radius" below), and the redaction logic under
test (`templates.rs`, `traits.rs`, `routes.rs`) reads `serde_json::Value`
records and `&'static str` column names only; it has no backend-specific
branch, so the SQLite result is representative.

## 🔎 Root cause of the fail-safe behavior

Every request-serving code path that can put a field's value into a
response already gates on `confidential::is_confidential_column_name`
(a name-based lookup against the process-wide `inventory` registry
`#[model]` populates, independent of how the `AdminModel` implementing the
route was itself written):

- `render_cell_value` (list view) — `autumn-admin-plugin/src/templates.rs:2000`
- `render_detail_value` (detail view) — `autumn-admin-plugin/src/templates.rs:2135`
- `render_readonly_display` (create-only field on edit) — `autumn-admin-plugin/src/templates.rs:2178`
- `render_form_widget` (edit/create form control) — `autumn-admin-plugin/src/templates.rs:2236`
- `AdminModel::csv_export_columns` default impl — `autumn-admin-plugin/src/traits.rs:623`
- `model_export_csv`'s own re-filter, for a model that overrides `csv_export_columns()` — `autumn-admin-plugin/src/routes.rs:1311`

All six sites check by column **name**, not by anything the `AdminField`
the route handler supplied carries (unlike `#[encrypted]` redaction, which
depends on a per-field `encrypted: bool` the `AdminModel` author sets).
That is stronger, not weaker: a hand-rolled `AdminModel` — the exact shape
this reproduction and `custom_admin_model.rs`'s pre-existing tests use —
cannot forget to flag a confidential column, because nothing about its
`AdminField` construction is consulted; the registry lookup alone decides.
The blind-index companion column gets the same treatment everywhere
(`confidential::registered_confidential_column_names()` includes it, and
every site above checks `is_confidential_column_name` on both the sealed
column and its `_bidx` companion).

Cross-checked against the six other places `registered_confidential_column_names()`
/ `registered_encrypted_column_names()` feed a `ParameterFilter` for the
access log, error-page capture, and structured log context
(`router.rs:4103-4104`, `router.rs:5141-5147`, `router.rs:5183-5189`,
`router.rs:5328-5330`, `job.rs:2239-2241`, `telemetry.rs:439-441`): every
one extends the filter with **both** `registered_encrypted_column_names()`
and `registered_confidential_column_names()` — no asymmetry there either.

## 🩹 Fix

None — no bug found. Regression test added at
`autumn-admin-plugin/tests/custom_admin_model.rs`
(`confidential_columns_stay_masked_across_the_admin_http_surface`), which
needs no new CI wiring: it lives in the same test binary
(`custom_admin_model`) `.github/workflows/ci.yml` already names for both
the Postgres/Docker lane (line ~1368) and the SQLite lane (line ~1973),
confirmed by `autumn-admin-plugin/tests/ci_coverage.rs`'s own
`every_ignored_test_binary_is_named_in_ci` check, which still passes
unchanged. This closes the coverage gap so a future change to any of the
six redaction sites above — or a seventh one added later without the same
check — fails this test loudly instead of shipping unverified, the way
`autumn/tests/integration/confidential_threat_model.rs` guards the
framework-owned sinks.

## 🔁 Review round (Codex)

Codex's automated PR review caught two real gaps in the first version of
this test, both fixed before merge:

1. The fixture inherited `AdminModel::csv_export_columns()`'s default
   implementation, which already strips confidential columns in
   `traits.rs` before `model_export_csv` (`routes.rs:1311`) ever runs its
   own re-filter — so the CSV assertions never actually exercised that
   route-level guard, which exists specifically for a model that
   overrides `csv_export_columns()` to return a curated list. Fixed by
   overriding `csv_export_columns()` in the fixture to deliberately
   *include* `sealed_body`/`sealed_body_bidx`, so the route's own filter
   is the only thing standing between the model and a leaked export.
2. The edit-form and CSV-export sections asserted only the *absence* of
   the envelope/token, so an unrelated regression (a 401, 404, 500, or an
   empty body) would have passed those two checks vacuously without ever
   reaching the redaction code. Fixed by asserting `assert_ok()` plus a
   positive marker (the mask text on the edit form, the non-confidential
   columns in the CSV body) before the negative assertions.

Re-running after both fixes caught a **real bug in the test itself**, not
the framework: the edit-form mask text is `"Sealed for its owner — the
server cannot read or write it"` (`templates.rs:2239`, capital S, a
different string from the lowercase `"sealed for its owner"` the list,
detail and readonly-display renderers use at `templates.rs:2001/2136/2181`).
The first positive assertion used the lowercase list/detail wording and
failed — the fixture was checking for text the edit-form path never
emits, which is exactly the kind of accidentally-vacuous check the second
Codex finding was about. Corrected to the actual string.

A third Codex pass, on the fixed commit, found one more gap of the same
shape: `render_cell_value` (list view only) truncates any plain-string
cell to 80 characters with an ellipsis (`truncate_display`), and the
seeded envelope is ~108 base64 characters — past that limit. Checking the
*full* envelope string against the list HTML would pass vacuously if the
list's confidential check regressed, since a regressed render would leak
only a truncated ciphertext prefix, never the complete string the
assertion looked for. Fixed by checking a 60-character prefix instead,
comfortably inside `truncate_display`'s 79-character keep window, so a
leak there is still caught.

A fourth Codex pass found a genuine coverage gap in the fixture's field
setup: with both confidential fields plain (no field-level flags at all),
the edit route sent both `sealed_body` and `sealed_body_bidx` through
`render_form_widget`, so `render_readonly_display` — the separately-coded
confidential check for a `create_only` field on the edit page
(`templates.rs:2178`) — was never exercised by this HTTP-level test at
all. Fixed by marking `sealed_body_bidx` `.create_only()`, which routes it
through `render_readonly_display` on `GET .../edit` instead, and adding a
positive assertion for that path's own mask text (lowercase `"sealed for
its owner"`, distinct from `render_form_widget`'s capitalized wording).
See `after.txt` for the run after all four fixes.

## ✅ Verification

- `cargo fmt --all -- --check` — clean.
- `cargo check -p autumn-admin-plugin --test custom_admin_model` (default/Postgres feature set) — clean.
- `cargo check -p autumn-admin-plugin --features autumn-web/sqlite --test custom_admin_model` — clean.
- `cargo test -p autumn-admin-plugin --features autumn-web/sqlite --test custom_admin_model -- --ignored` — 4/4 pass, including the pre-existing three tests in the file (no regression).
- `cargo clippy -p autumn-admin-plugin --all-targets -- -D warnings` — clean (one pre-existing, unrelated `unknown_lints` warning from `autumn-macros`, not from this change).
- `cargo clippy -p autumn-admin-plugin --features autumn-web/sqlite --all-targets -- -D warnings` — clean, same pre-existing warning.
- `cargo test -p autumn-admin-plugin --test ci_coverage` — 2/2 pass (no CI wiring gap introduced).
- `./scripts/pre-push-check.sh` — see `pre-push-check.txt`.
- Re-attack: tried moving the assertions earlier (only checking the list
  view) to make sure I wasn't relying on later routes to "flush out" a bug
  the first route already had; each of the four routes has its own
  positive assertion (mask text present) and negative assertion (envelope
  and token absent), and a variant run confirmed the test fails loudly
  (not vacuously) if the confidential check in any one of the four
  templates.rs functions is commented out locally.

## 📡 Blast radius

- Swept `is_confidential_column_name`'s six call sites in
  `autumn-admin-plugin` (listed under Root cause) — all six correct, no
  fix needed.
- Swept the framework-side log/capsule filter call sites (six, listed
  under Root cause) — symmetric with `registered_encrypted_column_names()`
  at every site, no asymmetry found.
- Checked `#[encrypted]` redaction in the same four `templates.rs`
  functions as a sibling class — present and correctly ordered (the
  `#[confidential]` check runs first in `render_form_widget`, so a column
  cannot be marked both and fall through to the weaker `#[encrypted]`
  edit-form branch, though the two attributes are mutually exclusive by
  the macro's own build-time refusal).
- Feature-independent: `#[confidential]` and the admin plugin's redaction
  logic are both default-feature-set code (`db` only gates whether the
  column type compiles), so this reproduces (or, here, fails to
  reproduce) identically under `--features autumn-web/sqlite` and the
  default Postgres backend — confirmed by running the SQLite arm and
  type-checking the Postgres arm (Docker unavailable locally; the
  Postgres arm is unchanged code from the file's pre-existing
  `WidgetAdminModel` pattern, already proven correct by
  `a_custom_admin_model_runs_on_the_active_backend`).

## 📜 Compatibility

No behavior change, no `CHANGELOG.md` entry (test-only addition, matching
this repo's convention for negative-result commits — see
`docs/security/2026-09-06-idempotency-token-principal/` and
`docs/security/2026-09-10-mcp-secured-guard-dispatch/`).

## 🗂 Ledger

This directory. `after.txt` has the full green SQLite test run;
`pre-push-check.txt` has the compile-only workspace gate.
