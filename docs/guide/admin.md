# Admin Panel

Autumn ships a first-party `autumn-admin-plugin` that gives you a
server-rendered CRUD back-office at `/admin` (configurable). The
`autumn generate admin` command turns an existing `#[model]` into a
fully-wired `AdminModel` without hand-writing the adapter.

## From `autumn new` to a working admin page

```bash
# 1. Create the project
autumn new my-app
cd my-app

# 2. Generate the model + migration + repository
autumn generate scaffold Post title:String body:Text published:bool

# 3. Generate the admin adapter for the same model
autumn generate admin Post title:String body:Text published:bool

# 4. Register the plugin and the generated adapter in src/main.rs
```

After step 3 you have:

```
src/admin/post.rs        # AdminModel implementation for Post
src/admin/mod.rs         # pub mod post;
tests/post_admin.rs      # smoke tests (anonymous reject + admin list + create)
```

### Wire up the plugin in `src/main.rs`

Add `autumn-admin-plugin` to `Cargo.toml`:

```toml
[dependencies]
autumn-admin-plugin = { workspace = true }
```

Then register the plugin and your new adapter:

```rust
mod admin;

use autumn_admin_plugin::AdminPlugin;
use admin::post::PostAdmin;

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .plugin(
            AdminPlugin::new()
                .require_role("admin")
                .register(PostAdmin),
        )
        .routes(routes![...])
        .run()
        .await;
}
```

Visit `http://localhost:3000/admin/posts` after running `autumn migrate && autumn dev`.

## What the generator produces

`autumn generate admin Post title:String body:Text published:bool` creates
`src/admin/post.rs` with a `PostAdmin` struct that implements every
`AdminModel` method:

| Operation | Admin UI action |
|-----------|----------------|
| `list`    | Paginated table with search, sort, and filters |
| `get`     | Detail / edit form pre-populated with the current values |
| `create`  | New record form |
| `update`  | Save changes to an existing record |
| `delete`  | Single-record delete |
| `execute_action` | Bulk-delete (default) — override for custom bulk actions |

Field types are mapped to admin widgets automatically:

| Field type         | Admin widget           | Default extras                   |
|--------------------|------------------------|----------------------------------|
| `String`           | Text input             | Searchable                       |
| `Text`             | Textarea               | Searchable, hidden from list     |
| `i32` / `i64`      | Number input           |                                  |
| `bool`             | Checkbox               | Filterable                       |
| `f32` / `f64`      | Number input           |                                  |
| `Uuid`             | Text input             | Read-only                        |
| `NaiveDateTime` / `DateTime` | Date-time picker | Read-only, optional       |
| `Bytea`            | Hidden                 | Excluded from update             |

The `id` field is always hidden, read-only, and excluded from the list table.

## Customising generated field metadata

Pass flags after the field tokens to override individual field behaviour
**without editing generated internals**:

```bash
autumn generate admin User \
  email:String \
  password_hash:String \
  role:String \
  created_at:DateTime \
  --password password_hash \
  --exclude password_hash \
  --select role=admin,editor,viewer \
  --readonly created_at
```

| Flag | Effect |
|------|--------|
| `--hidden FIELD` | Render as `AdminFieldKind::Hidden` (shown in detail, not editable) |
| `--readonly FIELD` | Add `.readonly()` — shown in all views, blocked from create/edit forms |
| `--password FIELD` | Render as `AdminFieldKind::Password` (write-only, never displayed) |
| `--select FIELD=val1,val2,...` | Render as a dropdown; labels are title-cased from values |
| `--exclude FIELD` | Omit the field from the generated adapter entirely |

Multiple flags of the same kind are repeatable:

```bash
autumn generate admin Post title:String slug:String body:Text published:bool \
  --readonly slug \
  --readonly published \
  --exclude body
```

## Search, filter, and sort

The generator derives sensible defaults:

- **Text (`String`)** — included in ILIKE full-text search.
- **Textarea (`Text`)** — searchable but hidden from the list table.
- **Boolean** — filterable with `true`/`false`/`yes`/`no`/`1`/`0`.
- Every field gets a sort arm in `apply_sort`; the default sort is by `id`.

To add custom search logic (compound queries, JSONB containment, etc.) edit
`apply_filters` in the generated file — it is ordinary user code.

## Security

The admin plugin enforces its own auth gate on **every route under `/admin`**:

```rust
AdminPlugin::new().require_role("admin")   // session must have role = "admin"
```

No route in `src/admin/` bypasses this check — the generated adapter only
handles the database work, not authentication. Anonymous requests are
redirected to the login page (or return 401) before any adapter method runs.

CSRF tokens are injected by the plugin into every state-changing form
(`POST /admin/{slug}`, `POST /admin/{slug}/{id}`, `DELETE /admin/{slug}/{id}`)
when the app enables CSRF protection. No changes to generated code are needed.

## Impersonation

Turn on "log in as this user" for the panel with one call:

```rust,ignore
use autumn_admin_plugin::AdminPlugin;
use autumn_web::auth::impersonation::ImpersonationGate;

AdminPlugin::new()
    .register(UserAdmin::default())
    .with_impersonation(ImpersonationGate::allow_roles(["admin"]))
```

That mounts two routes:

| Route | Guard |
|---|---|
| `POST /admin/impersonate` (body: `user_id`) | admin role + step-up (if enabled) + the gate |
| `POST /admin/impersonate/stop` | none — reverting must always work |

The revert route sits **outside** the role gate on purpose: while impersonating,
the session carries the target's role (usually none), so a gated revert would
trap the operator in the target's identity. It is self-gating — a session that
is not impersonating gets a `400` and nothing changes.

Every admin page then renders a persistent banner naming both parties with a
one-click revert. Put the same banner in your **application** layout too — that
is the surface an operator actually looks at while impersonating a non-admin:

```rust,ignore
#[get("/")]
async fn home(State(state): State<AppState>, session: Session, csrf: CsrfToken) -> Markup {
    let banner = autumn_admin_plugin::impersonation_banner_for(
        &state, &session, "/admin", csrf.token(), "_csrf",
    ).await;
    html! {
        body {
            @if let Some(banner) = banner { (banner) }
            main { "…" }
        }
    }
}
```

Add `autumn_admin_plugin::IMPERSONATION_BANNER_CSS` to that layout's stylesheet;
the plugin's own pages already include it.

Without `with_impersonation` neither route is mounted, and the underlying
primitive default-denies — see the [authentication
guide](./authentication.md#impersonation-log-in-as-this-user) for what the swap
guarantees and refuses.

## Running the generated smoke tests

Three tests are generated in `tests/<snake>_admin.rs`:

| Test | What it checks |
|------|---------------|
| `anonymous_access_is_rejected` | `GET /admin/{plural}` without a session returns 302/401/403 |
| `list_loads_for_admin_user`    | With a valid admin session the list page returns 200 |
| `create_redirects_for_admin_user` | A POST with form data and an admin session returns 200/302/303 |

Run them against a live server:

```bash
AUTUMN_TEST_BASE_URL=http://localhost:3000 \
AUTUMN_TEST_ADMIN_SESSION=<your_session_cookie> \
cargo test post_admin
```

If your app enables CSRF in all profiles, configure `autumn.toml` to skip it
in the `test` profile or supply a valid token in the form body.

## `--dry-run` and `--force`

```bash
# Preview what would be created — nothing is written
autumn generate admin Post title:String --dry-run

# Overwrite existing src/admin/post.rs after customising the scaffold
autumn generate admin Post title:String --force
```

`--dry-run` lists every file and registration change then exits. `--force`
overwrites existing `Create` targets; idempotent `Modify` actions (like
`src/admin/mod.rs`) are always safe to re-run.

## Updating an existing adapter

The generator **creates, not updates**. If you add a column to the model
and want to reflect it in the admin adapter, re-run with `--force`:

```bash
autumn generate admin Post title:String body:Text published:bool featured:bool --force
```

Or edit `src/admin/post.rs` directly — it is ordinary user code.

## Example: blog admin

The `examples/blog` app uses `autumn-admin-plugin` with a hand-written
`PostAdmin` in `src/admin.rs`. Running the generator would produce an
equivalent file:

```bash
cd examples/blog
autumn generate admin Post \
  title:String \
  slug:String \
  body:Text \
  published:bool \
  created_at:DateTime \
  updated_at:DateTime \
  --readonly slug \
  --readonly created_at \
  --readonly updated_at
```

Compare the generated output with `examples/blog/src/admin.rs` to see
the patterns the generator encodes automatically.
