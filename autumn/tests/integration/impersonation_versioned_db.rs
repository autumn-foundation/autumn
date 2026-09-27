//! Database-level proof that a `#[repository(versioned)]` write performed
//! **while impersonating** records the real impersonator (issue #1394, AC6 and
//! the issue's success metric).
//!
//! The two halves of that claim are covered separately elsewhere —
//! `integration/impersonation.rs` proves the ambient current actor is the
//! impersonator, and `repository_audit_actor.rs` (#1383) proves a version row
//! takes its actor from that ambient value. This test composes them over a real
//! Postgres: a request authenticated by a session that is impersonating writes
//! through a versioned repository, and the row in `_autumn_version_history`
//! must name the operator, not the customer.
//!
//! **Requires Docker** to be running.

#![cfg(feature = "db")]

use std::sync::Arc;

use autumn_web::audit::{AuditLogger, TracingAuditSink};
use autumn_web::auth::impersonation::ImpersonationGate;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use autumn_web::version_history::{VersionFilter, VersionOp};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

diesel::table! {
    test_impersonation_notes (id) {
        id -> Int8,
        body -> Text,
    }
}

#[autumn_web::model(table = "test_impersonation_notes")]
#[derive(PartialEq, Eq)]
pub struct ImpersonationNote {
    #[id]
    pub id: i64,
    pub body: String,
}

#[autumn_web::repository(
    ImpersonationNote,
    table = "test_impersonation_notes",
    versioned = true
)]
pub trait ImpersonationNoteRepository {}

// ── Handlers ─────────────────────────────────────────────────────────────────

#[autumn_web::post("/login-admin")]
async fn login_admin(session: Session) -> &'static str {
    session.insert("user_id", "admin-1").await;
    session.insert("role", "admin").await;
    "ok"
}

#[derive(serde::Deserialize)]
struct TargetForm {
    user_id: String,
}

#[autumn_web::post("/impersonate")]
async fn begin(
    State(state): State<AppState>,
    session: Session,
    Form(form): Form<TargetForm>,
) -> AutumnResult<String> {
    autumn_web::auth::impersonation::begin_impersonation(&state, &session, form.user_id).await?;
    Ok("impersonating".to_owned())
}

#[autumn_web::post("/stop-impersonating")]
async fn stop(State(state): State<AppState>, session: Session) -> AutumnResult<String> {
    autumn_web::auth::impersonation::end_impersonation(&state, &session).await?;
    Ok("stopped".to_owned())
}

/// A versioned repository write behind `#[secured]` — the ordinary shape of an
/// authenticated mutation. Returns the new record id.
#[autumn_web::post("/notes")]
#[autumn_web::secured]
async fn create_note(State(state): State<AppState>) -> AutumnResult<String> {
    let repo = build_repo(state.pool().cloned().expect("pool configured"));
    let created = repo
        .save(&NewImpersonationNote {
            body: "written during a support session".to_owned(),
        })
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(e.to_string()))?;
    Ok(created.id.to_string())
}

// ── Setup & helpers ──────────────────────────────────────────────────────────

const fn build_repo(pool: Pool<AsyncPgConnection>) -> PgImpersonationNoteRepository {
    PgImpersonationNoteRepository {
        pool,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_impersonation_notes \
         (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_impersonation_notes");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS _autumn_version_history (
            id          BIGSERIAL   PRIMARY KEY,
            table_name  TEXT        NOT NULL,
            tenant_id   TEXT,
            record_id   BIGINT      NOT NULL,
            op          TEXT        NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
            actor       TEXT        NOT NULL DEFAULT 'system',
            request_id  TEXT,
            changes     JSONB       NOT NULL DEFAULT '[]',
            recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(&mut conn)
    .await
    .expect("create _autumn_version_history");

    (pool, container)
}

/// The actor recorded on the insert version row for `id`.
async fn insert_actor(pool: Pool<AsyncPgConnection>, id: i64) -> Option<String> {
    let repo = build_repo(pool);
    repo.version_history(id, VersionFilter::default())
        .await
        .expect("version history")
        .entries
        .into_iter()
        .find(|entry| entry.op == VersionOp::Insert)
        .map(|entry| entry.actor)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_versioned_write_while_impersonating_records_the_impersonator() {
    let (pool, _container) = setup_pool().await;
    let client = TestApp::new()
        .routes(routes![login_admin, begin, stop, create_note])
        .with_db(pool.clone())
        .state_initializer(|state| {
            state.insert_extension(ImpersonationGate::allow_roles(["admin"]));
            state.insert_extension(AuditLogger::new().with_sink(Arc::new(TracingAuditSink)));
        })
        .build();

    client.post("/login-admin").send().await.assert_ok();

    // Baseline: the operator's own write is theirs.
    let own_id: i64 = client
        .post("/notes")
        .send()
        .await
        .text()
        .parse()
        .expect("record id");
    assert_eq!(
        insert_actor(pool.clone(), own_id).await.as_deref(),
        Some("admin-1")
    );

    client
        .post("/impersonate")
        .form("user_id=customer-9")
        .send()
        .await
        .assert_ok();

    // The write is performed by a session that resolves as `customer-9`…
    let impersonated_id: i64 = client
        .post("/notes")
        .send()
        .await
        .text()
        .parse()
        .expect("record id");

    // …and the version row names the operator who is really responsible. This
    // is the guarantee the issue's success metric asks for: 100% of writes made
    // during impersonation carry the real impersonator.
    assert_eq!(
        insert_actor(pool.clone(), impersonated_id).await.as_deref(),
        Some("admin-1"),
        "a versioned write made while impersonating must name the impersonator, \
         not the customer"
    );

    // After reverting, writes are the operator's own again — same id, but now
    // because they really are acting as themselves.
    client.post("/stop-impersonating").send().await.assert_ok();
    let after_id: i64 = client
        .post("/notes")
        .send()
        .await
        .text()
        .parse()
        .expect("record id");
    assert_eq!(
        insert_actor(pool, after_id).await.as_deref(),
        Some("admin-1")
    );
}
