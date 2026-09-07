# Code Generators

`autumn generate` collapses the five-file dance of "add a resource" into a
single command. Four subcommands cover the cases you actually hit:

| Command                              | What it produces                                                                 |
| ------------------------------------ | -------------------------------------------------------------------------------- |
| `autumn generate model`              | A `#[model]` struct, a Diesel `up.sql`/`down.sql` pair, a `schema.rs` entry      |
| `autumn generate migration`          | A Diesel migration directory; columns are inferred when the name matches a verb |
| `autumn generate task`               | A one-off operational `#[task]` skeleton under `tasks/`                         |
| `autumn generate job`                | A `#[job]` background-job handler with args struct, `registered_jobs()` aggregator, and `.jobs(…)` wiring in `src/main.rs` |
| `autumn generate channel`            | A real-time broadcast channel over the `Channels` API — an htmx SSE live view by default, or a raw `#[ws]` handler with `--ws` |
| `autumn generate webhook`            | A signature-verified, replay-protected inbound provider webhook (Stripe/GitHub/Slack/generic) — handler, event dispatch, `autumn.toml` endpoint config, and tests |
| `autumn generate scaffold`           | Everything `model` does plus `#[repository]`, HTML routes, smoke test, `routes![]` registration |
| `autumn generate wizard`             | A session-backed multi-step form wizard with per-step validation and a confirm/commit/cancel flow |
| `autumn generate admin`              | An `AdminModel` adapter for an existing model, wired to `autumn-admin-plugin`   |
| `autumn generate tauri`              | A complete `src-tauri/` sidecar project so the app ships as a native desktop installer (see [Tauri guide](tauri.md)) |

The generators only emit code that uses macros and conventions Autumn already
ships (`#[model]`, `#[repository]`, `#[get]/#[post]`, the `i64`-PK convention,
Diesel migrations, Maud templates). They never introduce new traits or
runtime concepts — once a generator has run, the generated files are
ordinary user code that you should edit freely.

## Five commands to a working CRUD app

This is the path that every other batteries-included framework boasts about.
On a fresh machine with Rust and Postgres installed, there is one one-time
prerequisite: `autumn migrate` delegates to the Diesel CLI, so install it
once with `cargo install diesel_cli --no-default-features --features postgres`.

```bash
autumn new my-app
cd my-app
autumn generate scaffold Post title:String body:Text published:bool
# Before migrating: configure the database (see the note below) and
# create it if it does not exist yet:
autumn db create
autumn migrate
autumn dev
```

One file edit belongs between `generate` and `migrate`: the generated
`autumn.toml` ships with the database section commented out (look for
"Uncomment to configure database:"). Uncomment it and point `url` at your
Postgres so both `autumn migrate` and the running app can reach the
database — without it, `autumn migrate` exits with `✗ No database URL found.`.
`autumn migrate` runs migrations against that database but does not create
it, hence `autumn db create` above — it reads the same `url`/`primary_url`
you just configured and creates that database, idempotently, with no extra
tooling beyond what `autumn migrate` already needs (an external `createdb` or
`CREATE DATABASE` in psql works too, if you have a Postgres client installed):

```toml
[database]
url = "postgres://user:pass@localhost:5432/my_app"
```

Visit <http://localhost:3000/posts> to see the generated index page.
The JSON endpoint at <http://localhost:3000/api/posts> returns `[]` until
rows exist; mount mutating API handlers only after adding a repository policy.

## The field-type DSL

Fields are passed as `name:Type` tokens. Only the documented public surface
is supported — anything else fails with an error that lists the supported
set.

| DSL token         | Rust type                       | Schema type        | SQL type            |
| ----------------- | ------------------------------- | ------------------ | ------------------- |
| `title:String`    | `String`                        | `Text`             | `TEXT`              |
| `body:Text`       | `String` (alias for `String`)   | `Text`             | `TEXT`              |
| `body:richtext`   | `String` (Markdown source)      | `Text`             | `TEXT`              |
| `count:i32`       | `i32`                           | `Int4`             | `INTEGER`           |
| `count:i64`       | `i64`                           | `Int8`             | `BIGINT`            |
| `score:f32`       | `f32`                           | `Float4`           | `REAL`              |
| `score:f64`       | `f64`                           | `Float8`           | `DOUBLE PRECISION`  |
| `published:bool`  | `bool`                          | `Bool`             | `BOOLEAN`           |
| `token:Uuid`      | `uuid::Uuid`                    | `Uuid`             | `UUID`              |
| `at:NaiveDateTime`| `chrono::NaiveDateTime`         | `Timestamp`        | `TIMESTAMP`         |
| `at:DateTime`     | `chrono::DateTime<chrono::Utc>` | `Timestamptz`      | `TIMESTAMPTZ`       |
| `data:Bytea` *(or `Vec<u8>`)* | `Vec<u8>`           | `Bytea`            | `BYTEA`             |
| `post:references` | `i64`                           | `Int8`             | `BIGINT`            |
| `slug:slug{from:title}` | `String`                  | `Text`              | `TEXT`              |
| `config:json` *(or `jsonb`)* | `serde_json::Value`    | `Jsonb`             | `JSONB`              |
| `rank:position` *(or `position{scope:col}`)* | `i64` (server-managed) | `Int8`  | `BIGINT`            |
| `comments:commentable` | `i64` (server-managed `comment_count`) | `Int8` | `BIGINT`            |

Wrap any of the above in `Option<…>` to make the column nullable
(`Option<String>`, `Option<i64>`, `Option<NaiveDateTime>`, …). The generator
emits both `NULL` in the migration SQL and `Nullable<T>` in `schema.rs`.

### `json`/`jsonb` — flexible structured data

`config:json` (or `config:jsonb`, `Json`, `Jsonb` — the lowercase and PascalCase
spelling of each alias are both accepted, not arbitrary casing)
stores arbitrary structured data as a Postgres `JSONB` column, typed as a bare
`serde_json::Value` in the generated model — no wrapper struct, so it composes
directly with `serde_json::json!(...)`, `Value::get`, etc. The generated
create/edit form renders a `<textarea>` and parses the submitted text as JSON
on write; invalid JSON is a `400`, not a `500`, and leaving an *optional*
`Option<json>` textarea blank stores `NULL` rather than failing to parse. The
JSON API round-trips the field as a native object/array, not a stringified
blob. On the `SQLite` dev/test backend the column is `TEXT` (plain-text JSON) —
diesel provides the `serde_json::Value` conversion on both backends directly,
so this needs no `--sqlite`-specific caveat the way `Uuid`/`decimal` do.

```bash
autumn generate scaffold Setting name:String config:json
```

### `richtext` — safe user-submitted Markdown

`body:richtext` is storage-identical to `body:Text` (a plain `TEXT` column
holding the Markdown **source**, never rendered HTML). What it changes is the
generated UI:

- the form renders a Markdown editor with a no-JavaScript syntax toolbar and an
  htmx live preview instead of a bare `<textarea>`;
- a `POST /{plural}/preview/{field}` endpoint drives that preview;
- the show view renders the column through
  `autumn_web::markdown::render_user_content`, which disables raw-HTML
  passthrough and runs the output through an allowlist sanitizer;
- `autumn-web`'s `markdown` feature is enabled on your project.

```bash
autumn generate scaffold Post title:String body:richtext
```

That is the whole setup — the resulting app accepts formatted user content
without stored XSS. See the [rich text guide](rich-text.md) for the exact
sanitization guarantee, the tag/URL-scheme allowlist, and what it deliberately
excludes.

`{min=N,max=N}` length bounds are accepted on a `richtext` column and emit the
same server-side `#[validate(length(…))]` rule as `Text` — but no client-side
`minlength`/`maxlength`, since the editor is rendered by `rich_text_area`, which
takes no HTML5 constraint attributes. `{email}` and `{url}` are rejected: a
Markdown body cannot satisfy a single-line format validator, so accepting them
would emit a field no submission could fill. `--api` scaffolds ignore the rich-text wiring entirely —
they render no form or show view, so the column is just `TEXT` carried out over
JSON.

### Validation and HTML5 constraints (`{…}` modifiers)

Add a trailing `{…}` block to a field to declare constraints once and have
them enforced on **both** sides — a server-side `#[validate(...)]` rule *and*
the matching client-side HTML5 input attribute:

```bash
autumn generate scaffold Post \
  'title:String{min=3,max=120}' \
  'contact:String{email}' \
  'homepage:String{url}' \
  'age:i32{min=0,max=130}'
```

| Modifier                        | Applies to        | `#[validate(…)]`        | HTML5 attribute(s)        |
| ------------------------------- | ----------------- | ----------------------- | ------------------------- |
| `{min=N,max=N}` (String/Text)   | `String`/`Text`   | `length(min, max)`      | `minlength` / `maxlength` |
| `{min=N,max=N}` (richtext)      | `richtext`        | `length(min, max)`      | *(none — server-side only)* |
| `{min=N,max=N}` (numeric)       | `i32`/`i64`/`f32`/`f64` | `range(min, max)` | `min` / `max` (`type="number"`) |
| `{email}`                       | `String`/`Text`   | `email`                 | `type="email"`            |
| `{url}`                         | `String`/`Text`   | `url`                   | `type="url"`              |

(The `{encrypted}` modifier below shares this block but is not a validator —
see [Encrypted columns](#encrypted-columns-with-encrypted).)

The generated model field carries the `#[validate(...)]` attribute (so a bad
submission is rejected through the existing changeset path as a **422 with
inline per-field errors**, never a 500 or a silent store), and the generated
form input carries the matching HTML5 attribute (so the browser blocks bad
input before it hits the network). The `required` signal from a non-nullable
column is preserved, and a rejected submission re-renders keeping the entered
values. A misspelled modifier (e.g. `{maxx=5}`) fails the scaffold with an
error naming the offending token. Quote the whole token in bash/zsh so the
shell doesn't brace-expand the comma.

### Encrypted columns with `{encrypted}`

Declare a column encrypted at rest in one token — no hand-edit of the generated
model, no forgetting:

```bash
autumn generate scaffold Account \
  username:String \
  'api_token:String{encrypted}' \
  'email:String{encrypted:deterministic}'
```

`{encrypted}` puts `#[encrypted]` on the generated model field, so the column is
stored as an opaque base64 AES-256-GCM envelope while staying a plain `String`
in your Rust code. The migration column is unbounded `TEXT` — sized for the
envelope, never the plaintext — and the generated admin **redacts** the column
automatically, because the admin generator reads the very attribute this token
emits. See [attribute encryption](attribute-encryption.md) for the envelope
format, key rotation, and the randomized/deterministic tradeoff.

| Modifier                    | Emits                          | Equality lookups |
| --------------------------- | ------------------------------ | ---------------- |
| `{encrypted}`               | `#[encrypted]`                 | ✗ (fresh nonce per write) |
| `{encrypted:deterministic}` | `#[encrypted(deterministic)]`  | ✓ (`find_by`/`exists_by`, `UNIQUE`) |

Randomized is the default and the safe choice. Reach for `deterministic` only
when the column genuinely needs lookups, and never on a low-entropy column —
stable ciphertext lets an observer of the database tell which rows share a
value.

Because randomized ciphertext can never match an equality predicate, the
generator **refuses** to pair `{encrypted}` with anything that would perform
one, rather than emitting code that fails at runtime with
`EncryptionError::RandomizedEqualityLookup`:

```bash
autumn generate scaffold Account 'api_token:String{encrypted}:unique'
# ✗ field 'api_token' is `unique`, but `{encrypted}` is randomized: … Declare it
#   `{encrypted:deterministic}` to support `find_by`/`exists_by` equality lookups …
```

That refusal covers `:unique`/`--unique`, `--query find_by_x:x`, and `--index`
— switching to `{encrypted:deterministic}` is the fix for all three. A second
set of refusals applies to **both** modes, because no mode makes them work:
`--searchable` (full-text search indexes the stored ciphertext, so plaintext
searches never match), `--default` (a defaulted column bypasses the insert path
that encrypts), `--shard-key` (the shard is chosen by hashing the stored
value), `Option<…>`, a non-text field kind, a `:states(…)` state machine, and
deriving a `slug{from:…}` from an encrypted column — a slug is stored in its
own plaintext column *and* used as the record's URL.

### What the generated app does and does not hide

`#[encrypted]` protects data **at rest**. In Rust the column is a plain
`String`, so what the generated app *renders* is a separate choice, and the
scaffold makes these:

- the **index table** shows `••••••••`, with no sort link — a list is a
  bulk-disclosure surface, and ordering by envelope bytes is meaningless;
- the **`show` view and the `edit` form** render the real value: you routed to
  one record deliberately, and a form has to show what it is editing;
- the **CSV export** omits the column entirely, matching the admin panel's own
  export — a downloaded file leaves the app;
- the **admin** redacts it (`••••••••`) and will not sort by it — and
  `autumn generate admin` refuses a `{encrypted}` token whose model field has no
  `#[encrypted]` attribute, since redacting a column the model stores in
  plaintext would only hide that fact;
- an `--api` scaffold **omits it from JSON responses** — `#[encrypted]` implies
  `#[serde(skip_serializing)]` unless the field opts in with
  `#[encrypted(admin_visible)]`. The column can still be *written* over the API.

All of that is ordinary generated code; edit any of it if your app wants
different behavior. One surface the generator cannot guard is a runtime
`?filter[col]=` / `?sort=` query naming an encrypted column: it matches nothing
and orders by ciphertext.

One requirement remains yours: key material. `autumn generate` prints the exact
next step, and the app fails fast in production without it:

```bash
autumn credentials edit
# [active_record_encryption]
# primary_key         = "<openssl rand -hex 32>"
# key_derivation_salt = "<openssl rand -hex 16>"
# deterministic_key   = "<openssl rand -hex 32>"   # only for {encrypted:deterministic}
```

`{encrypted}` applies to `String`/`Text` columns only, and never to a nullable
one — `#[encrypted]` supports non-null `String` columns in v1. To encrypt a
column that **already exists** (with data in it), use the backfill migration
instead: `autumn generate migration EncryptApiTokenOnAccounts`.

Every generated table also includes:

- `id BIGSERIAL PRIMARY KEY` (the `i64`-PK convention used everywhere else
  in Autumn).
- `created_at TIMESTAMP NOT NULL DEFAULT NOW()` annotated `#[default]` on
  the model so it stays out of `NewX`.

### Foreign keys with `references`

`post:references` scaffolds a foreign-key column: the declared name is
rewritten to end in `_id` (`post` -> `post_id`), the referenced table is
derived by pluralising the base name (`post` -> `posts`, matching
`naming::pluralize`), and the column is emitted as
`post_id BIGINT NOT NULL REFERENCES posts(id)` with an automatic index
(`CREATE INDEX idx_comments_post_id ON comments (post_id);`) — no `--index`
flag required. Append `?` for a nullable foreign key
(`post:references?` -> `post_id: Option<i64>`, column `NULL`):

```bash
autumn generate scaffold Comment body:Text post:references
```

If the referenced model doesn't exist yet (no `src/models/post.rs`, or a
matching declaration in a single-file `src/models.rs`), the generator still
scaffolds the column, constraint, and index — it just prints a warning that
the referenced table is assumed to already exist.

#### belongs_to dropdowns (populated from the parent)

When the referenced model *does* exist, the scaffolded new/edit form renders
the foreign key as a **populated `<select>`** — one `<option>` per parent row
— instead of a text box demanding a raw numeric id, and the index/show views
render the parent's **display value** rather than the raw `*_id` integer. No
hand-editing required:

```bash
autumn generate model Post title:String
autumn generate scaffold Comment body:Text post:references
# → the new-comment form's "post" field is a dropdown of existing posts,
#   labeled by each post's title; the comment index/show show the post title.
```

The **display column** is chosen by heuristic: a `name` or `title` column if
present, otherwise the first `String`/`Text` column, falling back to the id
only when the parent has no string column. Override it explicitly with a
`{label:col}` modifier:

```bash
autumn generate scaffold Comment body:Text 'post:references{label:slug}'
```

A nullable reference (`post:references?`) renders a blank "— Unset —" first
option so the selection can be cleared, and its index/show views render a dash
when unset. (Index/show use a simple per-view fetch; the N+1-safe batched
variant is issue #835.)

`references` only supports the i64/BIGSERIAL primary-key convention. If the
referenced model *is* found but was generated with `--id uuid`, the generator
fails fast with an error instead of emitting a migration that would break at
`autumn migrate` time with a `BIGINT`-vs-`UUID` type mismatch — hand-write
the migration for a UUID foreign key instead.

Composite foreign keys, cascade policy (`ON DELETE`/`ON UPDATE`), and runtime
association traversal (`belongs_to`/`has_many`) are not in scope for this
token — see issue #835 for the latter.

#### Nested resources with `--belongs-to`

`references` gives the child a foreign key and a parent dropdown on its *own*
form. `--belongs-to` adds the half a flat scaffold has always left to you: the
**parent's** show page listing its children with an inline "add" form, and the
nested routes behind it.

```bash
autumn generate scaffold Post title:String
autumn generate scaffold Comment body:Text post:references --belongs-to Post
```

On top of the usual flat CRUD you get:

| Generated | What it does |
| --- | --- |
| `GET /posts/{post_id}/comments` | The child list for one parent, paginated by the same `PageRequest` extractor the flat index uses (`?page=N&size=M`). A `post_id` that doesn't exist answers **404**, not an empty list. |
| `POST /posts/{post_id}/comments` | A `#[secured]` create whose foreign key comes from the **path**. Invalid input re-renders at 422 with inline errors and preserved values; success redirects (PRG) to the parent's show page. |
| `pub async fn children_section(…)` in `src/routes/comments.rs` | The child list (a `data_table`, each row linking to the child's own show view) plus the inline create form. Public on purpose — call it from any hand-written page too. |
| An edit to `src/routes/posts.rs` | The parent's generated `show` view now renders that section. |
| A back-link on the child's show view | `Back to Post`, closing the loop. |
| A test in `tests/comment.rs` | Create a child under a parent → it appears in *that* parent's list → it does **not** appear under a different parent. |

If the child carries an owner column (`user_id`/`author_id`/`owner_id`), the
nested list inherits the flat index's owner scoping: it is `#[secured]` and
filtered to the signed-in user's own rows, so nesting can never open a second,
wider door onto the same table. Widen it deliberately (drop the filter in
`children_section_with`) if the children really are public.

The nested create runs the same context-only `authorize_create::<Child>` the
flat create runs — it does **not** authorize the *parent*. If attaching a child
should depend on who owns the post, add that check to the generated handler; the
policy has no parent-aware hook.

The parent foreign key is deliberately **not** an editable control on the nested
form: the parent is the URL. The handler overwrites the column from the path
before it validates, so a hand-crafted body carrying its own `post_id` cannot
re-parent a comment. (The child's own `/comments/new` form still shows the
belongs_to dropdown — that is where choosing a parent belongs.)

The child list is deliberately **not** sortable: unlike the flat index it runs a
fixed `ORDER BY id DESC` and extracts no `ListQuery`, so advertising sortable
headers would render links that reload the identical list. It is also a
hand-written parent-scoped query rather than a repository call — the repository
has no "children of X" method — so anything the repository layer applies for
free on the flat index has to be spelled out in `children_section_with`
(soft-delete and owner scoping already are).

Because the markers record the relationship durably, `--belongs-to` is a
**one-time** flag: re-running `generate … --force` without it keeps the nesting
(and says so in a warning) rather than half-dismantling it, and `autumn destroy`
finds the parent without it.

Regenerating the **parent** is safe: `autumn generate scaffold Post … --force`
re-applies every child section it was carrying onto the fresh render, so the
children list and the markers survive. Destroying a parent that still has nested
children is refused (destroy the children first — that removes the section from
the parent as it goes).

Changing or dropping the parent is refused, before anything is written. The
parent's section hands `children_section` its own `row.id`, so re-pointing the
child at a different parent would leave that call compiling but reading the
wrong table's ids — a post's page listing whichever *user* shares its id.
Un-nest first, then re-nest:

```bash
autumn destroy scaffold Comment body:Text post:references user:references --belongs-to Post
autumn generate scaffold Comment body:Text post:references user:references --belongs-to User
```

The edits to the parent's routes file are marker-delimited
(`// autumn:nested:comments`), so re-running the generator never double-injects
and `autumn destroy scaffold Comment body:Text post:references --belongs-to Post`
takes exactly those lines back out — including when one parent has several
nested children, and including after `cargo fmt` has reflowed them. Destroy also
finds the parent from the markers alone, so forgetting to repeat `--belongs-to`
still leaves a compiling project.

Refused at generation time, with an actionable message, when combined with
`--api`, `--live`, `--live-validation`, `--sharded`, an `Attachment` column, a
nullable parent reference (`post:references?`), or a self-referential parent —
and when the **parent** isn't a shape the injection can patch: not scaffolded
yet, `slug`-keyed (`slug:slug{from:title}`), carrying a `:states(…)` column, or
with a hand-rewritten `show` view. In those cases scaffold the child without
`--belongs-to` — the `references` column and its dropdown still work — and write
the parent-scoped list by hand. Nesting is single-level: `/a/{id}/b` but
never `/a/{id}/b/{id}/c`. Many-to-many joins, polymorphic associations, and
counter-cache columns stay hand-written.

### Human-readable URLs with `slug:slug{from:...}`

`slug:slug{from:title}` gives a model a clean, shareable URL
(`/posts/why-rust-wins`) instead of the default `/posts/42` — a single DSL
token, zero hand-edits, composing with the existing `unique` (issue #1032)
and `references` (issue #1026) machinery rather than introducing a parallel
system:

```bash
autumn generate scaffold Post title:String body:Text slug:slug{from:title}
```

This wires up, end to end:

- **A public `autumn_web::slugify(&str) -> String` helper.** Lowercases,
  best-effort ASCII-folds accented Latin characters (`"café"` ->
  `"cafe"`), and treats everything else (punctuation, whitespace, un-folded
  non-Latin script) as a separator, collapsing runs to a single `-` and
  trimming leading/trailing `-`. An input that slugifies to nothing (empty,
  or entirely punctuation/non-Latin) falls back to a stable, deterministic
  non-empty token rather than ever returning `""`.

  Because of that fallback, **`slugify(input).is_empty()` is always `false`**
  and can never be used as a validation check. To reject input that carries no
  text at all — a post title of `"***"` or `"🎉🔥💯"` — use
  `autumn_web::contains_letter_or_number(&str) -> bool`, which asks whether the
  input holds at least one letter or number *in any script*. It deliberately
  accepts `"日本語"`: that has no ASCII fold and gets the hash fallback for its
  URL segment, but it is real text the author typed, and the fallback exists so
  such a page is still reachable. The generated scaffold does **not** apply
  this check — it only derives the slug — so wire it into your own form
  validation if a content-free source value matters to you. See
  [Forms](forms.md#changesetformt--for-pages-a-human-is-looking-at).
- **A `NOT NULL` column with its own `UNIQUE INDEX`** in the migration — a
  `slug` field is implicitly `unique`, so it reuses the exact `CREATE UNIQUE
  INDEX` codegen a `:unique` modifier produces on any other field.
- **A `find_by_slug(&str)` repository lookup** — again for free, from the
  same "every `unique` field gets a `find_by_<field>`" machinery `:unique`
  already triggers.
- **Auto-derivation on create.** When the submitted slug is blank, the
  `create` handler derives it from the `from` field via `slugify`, then
  probes for a collision and appends a deterministic `-2`, `-3`, ... suffix
  until it finds a free value — so two posts titled "Hello" get distinct
  slugs (`hello`, `hello-2`) instead of a 422 on the unique index. The
  generated form exposes the slug as a plain, optional text input (no
  live-preview JS); a non-blank submission is used as-is and still goes
  through the normal unique-violation handling on conflict.
- **Slug-keyed `show`/`edit`/`update`/`delete` routes.** `GET /posts/{slug}`
  (and its edit/update/delete siblings) resolve the record by slug instead of
  `id`, 404ing through the same `AutumnResult`/`AutumnError::not_found` path
  every other lookup uses. Every generated view, redirect, and `paths::`
  helper for the resource links through the slug, never the numeric id — an
  `rg "/\{id\}"` over a slug-bearing scaffold's HTML routes returns zero
  hits. (The auto-generated JSON REST API under `/api/...` stays id-keyed;
  rekeying it is out of scope for this token.)
- **A model has at most one `slug` field** (it's the resource's routing key)
  and it always needs the `{from:<field>}` modifier, naming a declared
  `String`/`Text`/`richtext` field to derive from — either error is caught
  at generate time with a message naming the problem.
- A slug field is not yet supported together with `--live`/
  `--live-validation`, `--sharded`, an `Attachment` field, or a field
  carrying a `:states(...)` state machine — each of those combinations is
  rejected at generate time rather than silently emitting routes that still
  key off `id`. A non-slug scaffold (the overwhelming common case) is
  completely unaffected: output stays byte-for-byte identical.
- Renaming the source field after the fact does not retroactively rename
  existing slugs, and there is no `slug_history`/301-redirect for a stale
  slug after a manual edit — both are follow-up work, not this slice.

## `autumn generate model`

```bash
autumn generate model Post title:String body:Text published:bool
```

Produces:

```
src/models/post.rs                              # #[model] struct
src/models/mod.rs                               # `pub mod post;` (created or appended)
src/schema.rs                                   # diesel::table! { posts (id) { ... } }
migrations/<timestamp>_create_posts/up.sql      # CREATE TABLE posts (...)
migrations/<timestamp>_create_posts/down.sql    # DROP TABLE posts;
```

The generated `src/models/post.rs`:

```rust
//! Generated by `autumn generate`.

use crate::schema::posts;

#[autumn_web::model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub body: String,
    pub published: bool,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}
```

| Generated file                   | Existing concept it maps to                            |
| -------------------------------- | ------------------------------------------------------ |
| `src/models/post.rs`             | The [`#[autumn_web::model]`](../../autumn-macros/src/model.rs) macro |
| `migrations/.../up.sql`          | Diesel migrations consumed by [`autumn migrate`](../../autumn-cli/src/migrate.rs) |
| `src/schema.rs`                  | The Diesel `table!` block referenced by `#[model]`     |
| `src/models/mod.rs`              | Standard Rust module aggregator                        |

## `autumn generate migration`

For schema changes that aren't a brand-new table.

```bash
# Empty migration — you fill in the SQL.
autumn generate migration BackfillSomething

# AddXxxToYyy — emits ALTER TABLE yyys ADD COLUMN per field
autumn generate migration AddPublishedToPosts published:bool

# RemoveXxxFromYyy — emits ALTER TABLE yyys DROP COLUMN per field
autumn generate migration RemoveBodyFromPosts body:String
```

The name detection is purely cosmetic — Autumn treats both `Post` and
`Posts` as the table `posts`. If your name doesn't match `Add…To…` or
`Remove…From…`, the generator just emits empty `up.sql` and `down.sql`
files for you to fill in.

### Always generate — never hand-create the directory

Reach for the generator even when you intend to write every line of SQL
yourself. The one thing it does that you cannot easily do by hand is pick the
version.

Your app's migrations, the framework's, and every plugin's are applied into
**one shared version space**. Diesel records what it has applied by the
14-digit version in `__diesel_schema_migrations`, and Autumn's
`autumn_migration_checksums` table makes that version a `PRIMARY KEY`. Two
migrations claiming the same version are therefore not two migrations — one of
them silently never runs, and you find out when a column you were sure you
added isn't there.

Hand-written versions collide because everyone reaches for the same number. A
human typing a date pads it out to `20260831000000`; the next person to touch
the tree that day types the identical string. The generators mint a full
`YYYYMMDDHHMMSS` from the wall clock instead, so two authors collide only if
they generate in the same second:

```bash
autumn generate migration BackfillSomething      # → 20260831191622_backfill_something
autumn schema diff --write-migration --name x    # → same convention
```

If you have already created a directory by hand, rename it to a real
timestamp before committing:

```bash
date -u +%Y%m%d%H%M%S
```

CI enforces this (`scripts/check-migration-versions.sh`): a migration whose
time component is `000000`, whose digits aren't a real UTC timestamp, or whose
version is already claimed by another migration fails the build.

One caveat if you are fixing an existing migration: **never rename one that
has already been applied.** The version is how the framework knows it ran, so
renaming it makes the framework consider it unapplied and run it a second
time. Migrations already in that state are grandfathered in
`scripts/migration-version-baseline.txt` rather than corrected.

### Generated safety comments

When `autumn generate migration` produces SQL that could be dangerous for a
rolling deploy, it prepends an `-- autumn-safety:` comment to the statement:

```sql
-- autumn-safety: potentially-blocking
ALTER TABLE posts ADD COLUMN score INTEGER NOT NULL;
```

```sql
-- autumn-safety: destructive
ALTER TABLE posts DROP COLUMN body;
```

These comments are purely informational; they do not change runtime behavior.
`autumn migrate check` strips them before classifying statements so they do not
produce duplicate findings.

### Expand/contract: safe column rename or removal

The naive approach — `autumn generate migration RenameBodyToContent` then hand-
editing the SQL to `RENAME COLUMN body TO content` — produces an `irreversible`
finding from `autumn migrate check` because old replicas still running the prior
code will error on any query that references the old name.

The expand/contract pattern splits the change into two consecutive deploys:

**Step 1 — Expand** (add the new column alongside the old one):

```bash
autumn generate migration AddContentToPosts content:String
```

Edit the generated `up.sql` to copy existing data:

```sql
ALTER TABLE posts ADD COLUMN content TEXT;
UPDATE posts SET content = body WHERE content IS NULL;
```

Deploy this. All replicas now see both `body` and `content`. Update application
code to dual-write both columns and read from `content`.

**Step 2 — Contract** (remove the old column once all replicas run the new code):

```bash
autumn generate migration RemoveBodyFromPosts body:String
```

The generated `up.sql` will contain:

```sql
-- autumn-safety: destructive
ALTER TABLE posts DROP COLUMN body;
```

Run `autumn migrate check` — the finding will now be `destructive`, not
`irreversible`, because the column rename is already complete. This migration is
safe to apply because no running code references `body` any longer.

The same two-step pattern applies to column type changes and to removing columns
with foreign-key references.

## Rolling back with `autumn migrate down`

Every `autumn generate migration` run creates a `down.sql` file alongside
`up.sql`. `autumn migrate down` is the command that honours it.

```bash
# Revert the most recently applied user migration (default: --steps 1):
autumn migrate down

# Revert the last 3 user migrations in newest-first order:
autumn migrate down --steps 3

# Revert user migrations until 20260101000000 is the latest applied.
# VERSION must be a currently applied user migration; framework migrations are
# forward-only and cannot be used as a boundary.
autumn migrate down --to 20260101000000

# Required when AUTUMN_ENV=prod:
autumn migrate down --yes-i-mean-prod

# Enable maintenance mode around the rollback, then disable it on success:
autumn migrate --with-maintenance down
```

### Framework migrations are forward-only

Framework-owned migrations (the ones Autumn ships internally) are **never**
rolled back by `autumn migrate down`. They are listed separately in
`autumn migrate status` and have no `down.sql`. This design is intentional —
rolling back framework schema changes would break the framework features that
depend on them.

### Safety guards

Before touching the database, `autumn migrate down` checks:

1. **Production guard** — If `AUTUMN_ENV` is `prod` or `production`, the command
   refuses unless `--yes-i-mean-prod` is passed. (An empty `AUTUMN_ENV` falls
   back to the legacy `AUTUMN_PROFILE`.)
2. **down.sql preflight** — Every migration in the plan must have a non-empty,
   non-comment `down.sql`. If any are missing or blank, the command names them
   and exits non-zero without touching the database. A migration recorded as
   applied but no longer present locally is also surfaced as non-revertable
   rather than silently skipped.

Listing the applied migrations, building the plan, and reverting all happen
while the migration advisory lock is held, so two concurrent `down` runs are
serialized and neither double-reverts.

### Sharded deployments

Like `autumn migrate`, `autumn migrate down` operates on the control
database plus every configured shard by default, and honours `--shard <name>`
and `--control-only` to scope to a single target. Targets are rolled back in
order and the command is **fail-fast**: if a later target fails (for example a
runtime `down.sql` error, or shards sitting at divergent migration states), the
earlier targets have already been rolled back. Re-running `down` then plans
from each target's current state, so scope the command with `--shard` /
`--control-only` when you need to roll a single database back in isolation. (A
missing or empty `down.sql` is caught by preflight before any target is
mutated, since all targets share one `migrations/` directory.)

`autumn migrate check` also classifies `down.sql` files (in addition to
`up.sql`), so you can catch unsafe rollback SQL — such as `DROP TABLE` or a
`DROP INDEX CONCURRENTLY` that is missing its `run_in_transaction = false`
opt-out — before an incident.

### Observability

`autumn migrate status` shows rollback availability for every applied user
migration:

```
  ✓ 20260101000000_create_posts
  ✗ 20260102000000_add_body_to_posts  (no executable down.sql — not revertable)
```

This makes the rollback path visible before an incident, so you know which
migrations can be safely reverted.

## `autumn generate task`

For operational scripts that should run through the full Autumn app context.

```bash
autumn generate task cleanup_users
```

Produces:

```
tasks/cleanup_users.rs                         # #[task] async function skeleton
```

The generated task uses `TaskArgs<T>` for CLI flags:

```rust
#[derive(Debug, Deserialize)]
struct CleanupUsersArgs {
    #[serde(default)]
    pub dry_run: bool,
}

#[autumn_web::task]
pub async fn cleanup_users(TaskArgs(args): TaskArgs<CleanupUsersArgs>) -> AutumnResult<()> {
    // ...
    Ok(())
}
```

Register the function with `.one_off_tasks(one_off_tasks![...])` before running
it with `autumn task cleanup_users --dry-run`.

## `autumn generate job`

For background work that should survive process restarts, be retried on failure,
and be visible in `/actuator/jobs`.

```bash
autumn generate job SendWelcomeEmail user_id:i64 email:String
```

Produces:

```
src/jobs/send_welcome_email.rs    # #[job] handler + SendWelcomeEmailArgs struct
src/jobs/mod.rs                   # registered_jobs() aggregator (created or appended)
src/main.rs                       # mod jobs; + .jobs(jobs::registered_jobs()) added in place
```

The generated `src/jobs/send_welcome_email.rs`:

```rust
use autumn_web::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendWelcomeEmailArgs {
    pub user_id: i64,
    pub email: String,
}

#[job(name = "send_welcome_email", max_attempts = 5, backoff_ms = 500)]
pub async fn send_welcome_email(
    _state: AppState,
    args: SendWelcomeEmailArgs,
) -> AutumnResult<()> {
    // TODO: implement send_welcome_email
    let _ = args;
    Ok(())
}
```

The `#[job]` macro generates a companion struct `SendWelcomeEmailJob` with:
- `SendWelcomeEmailJob::NAME` — the job's registered name (`"send_welcome_email"`).
- `SendWelcomeEmailJob::enqueue(args).await?` — at-least-once enqueue (use from most handlers).
- `autumn_web::job::enqueue_on_conn(SendWelcomeEmailJob::NAME, args, conn).await?` — transactional enqueue (enqueues only if the surrounding DB transaction commits; use when the job outcome must be atomic with a DB write).

The generated `src/jobs/mod.rs` aggregator:

```rust
pub mod send_welcome_email;

#[must_use]
pub fn registered_jobs() -> Vec<autumn_web::job::JobInfo> {
    autumn_web::jobs![send_welcome_email::send_welcome_email]
}
```

Running `autumn generate job` a second time with a different name augments `mod.rs` and `registered_jobs()` in place — it never duplicates an entry.

The `.jobs(jobs::registered_jobs())` call added to `src/main.rs` wires the aggregator into the job runtime and automatically populates `/actuator/jobs` with every registered job.

### Slow live job verification

```bash
cargo test -p autumn-cli --test generate generated_job_cargo_checks -- --ignored --exact
```

This scaffolds a fresh project, runs `autumn generate job`, and asserts that `cargo check --tests` passes with no hand-editing required.

## `autumn generate channel`

For a live feature (chat, notifications, a live-updating list) built entirely
on Autumn's existing realtime stack — the `Channels` pub/sub API, SSE, and
`#[ws]` upgrade routes. No new transport is invented; the generator only
wires up what already ships.

```bash
autumn generate channel Chat
```

Produces:

```
src/channels/chat.rs      # GET /chat (live view), GET /chat/events (SSE), POST /chat/messages
src/channels/mod.rs       # pub mod chat; (created or appended)
src/main.rs               # mod channels; + routes![...] entries added in place
tests/chat_channel.rs     # smoke test: publishes a message, asserts a subscriber receives it
```

SSE-over-htmx is the default transport — `GET /chat` renders a view wired to
htmx's `sse-connect`/`sse-swap`, so browser tabs update live with **zero
client JS authored by the user**:

```rust
#[get("/chat/events")]
pub async fn chat_events(State(state): State<AppState>) -> impl IntoResponse {
    autumn_web::sse::stream(&state, TOPIC)
}

#[post("/chat/messages")]
pub async fn chat_publish(
    State(state): State<AppState>,
    Form(form): Form<ChatForm>,
) -> AutumnResult<&'static str> {
    let fragment = message_fragment(&form.message).into_string();
    state.broadcast().publish(TOPIC, fragment)?;
    Ok("published")
}
```

Pass `--ws` to emit a raw `#[ws]` WebSocket handler instead, for clients that
need a bidirectional socket rather than SSE + form posts:

```bash
autumn generate channel Chat --ws
```

Either transport adds the `"ws"` feature to the `autumn-web` dependency in
`Cargo.toml` — channels, SSE, and `#[ws]` are all gated behind it.

The generated smoke test is a real assertion, not a stub: it publishes
through the in-process `TestApp`, subscribes to the same topic, and asserts
the message arrives — no Postgres/Docker required, so it runs on every
`cargo test`.

### Slow live channel verification

```bash
cargo test -p autumn-cli --test generate generated_channel_cargo_checks -- --ignored --exact
cargo test -p autumn-cli --test generate generated_channel_ws_cargo_checks -- --ignored --exact
cargo test -p autumn-cli --test generate generated_channel_smoke_test_passes -- --ignored --exact
```

These scaffold a fresh project, run `autumn generate channel` (both
transports), and assert `cargo check --tests` passes with no hand-editing —
plus one gate that actually runs the generated smoke test with `cargo test`
to confirm it passes on first run.

## `autumn generate webhook`

For inbound provider callbacks — Stripe payment events, GitHub push/CI events,
Slack Events API, or any provider that signs its body with HMAC-SHA256. The
generator wires up the shipped `SignedWebhook` extractor; it never hand-rolls
signature verification.

```bash
autumn generate webhook stripe Payments
```

Produces:

```
src/webhooks/payments.rs   # POST /webhooks/stripe: verified handler, event dispatch, tests
src/webhooks/mod.rs        # pub mod payments; (created or appended)
src/main.rs                # mod webhooks; + routes![...] entry added in place
autumn.toml                # [[security.webhooks.endpoints]] + replay backend + path exemptions
Cargo.toml                 # serde_json + tracing, and the tokio test features
```

The handler takes the extractor and dispatches on the provider's event type,
with clearly-marked stub functions to fill in and a default arm that
acknowledges-and-ignores everything else (a 2xx stops the provider retrying an
event the app does not handle):

```rust,ignore
#[post("/webhooks/stripe")]
pub async fn payments_webhook(webhook: SignedWebhook) -> AutumnResult<Json<serde_json::Value>> {
    let event: serde_json::Value = webhook.json::<serde_json::Value>().map_err(|error| {
        AutumnError::bad_request_msg(format!("invalid stripe webhook payload: {error}"))
    })?;
    match webhook.event_type().unwrap_or("unknown") {
        // TODO: fill this in
        "payment_intent.succeeded" => on_payment_intent_succeeded(&event).await?,
        // …one arm and one `on_*` stub function per preset event…
        _ => tracing::debug!(event_type, "unhandled webhook event — acknowledged and ignored"),
    }
    Ok(Json(serde_json::json!({ "received": true })))
}
```

Provider presets (`stripe`, `github`, `slack`, `generic`) map onto
`WebhookProvider` and pick the route path, signature/event/delivery headers,
and the stub event arms. The Slack preset also unwraps the Events API
`event_callback` envelope — `event_type()` reports the envelope type, not the
inner event — and answers Slack's `url_verification` handshake by echoing the
challenge. `generic` covers any other provider: raw-body HMAC-SHA256 with
`X-Webhook-Signature`/`X-Webhook-Event`/`X-Webhook-Delivery`.

The `autumn.toml` block references the signing secret by environment variable
(`secret_env`) — a plaintext secret is never written — and turns replay
protection on:

```toml
[[security.webhooks.endpoints]]
name = "payments"
path = "/webhooks/stripe"
provider = "stripe"
secret_env = "STRIPE_WEBHOOK_SECRET"
previous_secret_envs = []      # add the old variable here during rotation
replay_protection = true
```

That block is all the wiring the endpoint needs. Autumn installs the webhook
registry from `[security.webhooks]` at startup, and derives the endpoint's CSRF,
submit-token, and CAPTCHA path exemptions from the same block on every boot — a
provider callback carries no browser session, and its signature is its
authentication — so the generator deliberately writes **no** `exempt_paths`
copies that could go stale when the path changes.

`[security.webhooks.replay]` is written explicitly, with guidance to switch to
Redis: production config validation rejects the process-local `memory` backend
for replay-protected endpoints, so a deployed app must configure Redis (which
needs `autumn-web`'s `redis` feature).

The generator then prints the remaining steps: set the secret env var (the app
refuses to start while a configured endpoint has none), point the provider
dashboard at the path, fire a test delivery locally with `autumn webhook sim`,
and fill in the `on_*` stubs.

Useful flags:

```bash
# A second Stripe endpoint (two endpoints on one path fail config validation):
autumn generate webhook stripe Billing --path /webhooks/stripe-billing

# A distinct secret variable per endpoint:
autumn generate webhook generic Partner --secret-env PARTNER_WEBHOOK_SECRET

# Print the plan without writing:
autumn generate webhook stripe Payments --dry-run
```

Fire a signed test delivery at the running app without touching the provider —
same four presets, and a fresh delivery id per call (for Stripe and Slack, whose
replay key lives in the body, the simulator rewrites that field before signing,
so repeated runs are new deliveries rather than `409 Conflict` replays):

```bash
autumn webhook sim stripe http://localhost:3000/webhooks/stripe \
  --secret "$STRIPE_WEBHOOK_SECRET" \
  --payload '{"id":"evt_1","type":"payment_intent.succeeded"}'

# GitHub and generic carry the event type in a header, so name it explicitly —
# the simulator's default (`sim.event`) matches no generated arm:
autumn webhook sim github http://localhost:3000/webhooks/github \
  --secret "$GITHUB_WEBHOOK_SECRET" \
  --payload '{"ref":"refs/heads/main"}' --event push
```

The printed command targets a generated stub arm, so a filled-in handler
actually runs rather than falling through to acknowledge-and-ignore. A `409
Conflict` on a repeat run is replay protection doing its job: for the
header-signed providers the endpoint also keys on the signature, so a
byte-identical payload is a duplicate delivery — vary `--payload`, or restart
the app to clear an in-memory replay store.

The generated `#[cfg(test)]` module is a real assertion, not a stub: it signs a
fixture delivery the way the provider does and asserts a valid signature is
accepted (200), a missing signature header is rejected (400 — the request is
malformed), a well-formed but wrong signature is rejected (401), and a replayed
delivery id is rejected (409). No Postgres or Docker required, so it runs on
every `cargo test`.

Re-running with `--force` and a different `--path`/`--secret-env` updates the
existing endpoint block in place rather than leaving a stale one behind (the
registry matches paths exactly, so a stale path would 500 every real delivery).

`autumn destroy webhook <provider> <Name>` removes the handler, its route
registration, and its `autumn.toml` block — including the shared replay block
once the last endpoint is gone. A `--path`/`--secret-env` used at generation
time does not have to be repeated: destroy recovers both from the endpoint block
recorded under the same name (an explicit flag still wins). Config you have since edited by hand (rotation
variables in `previous_secret_envs`, a tightened `timestamp_tolerance_secs`, a
Redis replay backend) is left in place rather than deleted.

### Slow live webhook verification

```bash
cargo test -p autumn-cli --test generate generated_webhook_cargo_checks -- --ignored --exact
cargo test -p autumn-cli --test generate generated_webhook_tests_pass -- --ignored --exact
```

These scaffold a fresh project, generate all four presets, and assert `cargo
check --tests` passes with no hand-editing — plus one gate that actually runs
the generated tests to confirm they pass on first run.

## `autumn generate scaffold`

Everything `model` produces, plus:

- `src/repositories/<snake>.rs` — a `#[repository(Model, api = "/api/<plural>")]`
  block that auto-generates CRUD methods plus JSON REST handlers.
- `src/repositories/mod.rs` — module aggregator.
- `src/routes/<plural>.rs` — Maud HTML handlers for `index`, `show`, `new_form`,
  `create`, `edit_form`, and `update`. (Skipped if `--api` is set).
- `src/routes/mod.rs` — module aggregator. (Skipped if `--api` is set).
- `tests/<snake>.rs` — a real, in-process smoke test built on
  `autumn_web::test::{TestApp, TestClient, TestDb}`: it boots a throwaway
  Postgres database, fires a request at a stand-in for the scaffolded index
  route, and asserts a real response — no running server, no env var, no
  silent skip. `cargo test` reports it as `ignored` with an explicit reason
  (Docker isn't assumed to be available); run `cargo test -- --ignored` to
  execute it for real. `--api` scaffolds get the JSON equivalent, asserting
  against `GET /api/<plural>`.
- `src/main.rs` — the `mod` declarations plus `routes![…]` entries get
  added in place. Existing entries are preserved; rerunning the generator
  with the same arguments is a no-op. By default, the scaffold registers only
  read-only API routes (`GET /api/<plural>` and `GET /api/<plural>/{id}`); mount
  `POST`/`PUT`/`DELETE` handlers only after adding a repository policy. For `--api`
  scaffolds, all 5 JSON endpoints (GET index/show, POST, PUT, DELETE) are automatically registered.

### No-JavaScript edit and delete flows

The scaffolded HTML routes accept ordinary browser form submissions
because Autumn's [method-override middleware](./middleware.md) rewrites
a `POST` carrying `_method=PUT|PATCH|DELETE` into the declared method
**before route matching**. That means you can keep your generated
handlers as `#[put]` / `#[delete]` and still serve clients with
JavaScript disabled — no parallel POST-only routes required.

Use [`autumn_web::form::method_input`](../../autumn/src/form.rs)
(or `ChangesetForm::form_tag` with `"delete"` / `"put"` /
`"patch"`) inside generated edit views and any custom edit/delete
buttons you add later:

```rust,ignore
use autumn_web::form::method_input;
use autumn_web::security::CsrfToken;

#[get("/bookmarks/{id}/edit")]
async fn edit_form(id: Path<i64>, csrf: Option<CsrfToken>) -> Markup {
    html! {
        // Delete button as a plain HTML form — works without htmx.
        form method="post" action=(format!("/bookmarks/{}", *id)) {
            (method_input("DELETE"))
            @if let Some(token) = csrf.as_ref() {
                input type="hidden" name="_csrf" value=(token.token());
            }
            button type="submit" { "Delete" }
        }
    }
}
```

`autumn routes` and `/actuator/routes` keep reporting the declared
method (`PUT`, `PATCH`, or `DELETE`); the rewrite is a transport
concession, not a routing one. CSRF protection still treats the
overridden mutation as unsafe and rejects submissions without a valid
token with `403 Forbidden`.

### Bulk select and delete selected

Every standard HTML scaffold's index list ships a no-JavaScript
bulk-delete flow (issue #1312):

- The `data_table` gains a leading checkbox column. Each row renders
  `autumn_web::widgets::bulk_select_checkbox(row.id, &bulk_cfg)` —
  `<input type="checkbox" name="ids" value="…">` with an
  `aria-label="Select row <id>"`, so a screen reader announces which row
  each control selects.
- The list (and, with `--searchable`, the whole
  `#<plural>-search-results` container htmx swaps) is wrapped in
  `autumn_web::widgets::bulk_actions_form(...)`: a plain
  `POST /<plural>/bulk_delete` form carrying the CSRF hidden field, the
  one-time submit-token hidden field, and a **Delete selected** submit
  button. The "New …" link and the search box stay outside the form —
  they are page furniture, not part of the selection.
- A `#[secured] #[post("/<plural>/bulk_delete")]` handler is emitted and
  mounted in `src/main.rs` right after `destroy`.

Contract of the generated handler:

| Situation | Behaviour |
| --------- | --------- |
| Checked rows | Deleted through the repository's `delete_many`, then a `Deleted N <plural>` flash and a 303 back to the index. |
| Field name | `name="ids"` (matching `autumn-admin-plugin`); the parser also accepts the `ids[]` spelling some clients send. |
| Empty selection | Info flash + 303 redirect. **Never** a 400 — a list-write endpoint doesn't fail on missing params. |
| Malformed id (`ids=abc`) | Silently dropped, same as above. Duplicates are collapsed through a `HashSet`, so parsing a crafted body full of distinct ids stays linear rather than quadratic. |
| Oversized selection | Capped at `MAX_BULK_IDS` (5000). A real selection is page-sized, so this only bites on a hand-crafted body — the default 32 MiB request limit otherwise leaves room for over a million ids. The parser stops one past the cap, and the handler refuses the batch with an error flash rather than truncating it: a silently partial destructive batch is worse than a refused one. |
| Large selection | The pre-flight `SELECT` is chunked at 1000 ids. `eq_any` binds one parameter per id, and `autumn_web::repository::MAX_BIND_PARAMS` is 32766 on SQLite, so one unbounded `eq_any` would fail with "too many SQL variables" before reaching the already-chunked `delete_many`. |
| `--soft-delete` | The pre-flight `SELECT` filters `deleted_at IS NULL`, and `delete_many` applies the soft-delete update — no hand-rolled `deleted_at` write. |
| Record policy wiring on (an owner column, the default) | Each selected row is authorized with the same `"delete"` action `destroy` uses. A row the actor may not delete is dropped from the batch rather than 403'ing the request, so the endpoint is not an existence oracle. |
| `dependent(restrict)` child rows | `delete_many` probes first and aborts the **whole** batch with a 409, rolling back — no partial delete. |
| Connection use | The handler `drop`s its `Db` extractor after the pre-flight `SELECT` and before `delete_many`, which checks out a connection of its own. It therefore holds **one** pooled connection at a time, which cannot stall on `database.pool.max_size = 1` or deadlock at any concurrency. |
| Double-click / Back→resubmit | The form carries a one-time `_submit_token` (issue #1360), so `SubmitTokenLayer` consumes it once and replays the first response instead of re-running the batch — hooks and dependent deletes included. The field leads the form body, ahead of the checkboxes, because the layer only scans the body's first chunk and a long selection would otherwise push it past the scan cap. |

Not emitted for `--live`, `--live-validation`, or `--sharded` (their list
DOM is owned by an SSE/htmx swap contract, or has no cross-shard
`delete_many`), and `--api` scaffolds render no HTML at all. Those
variants' output is byte-identical to their pre-#1312 shape.

The three widgets are ordinary public helpers — use them in hand-written
list views too:

```rust,ignore
use autumn_web::widgets::{BulkActionsConfig, bulk_actions_form, bulk_select_checkbox};

let action = paths::bulk_delete();
let cfg = BulkActionsConfig::new(&action)
    .submit_label("Archive selected");
html! {
    (bulk_actions_form(
        &cfg,
        csrf.as_ref().map(|t| t.token()),
        csrf_field.as_ref().map(|f| f.0.as_str()),
        submit_token.as_ref().map(|t| t.token()),
        submit_field.as_ref().map(|f| f.0.as_str()),
        html! { (my_list(&rows, &cfg)) },
    ))
}
```

Pass the submit-token pair on any destructive bulk form. A request that
carries no `_submit_token` passes through `SubmitTokenLayer` unguarded,
so omitting it silently gives up double-submit protection on exactly the
endpoint that most needs it.

The bulk toolbar emits **no** confirmation prompt, and the flow needs no
JavaScript at all. Autumn's default CSP is `script-src 'self'` with no
`'unsafe-inline'`, so an inline `onclick="return confirm(..)"` is blocked
by the browser — the form would submit with no prompt, which is worse
than not promising one. The framework's server-rendered replacement for
`window.confirm()`, [`confirm_action`](../../autumn/src/widgets.rs), submits
its own single-action form and so cannot carry a bulk form's checkbox
selection (HTML forbids nesting forms). To confirm a batch, post the
selection to an interstitial page that lists the affected rows and asks
for a second, explicit submit.

### Concurrent edits: `lock_version` (optimistic locking)

Declare a column named `lock_version` and the scaffold wires the
framework's [optimistic-concurrency
primitive](./cloud-native.md#optimistic-concurrency-via-lock_version)
through the HTML edit flow (issue #1318) — no extra flag, no extra code:

`lock_version` is a **magic column name**, like `slug` and `deleted_at`.
Declaring it changes what the column *is*, so the generator prints a
warning saying so; rename the column if you wanted an ordinary integer
counter you set yourself.

```bash
autumn generate scaffold Post title:String body:Text lock_version:i32
```

- The **model** gets `#[lock_version]`, so the column is DB-managed:
  excluded from `NewPost`, carried on `UpdatePost` as the *expected*
  version, and compared by `#[repository]`'s update (which is what makes
  the JSON API path conflict-check too).
- The **migration** declares it `NOT NULL DEFAULT 0` (`INTEGER` for
  `i32`, `BIGINT` for `i64`), since the INSERT never names it. Pass
  `--default lock_version=<n>` to seed from a different base (a seed at the
  column's maximum is refused — the generated `UPDATE` increments in SQL, and
  Postgres raises `integer out of range` rather than wrapping, so the first
  save on every row would fail). A counter that reaches its ceiling
  organically needs 2³¹ saves of one row; use `lock_version:i64` for anything
  that churns that hard.
  `autumn generate migration AddLockVersionToPosts lock_version:i32`
  gets the same `DEFAULT 0`, so retrofitting an existing table also
  backfills its rows in one statement.
- The **edit form** carries the row's current version in a hidden
  `lock_version` input. The new form does not — a row that does not exist
  yet has no version to guard against — and the version never appears as
  an editable control.
- The **update handler** turns the write into a compare-and-swap:

  ```rust,ignore
  let updated = diesel::update(
      posts::table.find(*id).filter(posts::lock_version.eq(expected_lock_version)),
  )
      .set((
          posts::title.eq(new.title.clone()),
          posts::lock_version.eq(posts::lock_version + 1),
      ))
      .execute(&mut *db)
      .await?;
  ```

  The guard and the bump are one statement, so there is no
  read-modify-write window between them.
- A **stale submit** matches zero rows. The handler re-reads the row to
  tell "someone else got there first" (409) from "the row is gone" (404).
  On 409 it re-renders the *same* edit form with the author's own input
  intact, an inline `role="alert"` banner, and — deliberately — the row's
  **current** version in the hidden field, so a second Save applies their
  edit on top of the newer row. Handing the stale version back would make
  the form permanently unsavable.
- The generated `tests/<snake>.rs` gains a
  `<plural>_optimistic_lock_conflict` test covering exactly that
  sequence: first write wins and bumps, stale write 409s and changes
  nothing, retry against the returned version succeeds.

- A `:states(...)` **transition** is guarded the same way. It is itself a
  read-modify-write — load the row, check the edge is legal from the state
  just read, write — so its `UPDATE` carries `WHERE lock_version =
  <the version it read>` and 409s (re-rendering the detail page) when it
  loses the race, instead of letting two concurrent transitions out of the
  same state both commit. It bumps the version too, so an author holding an
  edit form opened before the transition is told the record moved on.
- `autumn db pull` reproduces the attribute when it finds a
  `lock_version` column, so a pulled table round-trips to the same model.

The column must be a non-nullable `i32` or `i64`; anything else is
rejected at generation time rather than silently ignored.
`--live-validation` is supported (it changes only how the controls are
rendered; its `update` writes through the same guarded statement).

**Also affects `--api`.** `#[lock_version]` puts a **required**
`lock_version` on `UpdatePost`, so JSON `PUT`/`PATCH` clients must send
the version they read — a deliberate contract change, and how the API
path gets conflict-checking. Existing clients that omit it will fail
deserialization.

**Not covered by the guard.** The scaffolded **admin** update
(`autumn generate admin`) and the **delete** actions bump or write
without a `WHERE lock_version = …` guard — the admin handler
deserializes `NewPost`, which excludes the column, so the expected
version never reaches it, and locking across deletes is out of scope
(issues #1021/#1312). Both remain last-write-wins.

**Not yet wired (HTML scaffolds only):** `--live`, `--sharded`, a `slug`
column, and scaffolds with an `Attachment` column write through paths that
do not route via the guarded statement, so combining them with
`lock_version` is refused up front instead of emitting an edit form that
only looks concurrency-safe. `--api` is exempt from all of these — it emits
no routes file, so there is no form to be inconsistent with, and
`--api --live` and friends keep generating.
(`slug` is refused because a slug scaffold keys its update off the
editable, reusable slug rather than the primary key, so
`WHERE slug = … AND lock_version = …` does not identify a stable row.)

### Threaded comments on anything with `commentable`

Declare a field with the `commentable` type and the scaffold wires a threaded,
**polymorphic** comment system (issue #1367) — one `comments` table that
attaches to *any* number of models — with **zero hand-written comment routes,
queries, or threading SQL**:

```bash
autumn generate scaffold Post title:String comments:commentable
```

- The token names the *feature*, not the column. The column it adds is the
  counter-cache source, normalised to `{singular}_count` — `comments:commentable`
  → `comment_count BIGINT NOT NULL DEFAULT 0`. Like `position`, it is
  server-managed: excluded from `NewPost`/`UpdatePost` and from the generated
  create/edit form, because the framework maintains it inside each comment's
  own transaction.
- A **second migration** creates the shared
  `comments(commentable_type, commentable_id, parent_id, author_id, body,
  created_at, deleted_at)` table, with an index covering the thread read and
  another for the delete cascade. It is emitted **once per project**: run the
  same token on a second model and the generator reuses the existing table and
  says so.
- The **model** gets `#[commentable(by = User, counter_cache = comment_count)]`
  — `by` only when the project actually has a `User` model, since naming a
  missing one would be a compile error in a file you did not write. That
  attribute is what brings `add_comment` / `comment_thread` / `delete_comment`
  onto the generated repository and registers the model with the framework's
  comment router.
- **No comment routes are generated at all.** Mount the framework's once:

  ```rust
  .nest("/comments", autumn_web::commentable::router(Default::default()))
  ```

  and render a thread with `autumn_web::widgets::comment_thread`. Adding a
  third commentable model needs no change there.

At most one `commentable` field per model, and it takes no `{…}` modifiers —
everything else is configured on the model's `#[commentable(...)]` attribute.
See [Threaded Comments on Anything](commentable.md) for the full option list.

### User-orderable lists with `position`

Declare a field with the `position` type and the scaffold wires a
transaction-safe, race-safe reorderable list (issue #1358) — todo
priorities, kanban columns, playlist tracks, form-builder fields — with
**zero hand-written reindexing SQL**:

```bash
autumn generate scaffold Todo title:String rank:position
```

- The **column name is yours** (`rank` above) — `position` is the *type*
  token, parsed the same way `String`/`i64`/`references` are; the field
  itself is server-managed, like `lock_version`.
- The **model** gets `#[position]`, so the column is excluded from
  `NewTodo`/`UpdateTodo` entirely: a create/update payload can never set it
  directly, which is what keeps the contiguous ordering from being hand-
  edited into an inconsistent state.
- The **migration** declares it `BIGINT NOT NULL DEFAULT 0` plus an index,
  and adds two database triggers (issue #1358's "handle every insert/delete
  path, not just the generated repository's" requirement):
  - an insert trigger assigns the next contiguous value (`MAX(position) +
    1`, or `0` for the first row) — every new row appends to the end of its
    list;
  - a delete trigger (and, under `--soft-delete`, a soft-delete trigger)
    compacts the remaining rows so no gap is left.

  The column's own `DEFAULT 0` is a placeholder only, immediately corrected
  by the trigger inside the same statement/transaction as the insert — it
  is never visible to a concurrent reader.
- The **repository** (`#[repository(Todo, position(column = "rank"))]`)
  gains five methods, each `O(rows shifted)` and transaction-safe:

  ```rust,ignore
  repo.move_to(id, 3).await?;        // absolute index, clamped to [0, len-1]
  repo.move_before(id, other_id).await?;
  repo.move_after(id, other_id).await?;
  repo.move_up(id).await?;           // one step toward index 0 (no-op at the start)
  repo.move_down(id).await?;         // one step away from index 0 (no-op at the end)
  ```

  `move_to` locks every row in the list (ordered by `id` — a fixed lock
  order, so two concurrent moves on the same list serialize against each
  other's first lock rather than deadlock), re-derives the row's current
  position under that lock (a prior mover may have shifted it), clamps the
  target, then shifts only the rows strictly between the old and new
  position before setting this row's — never a full-table rewrite. This is
  what makes the ordering safe under concurrent reorders: two browser tabs
  dragging different rows at once still leave a valid, gapless `0..len-1`
  permutation.
- The **HTML index** orders by the position column and renders no-JS
  **Move up / Move down** buttons per row — small `POST` forms, CSRF- and
  one-time-submit-token-protected like the trash view's Restore button, so
  a reorderable list works with JavaScript disabled.

Scope the ordering to a parent with `{scope:col}` — a separate contiguous
`0..len-1` sequence per distinct value of that column, so reordering one
board's tasks never touches another board's:

```bash
autumn generate scaffold Task title:String board:references rank:position{scope:board_id}
```

The scope column must already be a `references` foreign key (the DSL
rejects any other kind), and the migration's index becomes composite
— `(board_id, rank)` — since every real query filters by scope first.

**At most one `position` field per model.** `--default rank=<n>` is
refused — a constant default would give every new row the same value,
which is exactly the invariant this feature exists to maintain (compare
`lock_version`, where a constant default is correct).

**Not yet wired:** `tenant_scoped`, `versioned = true`, and `dependent(...)`
repositories — `position(...)` is refused up front in combination with any
of them (matching `retention(...)`'s own posture on the same three). A
`--sharded` scaffold gets the column, migration triggers, and `#[position]`
attribute, but no `move_*` methods and no HTML buttons — a move only
reaches the pool/shard it happens to be given, so
`#[repository(..., sharded)]` rejects `position(...)` outright; the
generator prints a warning explaining why. `--api`/`--live`/
`--live-validation`/owner-scoped scaffolds keep the `move_*` methods (and,
for `--api`, the ordered data) but not the HTML buttons — reordering there
needs a hand-written endpoint or client-side call against the repository
method directly.

**Restoring a soft-deleted row** does not renumber it back into the live
sequence — it keeps the stale position it had when deleted, which can now
collide with a position a live row has since taken. Out of scope for this
slice; call `move_to` after restoring if you need it placed precisely.

### Export CSV from the list view

Every standard HTML scaffold's index also ships a working **Export CSV**
download (issue #1315) — zero lines of user code:

- A `CsvSchema` impl for the model is emitted in
  `src/routes/<plural>.rs`, listing `id`, every scaffolded column in
  declaration order, then `created_at` — the same set the `show` view
  renders. It is ordinary editable Rust: drop a sensitive column from the
  export by deleting one entry from `csv_columns` and its matching slot in
  `to_csv_record`.
- A `#[get("/<plural>/export.csv")]` handler feeds those rows through
  `autumn_web::data::csv::export_csv` and answers with a
  [`Download`](downloads.md), which derives
  `Content-Type: text/csv; charset=utf-8`,
  `Content-Disposition: attachment; filename="<plural>.csv"` and
  `Content-Length` from the `.csv` filename.
- The index renders an **Export CSV** link next to "New …", carrying the
  index's current query string — so *filter → sort → export* downloads
  exactly the rows on screen.
- `autumn-web`'s `csv` feature is added to `Cargo.toml` (and removed again
  by `autumn destroy scaffold`, unless hand-written code still uses
  `autumn_web::data::csv`).
- The generated `tests/<name>.rs` gains a database-free test asserting the
  download contract: 200, `text/csv`, an `attachment` disposition, the
  model's header row, RFC 4180 quoting, empty cells for NULLs, and the
  formula guard below.

Contract of the generated handler:

| Situation | Behaviour |
| --------- | --------- |
| `?sort=`/`?filter[col]=` | Honoured through the same `ListQuery` extractor and the same `repo.list` call the index uses, so the file reflects the filtered view rather than the whole table. Unknown or malicious keys are ignored against the model's column allowlist. |
| `?page=`/`?size=` | Ignored. An export spans every page of the current filter. |
| `?q=` (`--searchable`) | **Not** honoured — `ListQuery` carries no full-text term, so the export cannot reproduce a `/<plural>/search` result set. Because of that the link is *placed* differently on a searchable scaffold: it renders **inside** the `#<plural>-search-results` container rather than beside "New …", so the htmx swap that shows search results also takes the link away. That matters because `active_search_input` pushes no URL — a link left outside the container would survive the swap still pointing at the unsearched set, and a user who narrowed the list would silently download the rows they just excluded. Searched results therefore offer no export; clear the search to get the link back. Filter with `?filter[col]=` instead, or add a `q` param to the handler and call the repository's `search_page` yourself. |
| Row volume | Read in `MAX_PAGE_SIZE` (100) batches, so no single query loads the table, and capped at `MAX_EXPORT_ROWS` (10 000). That caps the **row count**, not bytes: the response is collected in memory, so a model with an unbounded `Text` column can still build a large body. Put a `{max}` length on such columns, lower the constant, or stream with `Download::from_stream` for a genuinely unbounded export. |
| Hitting the cap | The file is truncated to `MAX_EXPORT_ROWS`, a `warn!` is logged, and the response carries `x-export-truncated: true`. To tell a complete export of exactly `MAX_EXPORT_ROWS` rows from a truncated one, the loop reads one batch **past** the cap and trims the surplus — so neither signal fires on an export that merely fills the cap exactly. There is no in-band marker in the CSV (it would parse as data), so narrow the filter (or raise the cap) rather than trusting a capped export for reconciliation. |
| Concurrent writes | Consistency is **per batch, not per export**. Each batch is an independent `LIMIT`/`OFFSET` query on its own pooled connection, so a row inserted or deleted mid-export shifts the offsets under the batches still to come and can be written twice or skipped. The index has the same property; an export spans more pages and more wall-clock, so it is likelier to notice. A point-in-time exact download means reading the batches yourself inside `Db::tx_with(TxOptions::repeatable_read().read_only(), ..)` — repository reads cannot be routed through a caller's transaction today, so that path also means re-deriving the sort/filter allowlist and any owner scoping by hand. |
| Request cost | The handler carries `#[throttle(limit = 6, per = "1m", key = "ip")]` that the index does not, and the cost is worse than the row count suggests: `list` runs a filtered `COUNT(*)` before each page, so a full export is ~100 page queries **and** ~100 whole-result-set counts — ~200 round trips where one index page costs two. The counts are waste (the export loop never reads `total_elements`; it stops on a short batch) but unavoidable, since `list` is also what applies the sort/filter allowlist and the repository exposes no count-free equivalent. On a large or poorly indexed table, lower `MAX_EXPORT_ROWS`, tighten the throttle, or put the route behind auth. Inline throttles apply regardless of `security.rate_limit.enabled` (that flag governs the *global* limiter); raise the limit freely for an internal back-office. |
| `NULL` column | An **empty cell**, never the literal string `None`. |
| Commas, quotes, newlines | Quoted and escaped per RFC 4180 by `export_csv`. Do not pre-quote values in `to_csv_record` — it would double-escape them. |
| Values that look like formulas | Text-backed columns pass through an emitted `csv_text_cell` helper that prefixes an apostrophe to a value starting `=`, `+`, `-`, `@`, TAB or CR. Numeric, boolean, UUID, timestamp and enum columns are **not** guarded — they render from typed values and cannot carry a formula, and guarding them would corrupt a negative number. |
| `Attachment` column | Exports the blob's storage **key**, not the signed, time-bounded URL the `show` view renders (a spreadsheet cell has no use for a URL that expires). Drop the column from `csv_columns` if those keys should not leave the app. |
| `references` column | Exports the raw **foreign key**, not the parent label the index and `show` views resolve. An id round-trips back through `import_csv`; a label does not, and resolving one per row would be an N+1 inside the export loop. |
| `--default`ed column | **Included.** It is dropped from the form and the index table, but it is model data the `show` view already renders, and a spreadsheet wants it. |
| `--soft-delete`'s `deleted_at` | Excluded. `list`/`list_scoped` filter `deleted_at IS NULL`, so the column is blank for every exportable row. |
| Record policy wiring on (an owner column, the default) | The handler is `#[secured]` and reads through the repository's owner-scoped `list_scoped` — the same method the index uses. It never calls the unscoped `list`, so the download cannot contain another user's rows. |
| No owner column | No `#[secured]`, exactly matching the generated index. The export opens no data path the list view did not already open. |
| Route vs `/<plural>/{id}` | The static `export.csv` segment outranks the `{id}` parameter in the router, the same way `/<plural>/new` already does. With a `slug` route key that also means a record whose slug is literally `export.csv` is unreachable at its canonical URL — the same caveat `new` already carries. |

The export is emitted wherever the index's row set is a repository call it
can reuse verbatim: the plain `repo.list` index (including
`--live-validation`) and the owner-scoped `repo.list_scoped` one. It is
**not** emitted for `--live` (an SSE `<ul>` on `repo.page`, with no
`ListQuery` to honour), `--sharded` (`from_shard` pins the query to a
single shard, so "export everything" would silently cover a fraction of
the table), an owner-scoped `--live-validation` index (it runs a manual
owner-filtered query rather than a scoped repository method), or `--api`
(no HTML index at all). Those variants' output is byte-identical to their
pre-#1315 shape.

> **Spreadsheet formula injection.** A cell beginning `=`, `+`, `-` or `@`
> may be interpreted as a formula by Excel. If the model holds untrusted
> text and you expect exports to be opened there, prefix-guard those values
> in the generated `to_csv_record` — the emitted impl carries a note at the
> same spot.

### Import CSV with a dry-run preview (`--import`)

`--import` adds the other direction of the same data door: an upload form, a
**dry-run preview** with per-row errors, and a confirmed commit — zero lines of
user code, no hand-written multipart handler, no import loop (issue #1393).

```bash
autumn generate scaffold Post title:String body:Text published:bool --import
```

- `GET /<plural>/import` renders the upload form: a file input, the
  confirmation checkbox, and the **expected header row** — printed as a
  copy-pasteable line straight from the same `CsvSchema::csv_columns()` the
  export writes, followed by the columns the import cannot set. An **Import
  CSV** link appears on the index beside **New …**. (Unlike **Export CSV** it
  stays there on a `--searchable` scaffold: it opens an upload form rather than
  describing the row set on screen, so a search that swaps the results container
  cannot leave it pointing at rows the user has filtered away.)
- `POST /<plural>/import` parses the uploaded multipart CSV and, unless the
  submit carries the `commit` confirmation, runs
  `autumn_web::data::csv::import_csv` in **`ImportMode::DryRun`**: the response
  reports rows read, rows that *would* insert, and a table of row errors with
  the **line number** and message from `ImportReport`. Nothing is written.
- Ticking **"Import for real"** and uploading again runs the same parse in write
  mode and commits through the repository's `save_many_skip_invalid` — a batched
  insert inside a transaction that falls back to row-by-row on a database
  constraint failure, so one duplicate key does not take the batch down with it.
  Every skipped row comes back against its own CSV line, and the result page
  shows inserted-vs-failed. No row is dropped silently.
- `autumn-web`'s `multipart` feature is added to `Cargo.toml` (`csv` is already
  on, because the import rides on the export's gate), and removed again by
  `autumn destroy scaffold` unless other code still uses it.
- The generated `tests/<name>.rs` gains a database-free test that uploads a
  2-row CSV (1 valid, 1 invalid), asserts the dry run reports 1 insertable row
  and 1 row error **on the right line** and writes nothing, then commits and
  asserts exactly the valid row persists.

**One column map, both directions** — for every column the form can set. A
column the CSV cannot faithfully carry back (`Attachment`, `Bytea`) is named on
the upload page as one the import cannot set, rather than round-tripped
lossily. The import decodes each row against the
*same* generated `CsvSchema` impl the export writes from — there is no second
list to keep in step. That also means a file this app exported can be edited and
uploaded straight back: `id` and `created_at` are columns the form does not own,
so they are ignored on the way in rather than rejected.

Contract of the generated handler:

| Situation | Behaviour |
| --------- | --------- |
| The confirmation box | Comes back **unchecked** on every page the handler renders — the preview, the committed result and the 422 refusal alike. The operator's next move after any of them may be to choose a different file, and a carried-over tick would commit one nobody previewed. |
| No `commit` confirmation | `ImportMode::DryRun`. Every row is parsed and validated, the report says what *would* happen, and the handler's only write call is not reached. This is the default: an unchecked checkbox submits nothing at all. |
| A file missing an expected column | Refused whole with a 422 naming the missing columns, before any row is decoded. This is the check that catches the *wrong file*: `decode_form` ignores headers it does not know and defaults fields that are absent, so for a model whose every form field can be defaulted (an unchecked checkbox's `bool`, an optional column) an unrelated spreadsheet would otherwise decode into a run of blank records and report them as insertable. Row-level validation cannot see it — each row is valid. Only the header can. |
| Whitespace around a column name | Trimmed, consistently. RFC 4180 keeps the space in `a, b, c`, and plenty of exporters write it — so both the header check and the row decoder compare trimmed names. They have to agree: a check that accepted `" published"` while the decoder looked for `published` would default the field and import blanks under a guarantee that the column was there. |
| How a row becomes a record | The row (`column -> value`) is re-encoded as a urlencoded body and handed to the module's own `decode_form`, so it is decoded, blank-normalized and validated by **exactly** the code path a browser form submission takes — the same `#[validate(...)]` rules, the same `into_new`. |
| A row that fails to parse | A row error naming the parse failure, against that row's line. The rest of the file still imports. |
| A row that fails validation | A **field** error naming the column (alphabetically first, so the message is stable) and its messages. On a `#[normalize]` column there is a second, later check: the repository re-runs the model's rules on the normalized value (#2586), and a row only that pass rejects is reported against its CSV line as a write failure instead, with nothing written for it. |
| A row the database rejects (e.g. a unique violation) | Reported against its own CSV line. `save_many_skip_invalid` isolates the failing chunk row-by-row rather than aborting the batch. |
| Atomicity | Owned by `save_many_skip_invalid`, which this handler calls exactly once: successful rows commit, failed rows are skipped and reported. For all-or-nothing, swap in `save_many`, which aborts the batch on the first failure. |
| Model hooks | Run per inserted row, exactly as for `create` — including `after_create_commit` and counter caches. An import of N rows is N records' worth of side effects. |
| Upload size | Capped at the emitted `MAX_IMPORT_BYTES` (2 MiB) *and* `security.upload.max_file_size_bytes`, whichever is smaller — `with_max_bytes` takes the min. Over the cap is a **413**, never a truncated import. That constant, not a row count, is what bounds the handler's memory. |
| File-type check | The filename must end in `.csv` (case-insensitively) **and** the declared content type must be one of a small allow-list (`text/csv`, `application/csv`, `text/plain`, `application/vnd.ms-excel`, `application/octet-stream`, or absent — browsers are inconsistent here). This is a *shape* check: CSV has no magic bytes, so what actually protects the handler is that the body is only ever parsed as CSV and every row is validated before it can be written. |
| Magic-byte sniffing (#1354) | Composes with care, and is not required. Setting `security.upload.allowed_mime_types` makes the `Multipart` extractor sniff each file part before the handler runs; CSV is plain text and has no signature, so the extractor falls back to the *declared* type — but only for types it classifies as signatureless text (`text/*`, `application/json`, `application/csv`). The two fallbacks this route also accepts because browsers really send them, `application/vnd.ms-excel` and `application/octet-stream`, are **not** in that set: allow-listing them does not help, and such uploads are refused before the handler sees them. If you enable an allow-list, expect to accept only `text/csv`/`text/plain`-declared uploads. |
| CSRF and double submit | The POST is `#[secured]` and the form renders the shipped CSRF and one-time submit-token hidden inputs as its **first** fields, so both land inside `security.csrf.token_scan_bytes` on a multipart body. Keep them first if you rearrange the form. |
| Rate limiting | No `#[throttle]` (unlike the export): the route is `#[secured]`, and its work is bounded by `MAX_IMPORT_BYTES` and `MAX_IMPORT_ROWS` rather than by the size of the table. It also does not fit: the handler declares 9 extractors on a policy-enabled scaffold (10 under `--i18n`), `#[secured]` injects 3 more and `#[throttle]` 8, and axum's `Handler` tops out at 16. To add one, drop extractors to make room — the submit-token pair is the cheapest, at the cost of double-submit protection. `--no-policy` (7 params) does leave room. |
| Record policy wiring on (an owner column, the default) | The handler is `#[secured]` and calls `authorize_create::<Model>` once, exactly like `create`: an import has no loaded row to authorize per record, any more than a create does. A per-row rule therefore does not apply here either; add the check inside the row closure if your policy needs one. |
| Line numbers | 1-based lines of the uploaded file, header included — and the **same numbers for CRLF and LF** files. (The underlying CSV parser's own counter runs one behind on CRLF, the dialect Excel writes; `import_csv` calibrates against the header and corrects for it, and strips the parser's own uncalibrated line number out of the message so a reader never sees two.) A quoted field containing a newline moves the rows after it down, and the reported numbers move with them. A header that itself spans a quoted newline is accounted for, so it does not shift the rows under it. The shift is measured once, so only a file that *mixes* terminators drifts. |
| Existing records | **Never matched or updated.** Every row is inserted as a new record and an `id` column in the file is ignored, so re-uploading an exported file *duplicates* it rather than updating it in place. `ImportMode::Upsert { by }` exists in the framework for update-in-place; wiring it up means matching on the `by` columns in the row closure and calling the repository's update path, which is deliberately left to you rather than guessed at. |
| Columns the import cannot set | Named on the upload page, from an emitted `CSV_IGNORED_COLUMNS`: `id` and `created_at` (the database assigns both), an `Attachment` (a storage key in a cell is not a file), and anything dropped from the form — a `--default`ed column, for instance. They are in the file the export writes, so without that line an operator could edit one, re-upload, and watch nothing happen. |
| An `#[encrypted]` column | **Refuses the whole surface.** The export omits an at-rest encrypted column (#1340 — the model holds plaintext), but the generated form requires it, so a file headed by `csv_columns()` could never satisfy it and every row would fail with "missing field". `--import` generates nothing there and warns, naming the column. |
| `bool` column | Normalized through an emitted `csv_bool_cell`: `TRUE`/`FALSE` (what Excel writes), `1`/`0`, `yes`/`no`, `on`/`off` all decode, and a **blank** cell means `false` for a non-nullable column — the same thing an unchecked checkbox means, which the form's `#[serde(default)]` already encodes. A nullable column keeps its blank as `NULL`. An unrecognised value is passed through untouched so it fails with the form's own message, naming the column. |
| Duplicate header names | The last column wins, silently (`import_csv` builds a `column -> value` map). A shifted or duplicated column in a spreadsheet therefore imports the wrong data rather than being rejected — check the preview. |
| No owner column | The POST is still `#[secured]` (unlike the export, which mirrors an unsecured index): an import writes. |
| Owner-scoped scaffold | `authorize_create` runs once, exactly as for `create` — and, exactly as for `create`, an owner column is taken from the submitted data, so an authorized user can insert rows owned by anyone. That is not new (the create form has the same property), but note the asymmetry with the export directly above, which *is* owner-scoped through `list_scoped`. Move the owner column out of the file and set it from the session in the row closure if that matters. |
| `--belongs-to` child | The parent foreign key comes from the FILE, not from a URL — the nested create route's "the parent is the route" rule does not apply to a flat `POST /<children>/import`. |
| A write that fails partway | `save_many_skip_invalid` writes in chunks, each in its own transaction. A failure a constraint cannot explain (a timeout, a dropped connection) aborts the call with earlier chunks already committed; the report says so in a `role="alert"` banner rather than 500ing, because the operator's natural next move — re-upload — would duplicate whatever landed. How many rows landed is unknowable there, so the count keeps the parse pass's total (rows that *reached* the write) and its label reverts to the dry run's wording rather than claiming an insert count it cannot have. |
| A row reported as failed | May still be **in the database**. `after_create` hooks run once the insert has committed, and a hook that fails puts an already-persisted row among the failures with nothing to distinguish it from a row that never landed. (A row the model's own `#[validate]` rules rejected is the exception: that check runs before the insert, so nothing was written — but the report cannot tell you which kind you are looking at.) Whenever the database stage rejected any row, the report carries a `role="alert"` note saying so — check the list view before re-importing. |
| A value for a column the import cannot set | Named in a `role="alert"` note on the report when the uploaded file actually carries one (`id` and `created_at` excluded — every exported file has those, so flagging them would fire on every round trip). Silently dropping an operator's spreadsheet edit is the one failure the counts could otherwise hide entirely. |
| Row volume | Capped at `MAX_IMPORT_ROWS` (10 000), mirroring the export. A file over the cap is **refused whole** with a 422, never imported as a prefix — a partially imported spreadsheet is the trap this route exists to avoid. The count is taken by `autumn_web::data::csv::count_data_rows` *before* the import, because a malformed row never reaches the row handler (`import_csv` records it and moves on), so an in-handler counter would miss exactly the file that costs the most to accumulate. The rendered error list is separately capped at `MAX_REPORT_ERRORS` (200) with a "further errors not listed" line; the counts above the table are always the whole truth. |
| An oversized upload | A **413** from the framework's size guard, not the friendly 422 the other refusals use — the status is the accurate one, but the page is the generic error page. |
| A column the import cannot set | Filtered out of the row *before* `decode_form` sees it, not merely omitted from the column lists. For most such columns that is belt-and-braces (serde ignores a field the form does not have), but a `Bytea` column **does** have a form field, so the filter is what actually stops its exported mojibake being written back. |
| A model with no settable column at all | Refused, with a warning. An empty form decodes *any* row, so such an importer could only ever commit rows of database defaults — and a file with any header would do it. Reached when every column is an `Attachment`, a `Bytea`, or `--default`ed. |
| A **non-nullable** `Bytea` column | The import is **refused** for the whole model, with a warning naming the column. The import must filter the column out (see the row below), but the generated form declares a non-nullable `Bytea` as a bare `String` with no default — so a filtered row would fail "missing field" and the importer could never import anything. Make it `Option<Bytea>`, drop it, or import it by hand. This is the same shape as the `#[encrypted]` refusal: a column the import cannot set that the form nonetheless requires. |
| `Bytea` column (nullable) | **Not importable.** The export renders it with `String::from_utf8_lossy`, so any non-UTF-8 byte is already a U+FFFD replacement character in the file — writing that back would silently replace a binary column with mojibake. It is listed among the columns the import cannot set, and supplying a value for it raises the report's discarded-column alert. A reversible encoding (base64/hex) would have to change the export too; that is out of this slice. |
| `Attachment` column | Not importable — a storage key in a cell is not a file. The column is left NULL; upload the file through the record's own edit form afterwards. |
| `datetime` column | Round-trips, naive and tz-aware alike: the export writes chrono's `Display` form (a space between date and time, plus a zone name for a tz-aware column) and an `--import` scaffold's `parse_local_datetime` accepts both shapes alongside the browser's `datetime-local` `T` form. Without the flag the helper is unchanged. |
| A value that looks like a formula | Round-trips too. The export prefixes an apostrophe to a text cell beginning `=`, `+`, `-`, `@`, TAB or CR; the import strips it back off — but only in exactly that shape, so `'tis` is left alone. A value someone really typed as `'=x` is indistinguishable from a guarded `=x` in the file and resolves to the latter; drop the `csv_unguard_cell` call if you would rather store the apostrophe. |
| `slug` column | Imported verbatim from the file. The blank-slug derivation `create` performs needs a database probe, which the parse closure (a synchronous callback) cannot do — so a blank slug reaches the insert as-is and its `UNIQUE` index reports the collision per row. A file exported from this app already carries its slugs. |
| Route vs `/<plural>/{id}` | The static `import` segment outranks the `{id}` parameter, the same way `/<plural>/new` already does. With a `slug` route key the DERIVED-slug guard treats `"import"` as taken, so a post titled "Import" gets `import-2` — but a slug submitted explicitly (including one imported from a CSV column) is not rejected, and a record whose slug is literally `import` is unreachable at its own URL. Same caveat `new` and `export.csv` already carry. |
| Very large files | Out of scope for this slice — it is the synchronous request-time flow. Raise `MAX_IMPORT_BYTES` only so far; past that, move the parse into a background job. |

`--import` is honoured wherever the CSV export is (it shares that gate, because
it shares that `CsvSchema` impl): the plain `repo.list` index — including
`--live-validation` — and the owner-scoped `repo.list_scoped` one. Passing it to
an `--api`, `--live`, `--sharded` or owner-scoped `--live-validation` scaffold —
or to a model with an at-rest `#[encrypted]` column — generates nothing and
prints a warning naming the reason and what to drop;
`autumn_web::data::csv::import_csv` is still there for a hand-written route.

It composes with `--i18n` (every string on both pages comes from the bundle),
`--searchable`, `--soft-delete`, `--belongs-to` and `--counter-cache`, and it
enables autumn-web's `multipart` feature (removed again by `autumn destroy
scaffold` unless other code still uses it).

### Trash, Restore and Purge (`--soft-delete`)

`--soft-delete` already turned the delete button into a `deleted_at`
stamp and gave the repository `restore` / `purge` / `with_deleted` /
`only_deleted` (see [soft delete](./soft-delete.md)). A standard HTML
scaffold generated with the flag now also ships the recovery UI those
methods were waiting for (issue #1332) — zero lines of user code:

- A `#[secured] #[get("/<plural>/trash")]` page listing the deleted rows
  through the repository's `page_only_deleted` (the paginated form of the
  `only_deleted` scope). The list handler never writes a `deleted_at`
  filter of its own, so "deleted" stays defined in exactly one place.
- A **Trash** link next to "New …" in the index's page furniture.
- Per row: a **Restore** submit posting to
  `POST /<plural>/{id}/restore`, and a **Purge** control posting to
  `POST /<plural>/{id}/purge` behind
  [`confirm_action`](../../autumn/src/widgets.rs)'s server-rendered dialog
  (titled per row, so the person confirming an irreversible delete can see
  which record it is). Both carry the CSRF hidden field.
- A `Deleted At` column showing each row's `deleted_at` stamp, so the page
  answers "when did this go?" as well as "what went".
- With a Trash page in the app, the delete button's flash becomes
  `<Model> moved to Trash` rather than `<Model> deleted` — it is now the
  only thing that tells the user the record is recoverable and where.
- A generated `tests/<name>.rs` case walking the whole lifecycle: create
  → delete (soft) → in Trash and out of the index → restore → back in the
  index and out of Trash → purge → gone from both.

Contract of the generated handlers:

| Situation | Behaviour |
| --------- | --------- |
| Restore | Clears `deleted_at` via the repository's `restore`, flashes `<Model> restored`, and 303s back to the Trash page. The row reappears in the index, which filters `deleted_at IS NULL`. |
| Purge | Hard-deletes via the repository's `purge` — the **only** hard delete in the generated app — then flashes and 303s back to Trash. |
| A row that is not in the trash | Both handlers load their target with `deleted_at IS NOT NULL` first and answer **404** otherwise. So a crafted `POST /<plural>/{id}/purge` can never hard-delete a live row, and neither action ever reports success for a no-op. |
| Record policy wiring on (an owner column, the default) | The loaded row is authorized with the same `"delete"` action `destroy` uses. Moving a row into or out of the trash is the same authority as deleting it, and `Policy::can` fails closed on action names it does not know — so a bespoke `"restore"` action would deny every request against a hand-written policy. |
| Confirmation | A server-rendered `<dialog>`, not an inline `onclick` handler: the default CSP is `script-src 'self'` with no `'unsafe-inline'`, so an inline confirm is blocked by the browser and the form would submit with no prompt at all. The dialog is driven by the `autumn-widgets.js` the generated layout already loads, with a `<noscript>` fallback that keeps Purge reachable without JavaScript. |
| Connection use | Each handler `drop`s its `Db` extractor after the guard load and before the repository call, so it holds **one** pooled connection at a time — no stall on `database.pool.max_size = 1`. |
| Route vs `/<plural>/{id}` | The static `trash` segment outranks the `{id}` parameter in the router, the same way `/<plural>/new` already does. |
| `--slug` route key | Restore and Purge key off the slug like the rest of the resource (`POST /<plural>/{slug}/restore`); the repository call still uses the loaded row's `id`. A *derived* slug of `trash` is treated as taken (and suffixed `trash-2`) alongside `new` and `search`, so a record titled "Trash" is not shadowed by the trash view. |
| Purge, concurrently restored | The membership check and the delete run on separate connections (the repository takes its own), so a `restore` committing in between means the purge hard-deletes a row that just came back. Nobody reaches the delete without the row having been trashed; if losing that race matters, the emitted handler carries a comment showing the filtered `diesel::delete` that closes it. Note that hand-rolling that delete gives up `purge`'s tenant filter and counter-cache decrement — keep both if your model uses either. |
| Purge, row still referenced | `purge` is a plain hard `DELETE`. A child row holding a `REFERENCES` foreign key — **including a soft-deleted one** — makes the database refuse, and the generated app answers 500. If your schema has children that can outlive a purge, declare `dependent(...)` on the association or add `ON DELETE` to the constraint. |
| `--counter-cache` parents | `restore` goes through the repository, so it re-increments the parent's counter — but the generated `destroy` writes `deleted_at` with raw diesel and never decremented it in the first place, so a Delete → Restore round trip inflates the count. That asymmetry is in the generated `destroy` (it predates the Trash view); route it through `repo.delete_by_id` if you use both flags together. |
| Gated off | The generator says so, and why, on stderr at generation time — a `--soft-delete` scaffold that gets no Trash view never fails silently. |

Emitted **only** with `--soft-delete`, and only on the standard HTML
path. Not emitted for `--live`/`--live-validation` (a restore goes
through the repository's `restore`, not the broadcasting `save`, so the
SSE list would never learn the row came back), `--sharded`
(`page_only_deleted` refuses to fan out, so a trash page would silently
show one shard's deletions), an **owner-scoped** index (there is no
owner-filtered deleted-rows scope to read through, and re-deriving the
owner filter by hand in a second list handler is how a list endpoint
leaks another user's rows), or `--api` (no HTML at all). Every one of
those variants — and every scaffold generated without `--soft-delete` —
is byte-identical to its pre-#1332 shape.

Bulk restore/purge, retention/auto-purge scheduling, and cascading
restore across associations are deliberately out of scope; compose them
with the bulk-actions widgets, `#[scheduled]`, and your own policy.

### Translatable views (`--i18n`)

Autumn ships the whole Fluent stack — the `t!(locale, "key")` macro with
compile-time key validation, `Locale::t()`, the `i18n/<tag>.ftl`
convention, fallback chains, and an `Accept-Language` `Locale` extractor
(see [i18n](./i18n.md)). `--i18n` is what wires the scaffold into it
(issue #1349), so a generated resource is translatable the moment it
exists instead of needing every English string hand-replaced:

```bash
autumn generate scaffold Post title:String body:Text published:bool --i18n
```

- Every user-facing string in the generated views — page titles, `h1`
  headings, buttons, links, index column headers, show-page property
  labels, form control labels, enum options and select placeholders,
  empty-state copy, the delete-confirm prompt, and the one-shot flash
  notices — is emitted as a `t!(locale, "key")` lookup instead of a
  literal. That includes the labels the shared widgets supply by default:
  the pager's Previous/Next, the bulk-delete button, the purge dialog's
  Cancel. A nullable `bool` is covered too — the form derive fills its
  tri-state select with a hardcoded `— Unset —`/`Yes`/`No`, and the
  generated `override_field` replaces those with `common.select.unset` /
  `common.yes` / `common.no` while keeping the submitted values. An
  `Attachment` column's meta line beside the download link is one
  pattern rather than a stray translated word — `common.attachment.meta =
  ({ $media }, { $size } bytes)` — so the media type and byte count
  interpolate as arguments and a translator owns the parentheses, the comma
  and the unit noun.
- **Two widgets are not covered yet**, both because they build their text
  inside autumn-web from arguments that carry no label seam. A `richtext`
  column's field label translates, but `rich_text_area`'s own chrome — the
  toolbar's group label and per-control names, the "Markdown supported…"
  hint, and the preview heading — stays English; and a `:states(…)` column's
  `transition_controls` keeps its `Mark as …` buttons and `… transitions`
  group label in English. Unlike the pager and bulk-delete widgets, these
  two are free functions with no label setters to call, so covering them
  needs new autumn-web API (a label per transition edge, and per toolbar
  control). Scaffolding either column with `--i18n` warns and names it,
  rather than leaving you to find it in the browser.
- **Validation messages stay English.** A field label translates; the
  inline error under it after a rejected submission does not.
  `#[validate(...)]` accepts a `message`, but `validator` takes it as a
  compile-time literal, so a runtime lookup cannot go there — and a rule
  with no message renders as `validation failed: <code>`. Reaching these
  means mapping error *codes* to lookups before the changeset is built,
  and that conversion happens inside autumn-web, so it needs a seam there
  rather than a generator change. Scaffolding with `--validate` under
  `--i18n` warns.
- Each view-rendering handler takes the `Locale` extractor as its **first**
  parameter (`Locale` is a `FromRequestParts` extractor, and axum requires
  the one body-consuming argument to stay last).
- `i18n/en.ftl` is created — or merged into, if it already exists — with
  every key the views reference and **only** those, valued with the English
  the plain scaffold renders. So an `en` app looks exactly like a
  non-`--i18n` one, adding French means translating that file rather than
  editing Rust, and `autumn i18n check --strict` passes on the result: no
  key referenced without a definition, and no definition nobody references.
- The project is wired so those lookups actually resolve: autumn-web's
  `i18n` feature is enabled, `[i18n] default_locale = "en"` is added to
  `autumn.toml` if it has no `[i18n]` block, and `.i18n_auto()` goes into
  the `AppBuilder` chain in `main.rs`.

Keys are split so a translator sees each string exactly once:

| Kind | Examples | Written |
| ---- | -------- | ------- |
| Shared chrome | `common.create`, `common.save`, `common.back`, `common.edit`, `common.delete`, `common.show`, plus the widget defaults `common.pagination` / `common.previous` / `common.next` / `common.delete.selected` | Once per project, under one header. A second resource reuses the block rather than duplicating it per model. |
| This resource's strings | `post.new`, `post.name.plural`, `post.index.title`, `post.index.empty`, `post.show.title`, `post.edit.title`, `post.delete.confirm`, `post.field.<column>`, `post.flash.*` | Once per resource, under a marked comment block. |

**What interpolates and what does not.** A row key or a count travels as a
Fluent argument, so a translation can *position* it: `post.show.title =
Post #{ $id }`, `post.flash.bulk_deleted = Deleted { $count } posts`.
Positioning is all it can do — see the pluralization limit below. The
model's **name** never does. `New { $resource }` would look like tidy
reuse, but it hands the translator a sentence whose article and adjective
must agree with a noun they cannot see — French *Nouveau*/*Nouvelle*,
German *Neuer*/*Neue*/*Neues*, case inflection in Slavic languages. So
"New Post" is a per-resource key (`post.new`), which costs one line in the
bundle and is actually translatable.

Notes and limits:

- **One key per field, three surfaces.** `post.field.title` labels the
  index column header, the show-page property row, and the form control
  alike, so one translation serves all three. The English value is the
  Title Case the form derive already uses, which means a **multi-word**
  column's show-page label normalizes from "Author name" to "Author Name"
  under `--i18n`. Single-word columns are unaffected.
- **`{ $count }` positions a number; it does not pluralize.** The bundle
  loader substitutes `{ $name }` placeables and nothing else — Fluent
  selectors (`{ $count -> [one] … *[other] … }`), terms and `NUMBER()` are
  carried through as literal text, not evaluated (`autumn/src/i18n.rs`).
  So `post.flash.bulk_deleted` can put the count wherever a language needs
  it, but a translator cannot vary the noun with it, and a language with
  more than two plural categories — Russian, Polish, Arabic — has no form
  that is right for every value. Write the value so it reads acceptably for
  any count ("Deleted: { $count }"), or handle the plural in application
  code and pass the finished string. Lifting this needs selector support in
  the loader, which is framework work rather than something the generator
  can emit around.
- **Re-running never clobbers a translation.** An existing value is left
  exactly as edited, and a new key from a later run lands inside its
  resource's block rather than at the end of the file. A `--force`
  regeneration that *drops* a field or a flag also prunes the keys for
  those surfaces — nothing references them any more, so `autumn i18n check
  --strict` would fail on them — while carrying over the values, comments,
  and blank lines of the keys that remain. The shared chrome is never
  pruned this way: another resource may still be using it.
- **The bundle `t!` validates against is kept in step.** `t!`'s
  compile-time key check does not read `autumn.toml`. It opens
  `AUTUMN_I18N_FILE`, or else `i18n/$AUTUMN_I18N_DEFAULT_LOCALE.ftl` with
  the locale defaulting to `en`, and degrades to a runtime lookup only when
  that file is absent. So if your project has such a bundle *and* a
  different default locale, the generator writes the same English into both;
  otherwise `cargo check` would fail with a `compile_error!` per lookup. The
  path is resolved the way the macro resolves it, including an `[env]` table
  in the project's own `.cargo/config.toml` — which is where a setting every
  build needs usually lives.
- **Keys go to the bundle the app actually reads.** A project whose
  `autumn.toml` says `default_locale = "fr"` and `dir = "translations"`
  gets `translations/fr.ftl`, because that is what its lookups resolve
  through — in any of TOML's spellings (`[i18n]`, `i18n = { … }`, or
  `i18n.default_locale = …`). Only a project that configures no i18n at all
  gets a block written for it (`default_locale = "en"`, `i18n/`).
- **Profile overlays are reported, not guessed at.** `[profile.prod.i18n]`
  can repoint `dir`/`default_locale`, and the app resolves the layered
  config at startup — so the generator writes the *base* bundle and warns
  which other path a deploy will actually read. Writing English into a
  profile that exists precisely because it serves another locale would be
  its own bug.
- **An app that installs its own bundle keeps it.** A `main.rs` calling
  `.i18n(my_bundle())` — embedded files, a translation-management service,
  memory — is left alone, and the generator warns that the keys it just
  wrote to disk will not reach that bundle.
- **`--embed` builds get the bundle too.** `.i18n_auto()` loads from disk,
  so an `autumn build --embed` binary would not be self-contained without
  it: the generator adds the same `EMBEDDED_LOCALES` static and
  `.embedded_locales(...)` call (behind `embed-assets`) that `autumn new
  --with-i18n` emits.
- **The Docker image gets the bundle.** `.i18n_auto()` reads the default
  locale's file at startup and *panics* if it is missing, so the generator
  adds a `COPY` for the configured bundle directory to both stages of a
  generated `Dockerfile`.
- **`autumn destroy scaffold … --i18n`** removes that resource's marked key
  block — the header, everything down to its `# — end <Model> —` marker
  (so a blank line or a note you leave among the keys does not move the
  boundary), and nothing outside it, so a hand-authored
  `post.email.subject` of your own survives —
  and leaves the file, the `i18n` feature, `[i18n]`, and `.i18n_auto()` in
  place, since those are project-level and shared. The `common.*` chrome
  survives as long as another `--i18n` resource still references it; when
  the last one goes, so does the chrome, keeping `autumn i18n check
  --strict` green.
- **Composes with** `--searchable`, `--soft-delete`, `--sharded`, and the
  CSV export — the strings those surfaces add (search box and its
  placeholder, Trash/Restore/Purge, "Export CSV", empty-state copy, the
  bulk-delete button) are translated too. `--api` renders no labels, so the
  flag is a no-op there: the output is byte-identical and no `.ftl` is
  written.
- **Refused with** `--live`, `--live-validation`, and `--belongs-to`. The
  first two render list rows and inline-validation fragments outside any
  request (so there is no `Locale` in scope), and the third splices markup
  into the *parent* resource's already-generated `show` handler, whose
  signature this generator does not own. Half-translated views under a flag
  that promises translatable output are worse than a refusal, so the
  generator says so and writes nothing. The nesting refusal follows the
  *relationship*, not the flag: `--belongs-to` is typed once, so a later
  `generate … --force --i18n` that omits it is still refused, naming the
  parent routes file the nesting was recovered from. `autumn destroy
  scaffold` that child first (it removes the parent-side section) to
  scaffold it flat with `--i18n`. A resource named `Common` is refused too —
  its keys would collide with the shared chrome namespace.
- **Without `--i18n`, output is byte-for-byte unchanged.** The default
  scaffold stays zero-i18n-config.

One thing is deliberately **not** translated: the `UNIQUE_CONSTRAINTS`
table's `"has already been taken"`. It is a `const`, so it cannot hold a
runtime lookup, and it is a validation message — which the issue scopes
out alongside the framework's own error pages.

Translating the framework's own strings (error pages, validation
messages), locale-prefixed routing ([#1251](./i18n.md)), missing/unused
key linting (`autumn i18n check`), and machine-translating non-English
`.ftl` files are all out of scope here.

Metadata flags let you keep common model and repository polish in the
generation step:

```bash
autumn generate scaffold Bookmark url:String title:String tag:String alive:bool \
  --index url \
  --index tag \
  --validate url=url \
  --validate title=length:min=1,max=200 \
  --default alive=true \
  --query find_by_tag:tag \
  --query find_by_alive:alive
```

| Flag | Effect |
| ---- | ------ |
| `--index FIELD` | Adds `#[indexed]` and `CREATE INDEX idx_<table>_<field> ...`. Repeatable. |
| `--validate FIELD=RULE` | Adds `#[validate(...)]` and the `validator` dependency. Supported rules: `url`, `email`, and `length:min=N,max=N` on `String` / `Text` fields. |
| `--default FIELD=VALUE` | Adds `#[default]` and a SQL `DEFAULT` for bool, string/text, integer, and float fields. `i32` defaults must fit PostgreSQL's `INTEGER` range. Defaulted fields are omitted from generated HTML forms and update columns because the model macro keeps them out of `NewX`. |
| `--query METHOD:FIELD` | Adds a derived repository method such as `find_by_tag(tag: String) -> Vec<Model>`. The `find_by_` suffix must match `FIELD`. |
| `--api` | Generates a JSON API-only scaffold (skips HTML routes/templates, registers 5 REST JSON routes, and generates a JSON-based smoke test). |
| `--i18n` | Emits translatable views: every view string becomes a `t!(locale, "key")` lookup, view handlers take the `Locale` extractor, and `i18n/en.ftl` is back-filled with the English. See [Translatable views](#translatable-views---i18n). |
| `--import` | Emits a CSV import route: an upload form, a dry-run preview with per-row errors, and a confirmed commit. See [Import CSV with a dry-run preview](#import-csv-with-a-dry-run-preview---import). |

| Generated file                        | Existing concept it maps to                                                                |
| ------------------------------------- | ------------------------------------------------------------------------------------------ |
| `src/models/<name>.rs`                | [`#[autumn_web::model]`](../../autumn-macros/src/model.rs)                                 |
| `src/repositories/<name>.rs`          | [`#[autumn_web::repository]`](../../autumn-macros/src/repository.rs)                       |
| `src/routes/<plural>.rs`              | [`#[get]`/`#[post]` route macros](../../autumn-macros/src/route.rs) returning `Maud Markup` |
| `src/main.rs` `routes![…]`            | The [`routes!` collection macro](../../autumn-macros/src/routes_macro.rs)                  |
| `migrations/<ts>_create_<plural>/`    | Diesel migrations                                                                          |
| `src/schema.rs`                       | Diesel `table!` blocks                                                                     |
| `tests/<name>.rs`                     | Standard `cargo test` integration test                                                     |

### Shipped example

The [`examples/bookmarks`](../../examples/bookmarks) app is regenerated from
the current scaffold shape:

```bash
autumn new bookmarks
cd bookmarks
autumn generate scaffold Bookmark url:String title:String tag:String alive:bool \
  --index url \
  --index tag \
  --validate url=url \
  --validate title=length:min=1,max=200 \
  --default alive=true \
  --query find_by_tag:tag \
  --query find_by_alive:alive
```

It is the reference for what `autumn generate scaffold` produces in practice
after a user makes ordinary app-specific edits. The committed follow-up diff is
intentionally small and documents which gaps are outside the generic generator:

| Bookmarks addition | Disposition |
| ------------------ | ----------- |
| Tailwind layout, htmx delete buttons, and public local-demo write forms | UI and access-policy choices; replace the generated route templates. |
| Hourly `#[scheduled]` link checker | Operational workflow; generate or write a task separately. |
| Mounting `POST`/`PUT`/`DELETE` JSON API routes | Application policy; scaffold keeps only read APIs registered by default. |

### Reusable scaffold config (`autumn.generate.toml`)

Long scaffolds with many metadata flags can be checked in as a TOML file
so the intent is reviewable and reproducible without spelunking shell
history. Create a file at any path — `autumn.generate.toml` is the
conventional name — with one `[scaffold.<ResourceName>]` section per
resource:

```toml
[scaffold.Bookmark]
fields      = ["url:String", "title:String", "tag:String", "alive:bool"]
indexes     = ["url", "tag"]
validations = ["url=url", "title=length:min=1,max=200"]
defaults    = ["alive=true"]
queries     = ["find_by_tag:tag", "find_by_alive:alive"]
api         = true # Optional: JSON API-only scaffold
```

Pass the file with `--config`:

```bash
autumn generate scaffold Bookmark --config autumn.generate.toml
```

All the same keys are supported as their CLI counterparts — see the
metadata flags table above for the accepted syntax of each.

**Precedence rules (CLI wins):** if a CLI flag is supplied alongside
`--config`, it completely replaces the corresponding TOML list for that
key. An empty CLI slice (i.e. the flag was not passed) falls back to the
TOML value. This matches normal CLI ergonomics where the explicit flag is
always authoritative:

| Scenario | Effective value |
|---|---|
| TOML only | TOML list |
| CLI only (no `--config`) | CLI list |
| Both, CLI non-empty | CLI list (TOML ignored for that key) |
| Both, CLI empty / flag absent | TOML list |

This applies independently to each key: you can keep `fields` and
`validations` from TOML while overriding `indexes` on the CLI for a
one-off variant.

The config is additive, not a replacement — existing CLI flags always
work without a config file, and the config never changes the output of
any previously working invocation.

### Slow live scaffold verification

The CLI test suite includes two ignored scaffold checks:

```bash
# Compile-check the generated app and its generated smoke test (CLI flags).
cargo test -p autumn-cli --test generate generated_scaffold_cargo_checks -- --ignored --exact

# Compile-check a config-file-driven scaffold (--config flag).
cargo test -p autumn-cli --test generate generated_scaffold_config_cargo_checks -- --ignored --exact

# Boot Postgres, run `autumn migrate`, start the generated server, and
# verify GET /posts and GET /api/posts over real HTTP.
cargo test -p autumn-cli --test generate generated_scaffold_serves_posts_index_and_json_api -- --ignored --exact
```

The live HTTP test requires Docker access for the Postgres testcontainer and
the `diesel` CLI on `PATH`, because `autumn migrate` delegates to
`diesel migration run`.

### WebAuthn native dependency note

`autumn generate auth --passkeys` enables the `webauthn` feature and adds
`webauthn-rs`. That dependency currently builds through OpenSSL. On Ubuntu CI
the system OpenSSL toolchain is already available, but Windows developers need
to install the OpenSSL libraries through `vcpkg` and set `VCPKG_ROOT` so
`openssl-sys` can find them.

The release SemVer gate checks `autumn-web` optional public feature APIs, so this
native dependency must be present on machines that run `scripts/check-semver.sh`
locally.

## `autumn generate wizard`

Multi-step forms where each step is validated before the user advances.
Session-backed: step data survives page refreshes and back-button navigation
without requiring the user to re-enter earlier steps.

```bash
autumn generate wizard checkout shipping payment review
```

Produces:

```
src/wizards/checkout.rs        # step structs + GET/POST handlers + confirm/commit/cancel
src/wizards/mod.rs             # pub mod checkout;  (created or appended)
tests/checkout_wizard.rs       # ignored integration test skeletons
```

Step names must be valid Rust identifiers (letters, digits, underscores;
no hyphens). The names `confirm`, `commit`, and `cancel` are reserved. A
minimum of two steps is required.

For each step the generator emits:
- A `{PascalStep}Form` struct with `Serialize`, `Deserialize`, `Validate`, and `Default`.
- A `GET /{name}/{step}` handler that guards, re-populates from session, and renders the form.
- A `POST /{name}/{step}` handler that validates, saves to session, and redirects — or returns 422 with errors.

Plus three fixed handlers:
- `GET  /{name}/confirm` — summary page; guards that all steps are complete.
- `POST /{name}/commit` — assembles all step data, performs the write, clears session.
- `POST /{name}/cancel` — clears session state and redirects.

Mount the routes in `src/main.rs` and add `mod wizards;`.

See the [Wizards guide](./wizards.md) for the full runtime API reference and
the [`examples/bookmarks`](../../examples/bookmarks) app for a worked example.

## Common flags

Every generator accepts:

- `--dry-run` — print the file plan and exit. Nothing is written; existing
  files are not touched. Useful for previewing what the generator will do.
- `--force` — overwrite existing files. By default, the generator refuses
  to clobber and surfaces a `would overwrite <path>` error listing every
  collision. `mod.rs` and `schema.rs` are always treated as modify-in-place
  edits and don't trigger collisions.

## Undoing a generator: `autumn destroy`

`autumn destroy <thing> <the same arguments>` reverses a matching `generate`.
It removes the files that invocation owns. It also takes that invocation's
edits back out of the files it shares with other resources.

`destroy` refuses to delete a file you have edited. To tell your edits from its
own output, `generate` records a digest of every file it **owns** in
`.autumn/generated.toml` — the files it creates, not the shared files it edits
in place. `destroy` deletes an owned file when the content matches that digest,
or matches what the current templates render. It reports `Diverged` and stops
when the content matches neither.

Each entry also records the inputs that produced it — the command's arguments,
a fingerprint of the `autumn.generate.toml` they resolve from, and the resolved
database backend — so the digest only counts when all three match. That is what
`destroy` asks for anyway. Changing the recipe, or moving the project between
SQLite and Postgres, drops the baseline for every resource, back to comparing
against the current templates: a stale baseline is worse than no baseline.
`autumn destroy model Post` after `autumn generate model Post title:String`
renders a different model and is refused, rather than deleting the file while
the `schema.rs` and `Cargo.toml` reverts, which are derived from those fields,
quietly do nothing. It also keeps one command from claiming another's files:
`autumn new --starter` writes through the same machinery, and no generator
owns what it wrote.

Commit `.autumn/generated.toml`. It is the baseline a later checkout compares
against. Without it, `destroy` can only compare against the current templates —
so a CLI upgrade that changed a template makes every untouched file that
generator wrote look edited.

Two cases need `--force`:

- The project has no manifest entry for the file — it was generated before
  Autumn wrote the manifest — **and** the template has changed since. Prefer
  re-running the original generator command with `--force` first (for example
  `autumn generate model Post title:String --force`): that rewrites the file
  and records its digest, and the destroy then works without `--force`.
- You edited the file, and you want it deleted anyway.

`--force` skips the content check. It does not skip the applied-migration
guard: `destroy` never removes migration files that a configured database
records as applied.

A file that several resources share (a mailer's `_layout.html`, say) is
never a hard error and is never force-deleted. `destroy` removes it only
when it is provably this generator's own output and no sibling resource is
left in its directory; otherwise it warns and leaves it in place.

`destroy` also removes each entry it acted on from the manifest, and deletes
the manifest once it holds nothing. Deleting or hand-editing the manifest
loses the baseline for the entries you removed — nothing fails, but those
files fall back to comparing against the current templates only.

## `autumn db pull` — scaffold models from an existing database

The generators above are greenfield: you describe a table with the `name:Type`
DSL and they emit a brand-new one. If you already run a Postgres database,
`autumn db pull` goes the other direction — it introspects your live schema and
emits the matching Autumn artifacts, so you can adopt autumn-web incrementally
instead of rewriting every table by hand.

```bash
# Pull every table in the public schema:
autumn db pull

# Pull specific tables, and also emit a #[repository] per table:
autumn db pull posts comments --with-repository
```

It connects using the same resolution `autumn migrate` uses
(`database.primary_url` / `database.url` in `autumn.toml`, or
`AUTUMN_DATABASE__PRIMARY_URL` / `AUTUMN_DATABASE__URL` / `DATABASE_URL`), and
for each selected table emits, through the same file-emission machinery as the
other generators:

- a `#[model]` struct in `src/models/<name>.rs`,
- a `diesel::table!` block in `src/schema.rs`,
- the `pub mod <name>;` aggregator line, and `mod models;` / `mod schema;`
  declarations in `src/main.rs`,
- optionally, a `#[repository(Model)]` trait in `src/repositories/<name>.rs`
  (with `--with-repository`).

This is **read-only**: no migration is written and no data is touched — the
tables already exist. Column types are the inverse of the [field-type DSL
table](#the-field-type-dsl) (`int8` → `i64`, `text` → `String`, `timestamptz`
→ `chrono::DateTime`, …); nullable columns become `Option<T>`. The primary key
is annotated `#[id]` and a `created_at` column is annotated `#[default]`, so a
table created by `autumn generate model` and then re-derived by `db pull`
produces a field-for-field equivalent model. A column whose SQL type is outside
the supported set fails with an error naming the column rather than silently
dropping it.

`--dry-run`, `--force`, and collision-refusal behave exactly as in the
`autumn generate` family: existing model/repository files are not clobbered
without `--force`, and `mod.rs` / `schema.rs` are modified in place. Under
`--force`, an existing `schema.rs` block for a pulled table is replaced with the
freshly introspected one so the model and schema can't drift apart on a re-pull.

Brownfield specifics:

- **Framework tables are skipped.** An unscoped `autumn db pull` ignores
  Autumn's own tables (`autumn_*` / `_autumn*`, `api_tokens`, …) so it works on
  a database that has already run `autumn migrate`. Name a table explicitly to
  pull it anyway.
- **Defaults and read-only columns.** A `created_at` column with a database
  default, and stored generated columns (`GENERATED ALWAYS AS … STORED`), are
  annotated `#[default]` so they stay out of inserts/updates. An ordinary
  column with a default (e.g. `status TEXT DEFAULT 'draft'`) stays settable.
- **Irregular plurals.** When the table name isn't the model macro's naive
  `Struct + "s"` inference (e.g. `people`, `categories`), `db pull` emits an
  explicit `#[autumn_web::model(table = "...")]` so the model compiles against
  the generated schema block.
- **`--with-repository` requires the `id`/`i64` PK convention.** Tables keyed by
  a `uuid`, a non-`id` column, or a composite key still get a model, but the
  repository is skipped (the `#[repository]` macro assumes an `i64 id`).
- **Unsupported shapes fail loudly.** Tables without a primary key, columns
  whose names aren't valid identifiers (e.g. `type`), unmapped SQL types, and
  two tables that collapse to the same model module all stop the pull with a
  clear error instead of emitting broken code.

Out of scope for `db pull`: foreign-key/association inference, generating routes
or admin adapters, non-Postgres backends, and SQL views / materialized views /
partitioned tables.

## What's intentionally not here

The generators are deliberately scoped to one resource per invocation and
to the existing public macro surface. Out of scope (track separately if
you need them):

- Authentication scaffolding. Auth has its own session, CSRF, and
  `#[secured]` story; bundling it here would balloon scope.
- Generators for optional plugin crates. Those plugins ship their own
  generators on their own timeline.
- Harvest workflow scaffolding. `autumn-harvest` is a companion workflow
  project with its own release train, so core web generators do not depend on
  it.
- Custom user-provided templates / template overrides.
- Test scaffolding beyond the single smoke test.
- Multi-resource scaffolds (`autumn generate scaffold Blog Post Comment`).
  One resource per invocation; chaining is the user's job.
