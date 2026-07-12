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
| `Attachment` | `JSONB NULL` (blob metadata) | `Option<Blob>` (always nullable) — **requires `storage` feature in Cargo.toml**; generator does not add it automatically |
| `Option<T>` | Nullable version of any above | `Option<T>` |
| `references` **(trunk-dev)** | `post:references` → `post_id BIGINT NOT NULL REFERENCES posts(id)` + auto index | `i64` (field name gets `_id` appended; `post:references?` for `Option<i64>`) |
| `enum{a,b,c}` **(trunk-dev)** | `TEXT` + `CHECK (col IN (...))` | Generated PascalCase Rust enum with Diesel/serde impls + `<select>` widget; `--default field=variant` sets SQL DEFAULT + `#[default]`. Quote the token in bash/zsh (brace expansion) |
| `decimal` / `decimal{10,2}` **(trunk-dev)** | `NUMERIC(12,2)` default, or explicit precision/scale | `rust_decimal::Decimal` (dependency added automatically); use for money, never `f64` |
| `:unique` modifier **(trunk-dev)** | `email:String:unique` → `CREATE UNIQUE INDEX` in the migration | Also generates a free `find_by_<field>` lookup and 23505 → 422 inline "already exists" form error. `--unique FIELD` is the flag equivalent |

**Foreign keys** (published 0.5.0 CLI only — no `references` token): scaffold
an `i64` field and hand-edit the generated migration to add
`REFERENCES users(id)` and an index. On trunk-dev, use `user:references`.

**Indexes/UNIQUE** (published 0.5.0 CLI only — no `:unique` token): add them
by hand in the generated migration's `up.sql`. On trunk-dev, use `:unique` /
`--unique FIELD` / `--index FIELD`.

**Do not use UUID as a primary key.** Primary keys are always `i64` /
`BIGSERIAL`. Use `Uuid` as a secondary column for external correlation only.

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
