//! A React + TypeScript single-page app on top of an Autumn backend, talking
//! GraphQL through a plugin, against a real Postgres model.
//!
//! The modules, in the order an evaluator should read them:
//!
//! - [`models`] — one `#[model]` struct: the `Note` row, its generated
//!   `NewNote`/`UpdateNote` types, and a `#[validate]` rule.
//! - [`hooks`] — `MutationHooks` that trim input inside the repository's
//!   transaction, on every write path.
//! - [`repositories`] — `#[repository(Note, hooks = …, api = "/api/notes")]`:
//!   generated CRUD, a derived `find_by_pinned` finder, and generated JSON
//!   REST handlers over the same rows.
//! - [`notes`] — the `async-graphql` `Query`/`Mutation` roots. Each resolver
//!   builds the repository from the pool on `AppState`, so GraphQL is one
//!   more door into the same model, not a second data layer.
//! - [`graphql_plugin`] — a small, generic [`graphql_plugin::GraphqlPlugin`]
//!   that adapts any `async_graphql::Schema` onto an Autumn app: nested
//!   router with router-local schema state, declared routes, `AppState`
//!   flowing into every execution, a contract. It knows nothing about notes.
//! - [`index`] — the Maud page shell the React bundle mounts into.
//!
//! The binary in `src/main.rs` calls [`app`]; the integration tests build the
//! same pieces through `autumn_web::test::TestApp`.

pub mod graphql_plugin;
pub mod hooks;
pub mod models;
pub mod notes;
pub mod repositories;
pub mod schema;

use autumn_web::app::AppBuilder;
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::prelude::*;
use autumn_web::{AppState, AutumnError};

use crate::graphql_plugin::GraphqlPlugin;
use crate::models::NewNote;

/// The `notes` table migration, applied on boot (`AUTUMN_ENV=development`)
/// or by `autumn migrate`. Tests apply it to their testcontainer too.
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

/// Where the [`GraphqlPlugin`] is mounted. The React client posts here.
pub const GRAPHQL_PATH: &str = "/graphql";

/// The `static/` tree — the committed React bundle under `static/app/`
/// included — baked into the binary for single-binary deploys. Only compiled
/// under the `embed-assets` feature, which `autumn build --embed` enables; a
/// dev build keeps serving from disk so `npm run build` output is picked up
/// without recompiling.
#[cfg(feature = "embed-assets")]
static EMBEDDED_STATIC: autumn_web::include_dir::Dir = autumn_web::embed_static!();

/// The page shell the React bundle mounts into.
///
/// Autumn renders the document; React renders everything inside `#root`. The
/// bundle is Vite's build output committed under `static/app/` (see
/// `frontend/vite.config.ts` for why the file names carry no hash), served by
/// the framework's standard `/static` mount, and referenced through
/// [`asset_url`] so a release build with an asset manifest gets fingerprinted
/// URLs for free.
///
/// The shell also carries the CSRF token as `<meta name="csrf-token">` when
/// the framework's CSRF layer is active (it is, by default, under the `prod`
/// profile). The CSRF cookie is `HttpOnly`, so this meta tag is how page
/// script learns the token; the client echoes it in `X-CSRF-Token` on every
/// mutation, which is the same double-submit contract the framework's htmx
/// helper follows. In dev, where CSRF is off, the extractor yields `None`
/// and the tag is simply absent.
#[get("/")]
#[public]
pub async fn index(csrf: Option<CsrfToken>) -> Markup {
    html! {
        (PreEscaped("<!DOCTYPE html>"))
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                @if let Some(csrf) = &csrf {
                    meta name="csrf-token" content=(csrf.token());
                }
                title { "Autumn Notes" }
                link rel="stylesheet" href=(asset_url("app/app.css"));
            }
            body {
                // React's mount point. `noscript` is the only server-rendered
                // content: everything else is fetched over GraphQL by the app.
                div id="root" {
                    noscript { p { "Autumn Notes needs JavaScript enabled." } }
                }
                // An external ES module satisfies the default `script-src 'self'`
                // CSP; no inline bootstrap, no nonce.
                script type="module" src=(asset_url("app/app.js")) {}
            }
        }
    }
}

/// The app's own (non-plugin) routes: the page shell plus the five JSON REST
/// handlers `#[repository(api = "/api/notes")]` generated. Mounting them next
/// to the GraphQL endpoint shows both surfaces reading — and writing — the
/// same rows through the same repository: a `POST /api/notes` is trimmed and
/// validated exactly like `createNote`, and `DELETE /api/notes/2` meets the
/// same `before_delete` hook `deleteNote` does.
///
/// `routes!` needs a fully-qualified handler path, and the route-info helper
/// it expands to is private to this crate, so the list is built here rather
/// than in `main.rs` or the tests.
#[must_use]
pub fn routes() -> Vec<autumn_web::Route> {
    routes![
        crate::index,
        crate::repositories::note_api_list,
        crate::repositories::note_api_get,
        crate::repositories::note_api_create,
        crate::repositories::note_api_update,
        crate::repositories::note_api_delete,
    ]
}

/// Build the complete application: migrations, the page shell and REST reads,
/// the GraphQL plugin at [`GRAPHQL_PATH`], and a startup seed.
#[must_use]
pub fn app() -> AppBuilder {
    let app = autumn_web::app()
        .migrations(MIGRATIONS)
        .routes(routes())
        .plugin(graphql())
        .on_startup(|state| async move { seed_if_empty(&state).await });

    // Single-binary deploys: serve `/static/*` (the React bundle) from the
    // binary when built with `autumn build --embed`. `asset_url` then resolves
    // against the manifest baked in alongside it.
    #[cfg(feature = "embed-assets")]
    let app = app.embedded_static(&EMBEDDED_STATIC);

    app
}

/// The GraphQL plugin exactly as the binary mounts it.
#[must_use]
pub fn graphql() -> GraphqlPlugin<notes::Query, notes::Mutation, async_graphql::EmptySubscription> {
    GraphqlPlugin::new(notes::build_schema()).path(GRAPHQL_PATH)
}

/// Seed two notes into an empty table so the first page load has something
/// to render and the browser smoke has something to look for.
///
/// Every instance runs its startup hooks, so on a first scaled deployment two
/// processes could both see an empty table and both insert. The check and
/// the insert therefore run in one transaction on **one** connection, behind
/// a transaction-scoped Postgres advisory lock (`pg_advisory_xact_lock`, keyed
/// the same way the framework's `Lock` keys its names): the first instance
/// seeds, the rest queue on the lock, then see rows and skip. One connection
/// matters — a session lock held on one pool slot while the seed queries
/// wait for a second would deadlock a `pool_size = 1` deployment on every
/// boot.
///
/// The rows are inserted with Diesel directly rather than through
/// `PgNoteRepository`, because the repository would check out its own
/// connection; they are trusted constants that already satisfy the model's
/// `#[normalize]` and `#[validate]` rules.
pub async fn seed_if_empty(state: &AppState) -> AutumnResult<()> {
    use autumn_web::reexports::diesel::prelude::*;
    use autumn_web::reexports::diesel::sql_types::BigInt;
    use autumn_web::reexports::diesel_async::{AsyncConnection, RunQueryDsl};

    let pool = state
        .pool()
        .ok_or_else(|| AutumnError::service_unavailable_msg("no database pool configured"))?;
    let mut conn = pool.get().await.map_err(AutumnError::from)?;
    let key = autumn_web::lock::distributed_lock_key("react-graphql:seed-notes");

    conn.transaction::<(), AutumnError, _>(async move |conn| {
        diesel::sql_query("SELECT pg_advisory_xact_lock($1)")
            .bind::<BigInt, _>(key)
            .execute(conn)
            .await?;
        let existing: i64 = schema::notes::table.count().get_result(conn).await?;
        if existing > 0 {
            return Ok(());
        }
        diesel::insert_into(schema::notes::table)
            .values(&seed_notes())
            .execute(conn)
            .await?;
        tracing::info!("seeded the notes table");
        Ok(())
    })
    .await
}

/// The seed rows, oldest first (`id` order). The welcome note is pinned.
#[must_use]
pub fn seed_notes() -> Vec<NewNote> {
    vec![
        NewNote {
            title: "Try the GraphQL endpoint".to_owned(),
            body: "curl -X POST 127.0.0.1:3000/graphql -H 'content-type: application/json' \
                   -d '{\"query\":\"{ notes { id title } }\"}' — or read the same rows at /api/notes."
                .to_owned(),
            pinned: false,
        },
        NewNote {
            title: "Welcome to Autumn Notes".to_owned(),
            body: "This list is rendered by React and fetched over GraphQL from an Autumn \
                   backend. Each note is a row in Postgres behind a #[model] and a #[repository]."
                .to_owned(),
            pinned: true,
        },
    ]
}
