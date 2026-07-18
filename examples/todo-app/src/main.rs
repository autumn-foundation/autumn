mod jobs;
mod models;
mod routes;
mod schema;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use autumn_web::auth::{API_TOKEN_MIGRATIONS, ApiTokenStore, DbApiTokenStore, RequireApiToken};
use autumn_web::migrate::{EmbeddedMigrations, embed_migrations};
use autumn_web::{AutumnResult, jobs, routes};

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

// ── DeferredStore ─────────────────────────────────────────────────────────────────
//
// `RequireApiToken` needs a store at construction time, but the DB pool only
// becomes available inside `on_startup`. `DeferredStore` holds a `OnceLock`
// that is filled during startup and then delegates all calls to the real store.

#[derive(Clone)]
struct DeferredStore(Arc<OnceLock<Arc<dyn ApiTokenStore>>>);

impl DeferredStore {
    fn new() -> Self {
        Self(Arc::new(OnceLock::new()))
    }

    fn init(&self, store: Arc<dyn ApiTokenStore>) {
        let _ = self.0.set(store);
    }

    fn inner(&self) -> Arc<dyn ApiTokenStore> {
        Arc::clone(
            self.0
                .get()
                .expect("DeferredStore used before on_startup ran"),
        )
    }
}

impl ApiTokenStore for DeferredStore {
    fn issue<'a>(
        &'a self,
        principal_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<String>> + Send + 'a>> {
        let store = self.inner();
        Box::pin(async move { store.issue(principal_id).await })
    }

    fn verify<'a>(
        &'a self,
        raw_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<Option<String>>> + Send + 'a>> {
        let store = self.inner();
        Box::pin(async move { store.verify(raw_token).await })
    }

    fn revoke<'a>(
        &'a self,
        raw_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send + 'a>> {
        let store = self.inner();
        Box::pin(async move { store.revoke(raw_token).await })
    }

    fn verify_scoped<'a>(
        &'a self,
        raw_token: &'a str,
    ) -> Pin<
        Box<dyn Future<Output = AutumnResult<Option<autumn_web::auth::VerifiedToken>>> + Send + 'a>,
    > {
        let store = self.inner();
        Box::pin(async move { store.verify_scoped(raw_token).await })
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────────

#[autumn_web::main]
async fn main() {
    let deferred = DeferredStore::new();
    let deferred_for_startup = deferred.clone();

    autumn_web::app()
        // App migrations (todos table) + framework migrations (api_tokens table).
        .migrations(MIGRATIONS)
        .migrations(API_TOKEN_MIGRATIONS)
        // HTML routes (session-auth) + open token-issuance endpoint.
        .routes(routes![
            routes::todos::index,
            routes::todos::list,
            routes::todos::detail,
            routes::todos::create,
            routes::todos::validate_title,
            routes::todos::toggle,
            routes::todos::delete_todo,
            routes::todos::start_export,
            routes::api::issue_token,
        ])
        // Tracked background job powering the async CSV export (#1373):
        // enqueue_tracked -> ctx.set_progress -> a polled download link.
        .jobs(jobs![jobs::export_todos_csv])
        // Bearer-token-protected JSON API under `/api`. Using `.scoped(...)`
        // (rather than a merged raw router) keeps these routes in the route
        // registry, so their `#[api_doc(mcp)]` tags project them as MCP tools
        // while `RequireApiToken` still guards every call — agent or HTTP.
        .scoped(
            "/api",
            RequireApiToken::new(Arc::new(deferred.clone())),
            routes![
                routes::api::list_json,
                routes::api::create_json,
                // A streaming MCP tool (#1118): its `Sse` stream is projected
                // onto the MCP SSE channel as progressive `notifications/progress`.
                routes::api::scan_json,
            ],
        )
        // Expose the tagged endpoints as agent-callable MCP tools. `tools/call`
        // dispatches through the real pipeline above, so the bearer token an
        // agent presents to `/mcp` is enforced by `RequireApiToken`.
        .mount_mcp("/mcp")
        // Wire the real DbApiTokenStore once the pool is ready.
        .on_startup(move |state| {
            let deferred = deferred_for_startup.clone();
            async move {
                let pool = state
                    .pool()
                    .expect("database required for API token auth")
                    .clone();
                deferred.init(Arc::new(DbApiTokenStore::new(pool)));
                Ok(())
            }
        })
        .run()
        .await;
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    const MIGRATION_SQL: &str = include_str!("../migrations/00000000000000_create_todos/up.sql");

    #[test]
    fn migration_uses_bigserial_ids() {
        assert!(
            MIGRATION_SQL.contains("id BIGSERIAL PRIMARY KEY"),
            "todo IDs must be 64-bit to match the Int8/i64 application schema",
        );
    }

    #[test]
    fn upgrade_migration_widens_existing_ids() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("migrations/00000000000001_widen_todo_ids_to_bigint/up.sql");
        let sql = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("missing upgrade migration at {}: {err}", path.display()));

        assert!(
            sql.contains("ALTER TABLE todos ALTER COLUMN id TYPE BIGINT"),
            "todo upgrade migration must widen existing IDs to BIGINT",
        );
        assert!(
            sql.contains("ALTER SEQUENCE todos_id_seq AS BIGINT"),
            "todo upgrade migration must widen the backing sequence to BIGINT",
        );
    }

    // ── DeferredStore tests ───────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn deferred_store_delegates_to_inner_store() {
        use autumn_web::auth::{ApiTokenStore, InMemoryApiTokenStore};
        use std::sync::Arc;

        let deferred = super::DeferredStore::new();
        deferred.init(Arc::new(InMemoryApiTokenStore::default()) as Arc<dyn ApiTokenStore>);

        let token = deferred.issue("user:1").await.unwrap();
        assert!(!token.is_empty());
        assert_eq!(
            deferred.verify(&token).await.unwrap(),
            Some("user:1".to_owned())
        );
        deferred.revoke(&token).await.unwrap();
        assert_eq!(deferred.verify(&token).await.unwrap(), None);
    }
}
