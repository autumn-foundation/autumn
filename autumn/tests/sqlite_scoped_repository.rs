//! `SQLite` scope/authorization connection-threading compile proof
//! (issue #1614, Codex P1).
//!
//! Under `--features sqlite` the pooled connection the repository macro checks
//! out for a `#[repository(scope = ...)]` list route is `RuntimeConnection`
//! (the `SyncConnectionWrapper<SqliteConnection>`), and the generated
//! `*_api_list` route calls `__scope.list(&__ctx, &mut __conn)` with that
//! connection. The `Scope::list` / `ScopeQuery::load` connection parameter used
//! to be hard-coded to `&mut AsyncPgConnection`, so those scope APIs could not
//! type-align with a `SQLite` `RuntimeConnection` — a latent gap that
//! `cargo build -p autumn-web --features sqlite` never surfaced because
//! autumn-web's own lib mounts no scoped-repo route under that feature.
//!
//! This test pins **exactly the threaded surface**: it declares a `Scope` impl
//! whose `list` takes `&mut RuntimeConnection` (matching the fixed trait) and
//! two never-called functions that force `Scope::list` and `ScopeQuery::load`
//! to accept a `&mut RuntimeConnection`. Under `--features sqlite`
//! `RuntimeConnection` is the `SQLite` wrapper, so these type-checks compile
//! **only** once the connection parameter was threaded from
//! `&mut AsyncPgConnection` to `&mut RuntimeConnection`; before the fix they
//! fail to compile. The Postgres build is a no-op because `RuntimeConnection`
//! aliases `AsyncPgConnection` under default features.
//!
//! It deliberately does **not** instantiate a full `#[repository]`: the default
//! CRUD the repository macro generates (upsert `ON CONFLICT ... RETURNING`,
//! `FOR UPDATE` locking, batch insert) uses Postgres-only diesel query
//! fragments that are independently invalid for the `SQLite` backend — a separate,
//! much larger "`SQLite` repository lane" concern well beyond this scope-threading
//! P1. Isolating the scope surface keeps the proof honest and focused.
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]` so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features sqlite --test sqlite_scoped_repository`.
#![cfg(feature = "sqlite")]

use autumn_web::authorization::{BoxFuture, PolicyContext, Scope, ScopeQuery, Scoped};
use autumn_web::db::RuntimeConnection;

/// A plain resource type — no `#[model]`/`#[repository]`, so none of the
/// Postgres-specific CRUD codegen is pulled in; only the scope surface is under
/// test.
struct ScopedWidget;

/// A `Scope` whose `list` takes `&mut RuntimeConnection` — the threaded
/// signature. Under `--features sqlite` this is `&mut
/// SyncConnectionWrapper<SqliteConnection>`; the impl compiling at all proves
/// the trait's connection parameter is the runtime pool connection, not the
/// hard-coded `&mut AsyncPgConnection`.
struct WidgetScope;
impl Scope<ScopedWidget> for WidgetScope {
    fn list<'a>(
        &'a self,
        _ctx: &'a PolicyContext,
        _conn: &'a mut RuntimeConnection,
    ) -> BoxFuture<'a, autumn_web::AutumnResult<Vec<ScopedWidget>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// Never called — its body is a type-check that `Scope::list` accepts a
/// `&mut RuntimeConnection` (the `SQLite` wrapper under `--features sqlite`). This
/// mirrors the `__scope.list(&__ctx, &mut __conn)` call the repository macro
/// generates for a scoped list route. Fails to compile before the fix.
#[allow(dead_code)]
async fn _scope_list_accepts_runtime_connection(
    scope: &WidgetScope,
    ctx: &PolicyContext,
    conn: &mut RuntimeConnection,
) -> autumn_web::AutumnResult<Vec<ScopedWidget>> {
    scope.list(ctx, conn).await
}

/// Never called — pins `ScopeQuery::load` (the `Post::scope(&ctx).load(&mut
/// db)` ergonomic path) to accept a `&mut RuntimeConnection`. Fails to compile
/// before the fix.
#[allow(dead_code)]
async fn _scope_query_load_accepts_runtime_connection(
    ctx: &PolicyContext,
    conn: &mut RuntimeConnection,
) -> autumn_web::AutumnResult<Vec<ScopedWidget>> {
    let query: ScopeQuery<'_, ScopedWidget> = <ScopedWidget as Scoped>::scope(ctx);
    query.load(conn).await
}

#[test]
fn scope_apis_thread_runtime_connection_under_sqlite() {
    // Reaching here means the file — including the two connection-threading
    // type-checks above — compiled under `--features sqlite`, i.e. the scope
    // APIs now carry `&mut RuntimeConnection` (the SQLite wrapper) rather than a
    // hard-coded `&mut AsyncPgConnection`.
    assert!(std::mem::size_of::<WidgetScope>() == 0);
}
