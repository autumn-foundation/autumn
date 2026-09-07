---
name: generate
description: >
  Use when the user runs /autumn:generate, asks to scaffold a resource,
  generate a model, migration, controller, mailer, or task using the
  autumn CLI.
argument-hint: "<subcommand> <Name> [fields...] [flags]"
allowed-tools:
  - Bash
  - Read
  - Write
  - Edit
---

# autumn:generate

Wrap `autumn generate <subcommand>` commands. Always show a redacted command
preview and get confirmation before executing any mutating generator.

**Version check first**: rows and tokens marked **(trunk-dev)** below exist
only on the trunk-dev CLI, NOT in the published `autumn-cli 0.5.0`. Run
`autumn --version` (or check how the CLI was installed) before suggesting
them; on 0.5.0 fall back to the documented manual alternative.

## Supported subcommands

| Subcommand | Example | What it creates |
|---|---|---|
| `scaffold` | `scaffold Post title:String body:Text` | Model + migration + routes (index/show/new/create/edit/update/delete) + views + a test suite (in-process read test + write-path CRUD test). Also updates `src/main.rs`. |
| `controller` **(trunk-dev)** | `controller pages home about contact` | Handler-only route module for non-CRUD pages/endpoints — one handler per named action, Maud stub views (HTTP 200), wired into `routes![...]` and `src/routes/mod.rs`. **No** model, migration, DB, schema, or `Cargo.toml` changes. `--api` emits JSON actions instead of views. |
| `model` | `model Post title:String body:Text` | Model struct + migration |
| `migration` | `migration add_slug_to_posts` | Empty timestamped migration file |
| `mailer` | `mailer User` | Mailer struct + email templates (generator appends `Mailer` → produces `UserMailer`) |
| `task` | `task RecalculateCounts` | `#[task]` operational command |
| `auth` | `auth User --oauth github,google` | Full auth scaffold (login/register/password reset/OAuth); **(trunk-dev)** also scaffolds a configurable password policy and persistent "remember me" login by default, authenticated change-password/change-email flows, and `--magic-link` passwordless login |
| `admin` | `admin Post title:String body:Text` | Admin plugin resource page — fields must be supplied explicitly; generator does not read the model |
| `policy` **(trunk-dev)** | `policy Post` | Scaffolds `<Pascal>Policy`/`<Pascal>Scope` for an EXISTING model — owner-or-admin `update`/`delete` plus an owner-scoped `list`. When no owner column (`user_id`/`author_id`/`owner_id`) is found, emits a default-deny TODO stub instead (issue #1125) |
| `teams` **(trunk-dev)** | `teams` | Organization membership + email invitations (issue #1261): `Organization`/`Membership`/`Invitation` models + migrations, a closed `Owner`/`Admin`/`Member` role + `require_role` guard, an `InvitationMailer`, and member-management routes under `src/teams/`. Takes **no name argument** — always emits this fixed set. Composes `#[repository(tenant_scoped)]` (#695) and the session `"role"` key (#496); does not scaffold its own login/signup — see "teams" below. |
| `webhook` **(trunk-dev)** | `webhook stripe Payments` | Signature-verified, replay-protected inbound provider webhook (issue #1366): `#[post("/webhooks/<provider>")]` handler over the shipped `SignedWebhook` extractor, event-type dispatch with `on_*` stubs, the `[[security.webhooks.endpoints]]` block in `autumn.toml` (`secret_env`, replay on — never an inline secret), and tests asserting 200/400/401/409 for valid/missing/wrong-signature/replayed deliveries. Presets: `stripe`, `github`, `slack`, `generic`; `--path`/`--secret-env` override the defaults. |
| `system-test` | `system-test checkout_flow` | System test fixture (name must be `snake_case` or `PascalCase` — no hyphens) |
| `pwa` | `pwa` | PWA scaffolding — manifest, service worker, offline shell, icons, route handlers, smoke test |
| `wizard` | `wizard checkout shipping payment review` | Session-backed multi-step form — step structs, GET/POST handlers, confirm/commit/cancel, and ignored integration test skeletons |
| `tauri` **(trunk-dev)** | `tauri` | `src-tauri/` desktop sidecar project; purely additive; then `cargo tauri build` |
| `plugin` **(trunk-dev)** | `plugin my-plugin` | Installable/conformant Autumn plugin crate |

**Reversing a generator (trunk-dev)**: `autumn destroy <subcommand> <args>`
mirrors `autumn generate` argument-for-argument and cleanly removes what it
created (`autumn destroy scaffold Post title:String`, `--dry-run` supported).
It never touches a database — only generated files/migrations. On the
published 0.5.0 CLI, revert by hand (git) instead.

`destroy` refuses to delete a file you edited. `generate` records a digest of
every file it owns in `.autumn/generated.toml` (commit it), and `destroy`
deletes a file whose content matches that digest or the current templates.
A CLI upgrade that changed a template therefore no longer forces `--force`.
The entry records the command's arguments, a fingerprint of the
`autumn.generate.toml` they resolve from, and the resolved database backend, so
the digest counts only when all three match.
Two cases still need it: a project generated before the manifest existed, and
a file you edited and want deleted anyway. Re-running the original generator
command with `--force` re-records the digest and is the better fix for the
first.

## Field type reference

Use the exact tokens below — the DSL parser is case-sensitive and does not
accept aliases like `Integer` or `Boolean`.

| Token | SQL type | Rust type |
|---|---|---|
| `String` | `TEXT NOT NULL` | `String` |
| `Text` | `TEXT NOT NULL` | `String` (alias for String) |
| `i32` | `INTEGER NOT NULL` | `i32` |
| `i64` | `BIGINT NOT NULL` | `i64` |
| `f32` | `REAL NOT NULL` | `f32` |
| `f64` | `DOUBLE PRECISION NOT NULL` | `f64` |
| `bool` | `BOOLEAN NOT NULL` | `bool` |
| `NaiveDateTime` | `TIMESTAMP NOT NULL` | `NaiveDateTime` |
| `DateTime` | `TIMESTAMPTZ NOT NULL` | `DateTime<Utc>` |
| `Uuid` | `UUID NOT NULL` | `Uuid` |
| `Bytea` | `BYTEA NOT NULL` | `Vec<u8>` |
| `Attachment` | `JSONB NULL` (blob metadata) | `Option<Blob>` (always nullable) — **requires the `storage` feature**. **(trunk-dev)** the scaffold auto-enables autumn-web's `storage` + `multipart` (and uuid `v4`) in the project's Cargo.toml when an Attachment field is present, so it compiles with no manual edits; on published 0.5.0 add the `storage` feature by hand. **(trunk-dev)** the scaffold's create/update handlers take a `Multipart` extractor and stream to the blob store via `save_to_blob_store`, and the show/edit views render the stored file as a signed download link (issue #1236) |
| `Option<T>` | Nullable version of any above | `Option<T>` |
| `references` **(trunk-dev)** | `post:references` → `post_id BIGINT NOT NULL REFERENCES posts(id)` + auto index | `i64` (field name gets `_id` appended; `post:references?` for `Option<i64>`) |
| `enum{a,b,c}` **(trunk-dev)** | `TEXT` + `CHECK (col IN (...))` | Generated PascalCase Rust enum with Diesel/serde impls + `<select>` widget; `--default field=variant` sets SQL DEFAULT + `#[default]`. Quote the token in bash/zsh (brace expansion) |
| `decimal` / `decimal{10,2}` **(trunk-dev)** | `NUMERIC(12,2)` default, or explicit precision/scale | `rust_decimal::Decimal` (dependency added automatically); use for money, never `f64` |
| `:unique` modifier **(trunk-dev)** | `email:String:unique` → `CREATE UNIQUE INDEX` in the migration | Also generates a free `find_by_<field>` lookup and 23505 → 422 inline "already exists" form error. `--unique FIELD` is the flag equivalent |
| `slug{from:col}` **(trunk-dev)** | `TEXT NOT NULL` + implicit `:unique`'s `CREATE UNIQUE INDEX` | `String`; free `find_by_slug`; `show`/`edit`/`update`/`delete` routes and generated links key off the slug, not `id` (`GET /posts/{slug}`); blank on create auto-derives via `autumn_web::slugify(&new.<col>)` with a deterministic `-2`/`-3` collision suffix. At most one per model; not yet supported with `--live`/`--live-validation`/`--sharded`/an `Attachment` field/a `:states(...)` field. Quote the token in bash/zsh (brace expansion) |
| `{encrypted}` / `{encrypted:deterministic}` modifier **(trunk-dev)** | `api_token:String{encrypted}` → unbounded `TEXT NOT NULL` sized for the base64 ciphertext envelope | `String` (plaintext in Rust, ciphertext at rest). Emits `#[encrypted]` / `#[encrypted(deterministic)]` on the model (issue #1340); the deterministic mode keeps `find_by`/`exists_by` lookups and a real UNIQUE index, the randomized default keeps nothing queryable. `String`/`Text` and non-null only. Randomized + `:unique`/`--unique`/`--query`/`--index` is refused at generate time pointing at `deterministic`; `--searchable`, `--default`, `--shard-key`, `:states(...)` and `slug{from:<encrypted col>}` are refused in both modes. The generated index cell shows `••••••••` (unsorted), the CSV export omits the column, and the admin redacts it. Needs key material under `[active_record_encryption]` — run `autumn credentials edit`. Quote the token in bash/zsh (brace expansion) |
| `{translatable}` modifier **(trunk-dev)** | `title:String{translatable}` → `TEXT NOT NULL DEFAULT '{}'` holding a JSON object of locale tag → value | `autumn_web::i18n::Translated`, a per-locale container (issue #1384). Emits `#[translatable]` on the model; `Display` renders the request's **active** locale so `html! { h1 { (post.title) } }` needs no `Locale` parameter, falling back through `I18nConfig::resolved_fallback_chain` and then to a single sentinel (`None` from `resolve()`, `""` from `Display`) — never a panic or 500. Generates per-field `<f>_localized()` / `<f>_in(locale)` / `set_<f>(locale, value)` / `<f>_locales()` / `<f>_is_translated(locale)` plus field-name-keyed `available_locales("title")` / `is_translated("title", "fr")` / `Post::translatable_fields()` for a "needs translation" badge. `String`/`Text` and non-null only. **`generate scaffold` refuses it** (its single-input CRUD form would replace the whole container on save) — use `generate model`, which also enables autumn-web's non-default `i18n` feature, and write the per-locale edit view yourself. `{encrypted}`, `:unique`/`--unique`, `--index`, `--searchable`, `--shard-key`, `:states(...)`, `Option<…>` and the `{min}`/`{max}`/`{email}`/`{url}` validation fan-out are all refused. Assigning a `Translated` **replaces** every locale — use `merge_from` for a partial update. A pre-existing plain-text column upgrades in place with no data migration. Quote the token in bash/zsh (brace expansion) |
| `lock_version` (magic column name) **(trunk-dev)** | `INTEGER`/`BIGINT NOT NULL DEFAULT 0` | Declaring a non-nullable `i32`/`i64` column literally named `lock_version` opts the model into optimistic locking (issue #1318): the model gets `#[lock_version]`, so the column is DB-managed (out of `New{Model}`, carried on `Update{Model}` as the *expected* version, conflict-checked by `#[repository]`, and JSON `PUT`/`PATCH` clients must now send it). A scaffold hides it in the edit form, turns `update` into `WHERE lock_version = $expected` + `SET lock_version = lock_version + 1`, and re-renders the form at **409** with the author's input and an inline banner when a concurrent editor got there first. On HTML scaffolds, not supported with `--live`/`--sharded`/a `slug` column/an `Attachment` field, and never as a model's only insertable column or marked `:unique` — all refused at generation time (`--api` is exempt from the variant gates: it emits no form). `--live-validation` is supported. Rename the column if you wanted a plain counter |
| `position` / `position{scope:col}` **(trunk-dev)** | `BIGINT NOT NULL DEFAULT 0` + an auto index (composite `(scope, position)` when scoped) + insert-assign/delete-compact triggers | `i64`. Declares a user-orderable list (issue #1358, Rails' `acts_as_list`): the column is DB-managed (`#[position]`, out of `New{Model}`/`Update{Model}` entirely — it's never hand-set), new rows append to the end of their scope on insert, and deleting (or soft-deleting) a row compacts the remaining live rows so no gap is left — all via database triggers, so the invariant holds for every insert/delete path, not just the generated repository's. `#[repository]` gains transaction-safe `move_to(id, n)` / `move_before(id, other)` / `move_after(id, other)` / `move_up(id)` / `move_down(id)`, each an `O(rows shifted)` shift-then-set inside one transaction, locking the scope's rows (ordered by id — a fixed lock order, so two concurrent movers on the same scope serialize rather than deadlock) before touching them. `position{scope:board_id}` keeps a separate contiguous `0..len-1` sequence per distinct value of a sibling `references` column instead of one sequence over the whole table; the scope column must already be declared as `references`. At most one `position` field per model. The HTML scaffold's index orders by it and adds no-JS Move up/down buttons (CSRF + one-time-submit-token protected, like the trash view's Restore button) — omitted for `--api`/`--live`/`--live-validation`/`--sharded`/owner-scoped scaffolds, though `--api`/`--live`/`--live-validation`/owner-scoped repositories still get the `move_*` methods (just no buttons); `--sharded` gets neither (`#[repository(..., sharded)]` rejects `position(...)` outright — a move only reaches the pool/shard it happens to be given). Not yet supported with `tenant_scoped`/`versioned = true`/`dependent(...)`. Quote the token in bash/zsh (brace expansion) |

**Foreign keys** (published 0.5.0 CLI only — no `references` token): scaffold
an `i64` field and hand-edit the generated migration to add
`REFERENCES users(id)` and an index. On trunk-dev, use `user:references`.

**Indexes/UNIQUE** (published 0.5.0 CLI only — no `:unique` token): add them
by hand in the generated migration's `up.sql`. On trunk-dev, use `:unique` /
`--unique FIELD` / `--index FIELD`.

**Do not use UUID as a primary key.** Primary keys are always `i64` /
`BIGSERIAL`. Use `Uuid` as a secondary column for external correlation only.

**SQLite backend (trunk-dev, #1614)**: the generator auto-detects the target
backend from the resolved database URL (`sqlite://…` in config / env / dotenv →
SQLite, else Postgres) and emits SQLite-appropriate DDL (e.g. `TEXT` for
`Decimal`, `TEXT PRIMARY KEY`/`Timestamp` remaps). `generate auth` and `generate
mailer --list-unsubscribe` are backend-aware too (#1927): on a SQLite app they
scaffold SQLite-dialect migrations (`INTEGER PRIMARY KEY AUTOINCREMENT`,
`DEFAULT CURRENT_TIMESTAMP`, `INTEGER` foreign keys) instead of being rejected,
and the generated auth session store is typed against
`::autumn_web::RuntimeConnection` so it compiles on either backend (#1908). That
store's query functions also bind `::autumn_web::RuntimeBackend` rather than
`diesel::pg::Pg` (#1908), so the tracked-sessions store compiles and runs on the
SQLite connection; the scaffolded session-management and OAuth guides emit their
operator SQL in the app's dialect too.
**Every** field kind now has a working diesel SQLite conversion (#1924): a
SQLite app's `Cargo.toml` gets the SQLite dependency set (diesel on `sqlite`,
bundled `libsqlite3-sys`, `autumn-web/sqlite`, no `pq-sys`), a generated
`enum{…}` carries `Text`/`Sqlite` impls, and `Uuid` / `decimal{p,s}` render
`autumn_web::db::sqlite_types::{SqliteUuid, SqliteDecimal}` — `TEXT`-backed
newtypes that are `Copy`, deref to the wrapped type, and are
`#[serde(transparent)]` (`uuid::Uuid` and `rust_decimal::Decimal` are foreign
types no crate but their own can convert for SQLite). `--searchable` works via
FTS5 (#1910); UUID primary keys and `--sharded` remain Postgres-only. A
scaffolded SQLite app's `tests/<model>.rs` still uses the Postgres-only
`TestDb`, so `cargo test` on it does not compile yet (#1905).

**Scaffold form behavior (trunk-dev)**: generated `create`/`update` handlers
build a `Changeset` and, on a rejected submission, respond **422** and
re-render the form with all submitted values preserved and per-field inline
errors — do not "fix" this by adding a redirect-to-error-page. The published
0.5.0 scaffold returns a 400 error page instead. Trunk-dev create/edit views
(and both 422 branches) render through one shared `{snake}_form_for` helper —
a single `autumn_web::form::form_for` call driven by the `#[model]`-derived
`FormModel` (enum columns get a `Select` override with their variants,
`decimal` columns pin the browser `step` to the declared scale, `references`
columns render a `<select>` of the referenced table's ids when that model
exists in the project, attachment columns append a file input at the end of
the form) — adding a column needs no view edits. `--live-validation` keeps the
per-field htmx emission instead.

**Scaffold test suite (trunk-dev)**: the generated `tests/<snake>.rs` exercises
the whole CRUD surface in-process via `autumn_web::test::{TestApp, TestClient}`,
not just the read path. Alongside the `<plural>_index_renders_scaffolded_rows`
read test (DB-backed, `#[ignore]`d unless Docker is available), a
`<plural>_write_path_crud` `#[tokio::test]` drives **create / update / delete
and the validation-failure re-render**: a valid `POST` redirects (303 See Other)
and the row is observable on a follow-up read, an invalid `POST` re-renders the
form at 422 with the submitted input preserved and an inline `role="alert"`
error (and does not persist), an update is observable on re-read, and a delete
removes the row. It runs on a process-local in-memory store — no database, no
running server — so it is a visible green (never `#[ignore]`d). `TestApp`
disables CSRF, so the same-origin form `POST`s carry no `_csrf` token (the real
`form_for`-rendered forms inject one for the browser; the in-process harness
does not require it). Emitted for HTML scaffolds only — `--api` scaffolds get
the JSON read test but no write-path suite.

**Scaffold `{…}` validation, belongs_to dropdowns & seed linking (trunk-dev)**:
three additive scaffold behaviors (issues #1388, #1146, #1718):

- **Inline `{…}` validation + HTML5 constraints** — a trailing `{…}` block on a
  field declares a constraint once and enforces it on both sides: the model
  field gets a `#[validate(...)]` rule and the form input gets the matching HTML5
  attribute. `title:String{min=3,max=120}` → `length(min,max)` +
  `minlength`/`maxlength`; numeric `{min,max}` → `range` + `min`/`max`;
  `{email}` → `email` + `type="email"`; `{url}` → `url` + `type="url"`. A bad
  submission is a 422 with inline per-field errors; a misspelled modifier (e.g.
  `{maxx=5}`) fails the scaffold naming the token. Quote the whole token in
  bash/zsh so the shell doesn't brace-expand the comma.
- **belongs_to dropdowns** — when a `post:references` parent model exists, the
  new/edit form renders the FK as a populated `<select>` (one `<option>` per
  parent row) and index/show render the parent's display value instead of the
  raw `*_id`. The display column is `name`/`title` → first string column → id;
  override with `'post:references{label:slug}'`. A nullable `post:references?`
  gets a blank "— Unset —" first option.
- **Seed-binary model linking** — when a `src/bin/seed.rs` exists, the generator
  idempotently splices `#[path = "../schema.rs"] mod schema;` +
  `#[path = "../models/mod.rs"] mod models;` into it so `autumn seed
  --count/--model` resolves the model's factory; `autumn destroy` removes those
  links again. See `docs/guide/generators.md`.

**Constrained-required numerics & `--live-validation` parity (unreleased — trunk-dev, issues #1748, #1750)**: two refinements to the `{…}` block above:

- **Blank-state for constrained required numerics** — a required numeric field
  with a `{min,max}` range (e.g. `age:i32{min=0,max=130}`) is emitted as
  `Option<T>` on the generated `*Form` struct with `#[validate(required,
  range(...))]`, not the native `i32`/`i64`/`f32`/`f64`. A native numeric
  defaults to `0` and pre-fills the input, so a blank submit used to pass both
  `required` and `range` when the range spans zero and silently persist `0`;
  `Option<T>` defaults to `None`, renders blank, and `required` rejects it with a
  **422** inline error. The empty `field=` pair is also dropped by the form
  decoder, so a blank non-browser submit yields the 422 rather than a `400`. The
  `required` rule sits on the form struct only, never the model field; a nullable
  numeric is already `Option<T>` and is unaffected (issue #1748).
- **`--live-validation` HTML5 + belongs_to parity** — `--live-validation`
  scaffolds now emit the same client-side HTML5 constraint attributes
  (`minlength`/`maxlength`, `type="email"`/`type="url"`, numeric `min`/`max`,
  `step="any"`, and a `<textarea>` for a constrained `Text`) and the `belongs_to`
  parent `<select>` as the standard `form_for` path; both were previously dropped
  on the per-field live path. Server-side `#[validate(...)]` was already enforced
  under `--live-validation` — this only restores the client-side hints and the
  request-time populated dropdown (with changeset-driven `selected` state), and
  the inline-validate fragment returns identical markup so an htmx `outerHTML`
  swap on `change` keeps the guards (issue #1750). See `docs/guide/generators.md`.

## Execution flow

1. Parse the subcommand and arguments from user input.
2. Build the full `autumn generate` command string.
3. Show the redacted preview:
   ```
   Will run: autumn generate scaffold Post title:String body:Text
   Project root: /path/to/project
   ```
4. Ask for confirmation: "Proceed? (yes/no)"
5. Only execute after explicit confirmation.
6. Run the command and capture stdout, stderr, and exit code.
7. Show what was created (the file list from stdout).
8. Show the mandatory next steps for the subcommand (see below).

## Next steps per subcommand

### scaffold
```
Next steps:
1. Run: autumn migrate   (the generator already updated src/main.rs)
2. Run: autumn dev
3. Visit: http://localhost:3000/<plural>
```

**Scaffold record-level authorization (trunk-dev)**: scaffolds now emit a
default-deny `Policy`/`Scope` by default. When an owner column is detected
(`user_id` → `author_id` → `owner_id` → the first `*_id` column referencing
`users`), the generator authorizes the create/edit/update/delete handlers and
owner-scopes the index; pass `--no-policy` to opt out (issue #1125).

**Scaffold typed path module (trunk-dev)**: scaffolds reference a generated
`autumn_web::paths![index, show, new, create, edit, update, delete]` module
(`+events` under `--live`, `+validate_{field}` under `--live-validation`,
`+nested_index, nested_create` under `--belongs-to`, `+trash, restore, purge`
under `--soft-delete`) for
every href/action/redirect/SSE endpoint/`hx-post` target instead of literal URL
strings (issue #1133).

**Scaffold sortable/filterable index (trunk-dev)**: the non-live,
non-owner-scoped `GET /<plural>` index wires allowlisted sort/filter through the
`ListQuery` extractor and `repo.list(&list_query, &page_req)` (`?sort=&dir=`,
`?filter[col]=`), rendering sortable `data_table` headers (blob/attachment/enum
columns excluded). Owner-scoped and `--live` indexes opt out (issue #1126).

**Scaffold bulk select + delete selected (trunk-dev)**: every standard HTML
index list ships a no-JavaScript bulk-delete flow — a leading
`bulk_select_checkbox` column, the list (and, with `--searchable`, the whole
htmx-swapped results container) wrapped in a `bulk_actions_form`, and a
`#[secured] POST /<plural>/bulk_delete` handler mounted right after `destroy`.
The handler drops malformed ids and redirects on an empty selection (a
list-write endpoint never 400s), per-row authorizes with the same `"delete"`
action `destroy` uses when policy wiring is on (an unauthorized row is dropped
from the batch, not 403'd), and deletes through `repo.delete_many` so
soft-delete, hooks and `dependent(...)` cascades all apply. Not emitted for
`--live`, `--live-validation`, `--sharded`, or `--api` (issue #1312).

**Scaffold CSV export (trunk-dev)**: every standard HTML index also ships a
working **Export CSV** download — a `CsvSchema` impl for the model (in
`src/routes/<plural>.rs`, covering `id`, every scaffolded column in
declaration order, then `created_at`), a `#[get("/<plural>/export.csv")]`
handler returning a `Download` (so `Content-Type: text/csv`,
`Content-Disposition: attachment; filename="<plural>.csv"` and
`Content-Length` come from the framework, not hand-built header strings), an
**Export CSV** link on the index carrying the current query string, and a
database-free generated test. The planner auto-enables autumn-web's `csv`
feature, so the scaffold compiles with no manual edits. The export honours the
same allowlisted `?sort=`/`?filter[col]=` params as the index via the same
`ListQuery` + `repo.list` pair (`?page=`/`?size=` are ignored — an export
spans every page; `?q=` is NOT honoured, since `ListQuery` carries no
full-text term), reads in `MAX_PAGE_SIZE` batches capped at `MAX_EXPORT_ROWS`
(10 000), and mirrors the index's security posture exactly: an owner-scoped
scaffold's export is `#[secured]` and goes through `list_scoped`, never the
unscoped `list`. It additionally carries `#[throttle(limit = 6, per = "1m",
key = "ip")]` the index does not — same row set, ~100x the cost per request.
`NULL` columns become empty cells, not the string `None`, and text columns
pass through an emitted `csv_text_cell` guard against spreadsheet formula
injection (numeric/date/bool/enum columns are not guarded — guarding them
would corrupt a negative number). Not emitted for `--live`, `--sharded`,
owner-scoped `--live-validation`, or `--api` (issue #1315).

**Scaffold CSV import `--import` (trunk-dev)**: the symmetric counterpart to the
export (issue #1393). `autumn generate scaffold Post title:String --import` adds
a `GET /<plural>/import` upload form (file input, an "Import for real" checkbox,
the expected header row printed from the same `CsvSchema` the export writes, and
the columns the import cannot set) and a `#[secured] POST /<plural>/import`
handler. **A dry run is the default**: unless the submit carries the `commit`
confirmation, the handler runs `autumn_web::data::csv::import_csv` in
`ImportMode::DryRun` and renders the `ImportReport` — rows read, rows that
*would* insert, and a table of row errors with **line numbers** — without
writing. A confirmed submit runs the same parse in write mode and commits
through the repository's `save_many_skip_invalid`, so a row the database rejects
is isolated and reported against its own CSV line instead of aborting the batch.
Each row is re-encoded and handed to the module's own `decode_form`, so it is
validated by exactly the `#[validate(...)]` rules a browser submission goes
through. The planner auto-enables autumn-web's `multipart` feature (`csv` comes
from the export). **Insert-only** — no row is matched against an existing
record, so re-uploading an exported file duplicates it; use
`ImportMode::Upsert { by }` by hand for update-in-place. Bounded by
`MAX_IMPORT_BYTES` (2 MiB) *and* `MAX_IMPORT_ROWS` (10 000): a file over the row
cap is **refused whole** with a 422 before anything is imported — never
partially imported — so an over-cap file must be split and the parts uploaded
separately. The count is taken by `autumn_web::data::csv::count_data_rows`
*before* `import_csv` runs, because a malformed row never reaches the row
handler. The upload is checked by extension **and** declared content type. Not emitted for `--live`, `--sharded`, owner-scoped
`--live-validation`, `--api`, or a model with an at-rest `{encrypted}` column
(the export omits that column but the form requires it) — the generator warns
and emits nothing, naming the reason and what to drop.

**Scaffold Trash view (trunk-dev)**: a `--soft-delete` standard HTML scaffold
also ships the recover-from-trash UI — a `#[secured] GET /<plural>/trash` page
listing deleted rows through the repository's generated `page_only_deleted`
(the paginated `only_deleted` scope; the handler writes no `deleted_at` filter
of its own), a **Trash** link in the index furniture, a `Deleted` column with
each row's `deleted_at` stamp, and per row a CSRF-protected **Restore**
(`POST /<plural>/{id}/restore` → `restore(id)`) and **Purge**
(`POST /<plural>/{id}/purge` → `purge(id)`) control — Purge behind
`confirm_action`'s server-rendered dialog, because the default
`script-src 'self'` CSP blocks an inline `onclick` confirm. Both write handlers
load their target with `deleted_at IS NOT NULL` first (so a crafted POST cannot
hard-delete a live row) and record-authorize it with the same `"delete"` action
`destroy` uses. The generated test walks create → soft delete → Trash →
restore → purge. Not emitted for `--live`, `--live-validation`, `--sharded`, an
owner-scoped index, `--api`, or any scaffold without `--soft-delete`
(issue #1332).

**Scaffold i18n views (trunk-dev)**: `--i18n` makes a generated resource
translatable the moment it exists — `t!(locale, "key")` lookups in place of
every English literal in the views, the `Locale` extractor on each view
handler, and an `i18n/en.ftl` back-filled with exactly the keys referenced (no
more — an unreferenced key is what `autumn i18n check` reports as unused). The
generated app passes `autumn i18n check --strict`. Reach for it whenever the
user mentions localization, translation, multiple languages, or a non-English
audience; otherwise leave it off, since the default scaffold stays
zero-i18n-config (issue #1349).

**Scaffold no-JS uploads (trunk-dev)**: `Attachment` fields produce working
`multipart/form-data` uploads without JS — the create/update handlers take a
`Multipart` extractor and stream to the blob store via `save_to_blob_store`
— the scaffold planner auto-enables autumn-web's `storage` and `multipart`
features (and uuid's `v4`) in the project's Cargo.toml, so a freshly
scaffolded resource compiles with no manual edits. The show and edit views
render the stored file as a signed, time-bounded download link
(`attachment_url` / `attachment_link`), and the edit form labels the currently
stored file above the file input (issue #1236).

### controller (trunk-dev)
```
Next steps:
1. The generator already:
   - Created src/routes/<controller>.rs (one handler per action)
   - Added `pub mod <controller>;` to src/routes/mod.rs
   - Wired each action into routes![...] and added `mod routes;` in src/main.rs
   No model, migration, database, schema, or Cargo.toml changes are made.

2. Path rule: each action maps to /<controller>/<action>, except an action
   literally named `index`, which maps to /<controller>. Under `--api` the
   prefix is /api/<controller>[/<action>].

3. Method rule: actions default to GET. Request another method with
   `action:method` (method ∈ get, post, put, patch, delete), e.g.
   `autumn generate controller pages home submit:post`.

4. HTML mode returns a Maud stub view (HTTP 200 placeholder) per action;
   `--api` returns AutumnResult<Json<serde_json::Value>> with a JSON stub and
   no view. Edit the generated handlers to add real content.

5. Re-running against an existing controller fails (non-destructive); pass
   `--force` to overwrite. `autumn destroy controller <name> <action>...`
   reverts it.

6. Run: cargo check   (then `autumn routes` lists every new route)
```

### model
```
Next steps:
1. The generator already created src/models/<snake>.rs and added
   `pub mod <snake>;` to src/models/mod.rs — no manual wiring needed.
2. Run: autumn migrate
3. Implement repository functions (free functions or #[autumn_web::repository])
```

### migration
```
Next steps:
1. Edit the generated migration file in migrations/<timestamp>_<name>/
2. Write up.sql (and down.sql if rollback matters)
3. Run: autumn migrate
```

### mailer
```
Next steps:
1. The #[mailer] macro generates send_<method> (async) and deliver_later_<method>
   (fire-and-forget) from each fn in the impl block. Call from a handler or job:

   // The generated method name matches the snake_case of your mailer name.
   // For `autumn generate mailer User`, the method is named `user`:
   // async send (awaits delivery):
   UserMailer.send_user(&mailer, to).await?;

   // fire-and-forget (background, no await):
   UserMailer.deliver_later_user(&mailer, to);

   // Rename the method in the generated file to get send_welcome, etc.:
   // pub fn welcome(&self, to: String) -> Mail { ... }
   // → generates send_welcome / deliver_later_welcome

   Both take a &Mailer extractor as their first argument after &self.
   Add `mailer: Mailer` to the handler's extractor list to get the handle.

2. The generator already adds mod mailers; and .mail_previews(...) to main.rs.
   The type lives at mailers::<snake>::<PascalName>Mailer, e.g.:
   use mailers::user::UserMailer;
   .mail_previews(mail_previews![UserMailer])

3. Preview at: http://localhost:3000/_autumn/mail (dev mode only)

4. CSS inlining (issue #1254): the scaffolded `templates/mailers/<name>.html`
   ships a `<style>` block with CSS classes, and the generated mailer calls
   `.inline_css(true)`. At send time those `<style>` rules are rewritten onto the
   elements as `style="…"` attributes so the mail renders styled in Gmail/Outlook
   (which strip `<head>`/`<style>`). Author with classes; keep `.inline_css(true)`.
   To default inlining on for every mailer instead, set `mail.inline_css = true`
   in autumn.toml and drop the per-message call.
```

### task
```
Next steps:
1. The generator writes the file to tasks/<name>.rs at the project root
   (not inside src/). Move it to src/tasks/<name>.rs or src/tasks.rs,
   or reference it from src/main.rs with:
   // Generator writes tasks/<snake_name>.rs at project root.
   // In src/main.rs, reference it with a path attribute:
   #[path = "../tasks/recalculate_counts.rs"]
   mod recalculate_counts;

   // Or move the file to src/tasks/<snake_name>.rs and add:
   // mod tasks; (with src/tasks/<snake_name>.rs inside it)

2. Register in main.rs (use the snake_case module and function name):
   .one_off_tasks(one_off_tasks![recalculate_counts::recalculate_counts])

3. Invoke with: autumn task recalculate_counts -- --dry-run
```

### wizard
```
Next steps:
1. The generator writes three files:
     src/wizards/<name>.rs       # step structs + handlers
     src/wizards/mod.rs          # pub mod <name>;  (created or appended)
     tests/<name>_wizard.rs      # ignored integration test skeletons

2. Fill in the generated TODO sections:
   - Replace `// TODO` in each step struct with real fields + #[validate(...)] attributes.
   - Replace `// TODO: render form fields` in each show_<step> handler with
     real form.text_input(...) / form.select(...) calls — copy the same block
     into the Err(form) branch of the matching POST handler.
   - Add a summary display in show_confirm for each step's data.
   - Replace `// TODO: use the step data` in commit with the actual DB write,
     then call wizard.clear().await after success.

3. Wire into src/main.rs:
   mod wizards;   // alongside other mod declarations
   // routes![...]:
   wizards::<name>::show_<step1>,
   wizards::<name>::submit_<step1>,
   // ... one pair per step ...
   wizards::<name>::show_confirm,
   wizards::<name>::commit,
   wizards::<name>::cancel,

4. Run: cargo check
```

### pwa
```
Next steps:
1. The generator already:
   - Created static/manifest.webmanifest, static/service-worker.js,
     static/pwa-register.js, static/icons/icon.svg (+ maskable variant)
   - Added route handlers (/manifest.webmanifest, /service-worker.js,
     /pwa-register.js, /offline) and PWA <meta>/<link> tags to src/main.rs
   - Created tests/system/pwa_smoke.rs and added system-tests to Cargo.toml

2. Replace static/icons/icon.svg with a real PNG icon for mobile installation.
   For iOS, also add 180×180 apple-touch-icon.png.

3. Edit static/manifest.webmanifest to set your app name, theme_color,
   background_color, and start_url.

4. Run the smoke test: cargo test --features system-tests pwa_smoke
```

### auth (trunk-dev)
```
Next steps:
1. `autumn generate auth User` now scaffolds a configurable password policy and
   persistent "remember me" login automatically — no extra flag.
   - Password policy `[auth.password]`: `min_length` (default 8),
     `reject_common` (default true; bundled weak-password corpus), and
     `breach_check` = off | fail_open | fail_closed (HIBP lookups). Enforced via
     autumn_web::auth::PasswordConfig / PasswordPolicy.
   - Remember-me `[auth.remember]`: `enabled` (default true), `duration_secs`,
     `cookie_name` (default "autumn.remember"). The generator adds a
     {snake}_remember_token model + table and wires the remember middleware.
2. Run: autumn migrate   (applies the sessions + remember-token migrations)
3. --oauth / --totp / --passkeys still apply; there is no --remember or
   --password-policy flag — both are on by default (#1345, #1397).
4. Authenticated account flows are scaffolded too (issue #1396): a
   change-password form at `GET`/`POST /account/password` (verifies the current
   password, applies the password policy, keeps the current device signed in)
   and a change-email flow at `GET`/`POST /account/email` — both `#[secured]`
   and behind a fresh `#[step_up]` claim, re-rendering at 422 on bad input.
5. Pass `--magic-link` for passwordless email login (issue #1328): the
   generator adds a `magic_link_token` model + `magic_link_tokens` table and the
   single-use, expiring login-link routes (do not name the auth resource
   `magic_link_token`).
```

**Magic-link account-lock recheck (trunk-dev)**: the generated `POST
/login/magic/verify` handler re-checks the account lock **(unreleased —
trunk-dev, issue #1777)** after consuming the single-use token and before
establishing a session, so a magic link minted before an account was locked can
no longer complete a login after lockout. The recheck is a fresh guarded
`UPDATE` on `locked_at` (same time-bounded `[auth.lockout]` cool-off semantics
as password login, gated on the same `lockout_enabled` predicate — a no-op when
lockout is disabled), re-reading the current lock state at the DB to close the
concurrent-lock TOCTOU. It sits before the TOTP branch, so it covers both
`--magic-link` and `--magic-link --totp`; a locked account renders the same
generic failure page as an expired/consumed/unknown token, so there is no
oracle.

### teams (trunk-dev)
```
Next steps:
1. `autumn generate teams` takes no name/model argument — it always emits the
   fixed Organization/Membership/Invitation set under src/teams/. Requires an
   existing `users` table (e.g. from `autumn generate auth`); it does not
   scaffold login/signup itself. Postgres only — rejects a SQLite-backed
   project up front with an actionable error.
2. Wire the three-line auth-integration seam by hand (see
   docs/generate-teams.md):
   - After your own signup handler creates the user, call
     `teams::routes::organizations::provision_default_organization(user.id, &mut db)`
     to make them the Owner of a personal organization (org+membership run in
     one transaction; wrap your own user insert in `db.tx(...)` too and call
     `provision_default_organization_on_conn` on the same `conn` instead if
     you need full 3-way atomicity with the account row).
   - After your own login handler resolves the session, call
     `teams::role::establish_org_session(...)` to set the active
     organization + role in the session.
   - Before (or as part of) your own account-deletion handler, call
     `teams::routes::organizations::remove_all_memberships(user.id, &mut db)`
     — `Membership` has no FK to your `users` table, so nothing else notices
     the account is gone; skipping this leaves a deleted user's team access
     live on any device with a still-valid session cookie.
3. Set `[tenancy]` in autumn.toml: `source = "session"`,
   `session_key = "organization_id"`, and list `/invite` (NOT `/invitations`)
   in `public_paths` — `/invite/{token}` is the invitee-facing accept flow;
   `/invitations` is the Admin-only create/revoke/resend surface and must
   stay tenant-gated.
4. Run: autumn migrate   (applies the organizations/memberships/invitations
   migration)
5. Generated forms already carry CSRF tokens — every mutating form embeds a
   hidden `_csrf` field sourced from an `Option<CsrfToken>` extractor, so no
   manual wiring is needed here even with the framework's on-by-default CSRF
   protection.
6. If you generated auth with a custom resource name (e.g.
   `autumn generate auth Account`, table `accounts` rather than the default
   `users`), edit `src/teams/routes/invitations.rs`'s `caller_email_lookup`
   module by hand to match — it hard-codes a `users` table name as a
   placeholder, since this generator can't introspect which name a separate,
   prior `auth` generation used.
7. `examples/teams` is the fully-wired reference this generator adapts
   from — it owns its own `users` table end to end, so all three integration
   points from step 2 are inlined directly into its `routes/auth.rs`.
```

## Flags

- `--api`: Generate JSON-only scaffold (no HTML views)
- `--oauth github,google`: For `auth` subcommand — add OAuth providers
- `--totp`: For `auth` — add TOTP two-factor auth
- `--passkeys`: For `auth` — add WebAuthn passkeys
- `--dry-run`: Print what would be written without touching the filesystem (supported by `wizard`)
- `--force`: Overwrite existing files without prompting (supported by `wizard`)
- `--live` **(trunk-dev)**: For `scaffold` — repository broadcasts, a `LiveFragment` impl, an SSE stream route, and an SSE-wired index list
- `--live-validation` **(trunk-dev)**: For `scaffold` — per-field inline validation endpoints with `hx-post` inputs (implies `--live`)
- `--unique FIELD` / `--index FIELD` / `--default field=variant` **(trunk-dev)**: Constraint/index/default markers (see field table)
- `--no-policy` **(trunk-dev)**: For `scaffold` — skip the default-generated record-level `Policy`/`Scope`. Ignored under `--api` (issue #1125)
- `--belongs-to <Parent>` **(trunk-dev)**: For `scaffold` — bind this resource to a parent as its child (issue #1323). Needs a matching `references` column (`--belongs-to Post` + `post:references`) and a parent that is **already scaffolded**. Emits `GET`/`POST /<parents>/{<fk>}/<children>` (the create takes the FK from the path, never the body), a `pub children_section(…)` helper, an injected children list + inline "add" form on the **parent's** generated `show` view, a back-link on the child's show view, and a cross-parent-isolation test. Marker-delimited, so re-runs are idempotent and `autumn destroy` reverses it. If the child has an owner column, the nested list inherits the flat index's `#[secured]` + owner scoping. Refused with `--api`/`--live`/`--live-validation`/`--sharded`, an `Attachment` column, a nullable or self-referential parent reference, or a parent that isn't scaffolded / is `slug`-keyed / carries a `:states(…)` column / has a hand-rewritten `show`. Single-level only
- `--searchable <field,field>` **(trunk-dev)**: For `scaffold` — make the named text fields Postgres full-text searchable. Emits `#[searchable]` attrs, a `search_vector` generated column + GIN index migration, and a search box wired to `GET /<plural>/search`. Rejected for non-text/unknown fields, uuid-PK models, and owner-scoped models; gated off under `--live`/`--live-validation` (issue #1319)
- `--i18n` **(trunk-dev)**: For `scaffold` — emit translatable views. Every user-facing string in the generated HTML (page titles, `h1`s, buttons, links, index column headers, show-page property labels, form control labels, enum options, empty states, the delete-confirm prompt, the flash notices, the media type/size line beside a stored attachment, and the labels the shared pager/bulk-delete/confirm widgets supply by default) becomes a `t!(locale, "key")` lookup; each view handler takes the `Locale` extractor as its **first** parameter (`FromRequestParts`, so it must precede the single body extractor); and the default locale's `.ftl` is created — or merged into, never rewriting a value already translated — with exactly the keys the views reference, so the generated app passes `autumn i18n check --strict`. Keys go to the locale `autumn.toml` actually names as default (`fr.ftl` for a `default_locale = "fr"` project). Also enables autumn-web's `i18n` feature, adds `[i18n] default_locale = "en"` when there is no `[i18n]` section, wires `.i18n_auto()` into the `AppBuilder` chain, and adds `COPY i18n` to both Dockerfile stages (`.i18n_auto()` PANICS at startup without the bundle). Shared chrome (`common.create`/`save`/`back`/`edit`/`delete`/`show`/`pagination`/…) is written once per project; each resource adds only its own strings (`post.new`, `post.field.<column>`, `post.flash.*`, …). Row keys and counts interpolate as Fluent arguments; the model NAME never does — "New Post" is a per-resource key, since a noun in a sentence pattern cannot be made to agree in gender or case from the bundle. Composes with `--searchable`, `--soft-delete`, `--sharded`, and the CSV export; a no-op under `--api`. Refused with `--live`/`--live-validation` (rows render outside a request, so no `Locale` in scope), `--belongs-to` (the nested section splices into the parent's own `show` handler), and a resource named `Common` (its keys would collide with the chrome namespace). The nesting refusal follows the relationship rather than the flag: a `--force --i18n` re-scaffold that omits `--belongs-to` is refused too, since the nesting is recovered from the parent's routes file — `autumn destroy scaffold` the child first to scaffold it flat. Without the flag, scaffold output is byte-for-byte unchanged. One behaviour note: one key per field serves the index header, show row, and form label alike, so a *multi-word* column's show label normalizes from "Author name" to "Author Name" (issue #1349)

## Wizard name constraints

The wizard subcommand has stricter naming rules than other generators:

- Wizard name and all step names: ASCII letters, digits, underscores only — no hyphens.
- Must start with a letter or `_` and must not be a Rust keyword.
- Minimum two steps.
- Step names `confirm`, `commit`, and `cancel` are reserved (auto-generated); using them causes a conflict error.
- Duplicate step names (after snake_case normalization) are rejected.
- PascalCase step names are accepted and converted to snake_case (`ShippingAddress` → `shipping_address`).

## Error handling

If `autumn generate` exits nonzero, show the stderr output and diagnose:
- File conflict: offer to show the conflicting file
- Missing autumn-cli: instruct `cargo install autumn-cli --version 0.5.0`
- Schema error: check `src/schema.rs` is up to date (`autumn migrate`)
