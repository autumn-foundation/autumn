//! #2423 — an embedded NUL byte in a form field is client input, not a 500.
//!
//! A Postgres `TEXT`/`VARCHAR` column cannot hold `0x00`. Before this suite,
//! a `%00` in a free-text field decoded cleanly, satisfied every
//! `#[validate(...)]` rule, and only failed at the Diesel→Postgres boundary —
//! surfacing as an uncaught `AutumnError` and a `500`, which by the
//! framework's own error-class convention means "server bug" rather than
//! "malformed client input".
//!
//! Two layers are covered here, from the outside in:
//!
//! 1. **The form boundary** (no Docker) — `ChangesetForm` records the NUL as
//!    an ordinary per-field error, so the handler re-renders the form inline
//!    with a 4xx exactly as it does for a failed `#[validate(...)]` rule.
//!    This mirrors the issue's `/submit` shape from `examples/reddit-clone`.
//! 2. **The database boundary** (Docker) — for the paths no form extractor
//!    sees (JSON APIs, hand-written queries), a real Postgres rejection is
//!    classified as `422 Unprocessable Entity` instead of `500`.
//!
//! ```text
//! cargo test -p autumn-web --test integration_tests nul_byte_input
//! cargo test -p autumn-web --test integration_tests nul_byte_input -- --ignored
//! ```

use autumn_web::form::{ChangesetForm, NUL_CHARACTER_FIELD_ERROR};
use autumn_web::reexports::axum::body::Body;
use autumn_web::reexports::axum::http::{Request, StatusCode};
use autumn_web::reexports::axum::response::IntoResponse;
use autumn_web::reexports::axum::routing::post;
use autumn_web::reexports::axum::{Router, response::Response};
use tower::ServiceExt as _;

/// The issue's form, reduced to the fields that matter: a validated `title`
/// and a free-text Markdown `body` with no rules of its own — the exact shape
/// that let a NUL through untouched.
#[derive(serde::Deserialize, serde::Serialize, validator::Validate)]
struct SubmitPostForm {
    #[validate(length(min = 1, max = 300, message = "Title must be 1-300 characters"))]
    title: String,
    #[serde(default)]
    body: String,
}

/// Stands in for `examples/reddit-clone`'s `submit`: valid input is written,
/// a rejected changeset is re-rendered with a 422 and the author's text.
async fn submit(form: ChangesetForm<SubmitPostForm>) -> Response {
    match form.into_valid() {
        Ok(valid) => (StatusCode::OK, format!("stored:{}", valid.body)).into_response(),
        Err(rejected) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "rejected:{}:{}",
                rejected.errors_for("body").join("|"),
                rejected.field_value("body").unwrap_or_default()
            ),
        )
            .into_response(),
    }
}

fn urlencoded(body: &'static str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/submit")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .expect("build request")
}

async fn body_text(resp: Response) -> String {
    let bytes = autumn_web::reexports::axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    String::from_utf8(bytes.to_vec()).expect("utf-8 body")
}

/// The reported repro, end to end at the HTTP boundary: `body=before%00after`
/// used to be a 500; it is now the documented `ChangesetForm` round-trip —
/// a 422 carrying one inline message and the author's surviving text.
#[tokio::test]
async fn nul_byte_in_post_body_is_a_422_round_trip_not_a_500() {
    let resp = Router::new()
        .route("/submit", post(submit))
        .oneshot(urlencoded("title=nul-test&body=before%00after"))
        .await
        .expect("router response");

    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body_text(resp).await,
        format!("rejected:{NUL_CHARACTER_FIELD_ERROR}:beforeafter")
    );
}

/// The same request without the NUL still succeeds — the guard adds no
/// false positives to an ordinary submission.
#[tokio::test]
async fn clean_post_body_is_still_accepted() {
    let resp = Router::new()
        .route("/submit", post(submit))
        .oneshot(urlencoded("title=nul-test&body=beforeafter"))
        .await
        .expect("router response");

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_text(resp).await, "stored:beforeafter");
}

// ── The database backstop (requires Docker) ─────────────────────────────────

/// Everything the form extractor never sees — a JSON API body, a hand-written
/// query, a background job — still reaches Postgres raw. There the rejection
/// is classified rather than blanket-500'd.
#[cfg(all(feature = "db", any(feature = "test-support", test)))]
mod docker {
    use autumn_web::error::{AutumnError, is_nul_byte_violation};
    use autumn_web::reexports::axum::http::StatusCode;
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use diesel_async::{AsyncPgConnection, RunQueryDsl};
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    async fn start_postgres() -> (
        testcontainers::ContainerAsync<Postgres>,
        Pool<AsyncPgConnection>,
    ) {
        let container = Postgres::default()
            .start()
            .await
            .expect("start Postgres container");
        let host = container.get_host().await.expect("container host");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("container port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
        let pool = Pool::builder(manager)
            .max_size(2)
            .build()
            .expect("build pool");
        (container, pool)
    }

    /// A real `INSERT` of a NUL-bearing `String` into a `TEXT` column: the
    /// server raises SQLSTATE `22021`, and the `?`-converted `AutumnError`
    /// carries `422`, not the `500` reported in #2423.
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn real_pg_nul_byte_insert_is_422_not_500() {
        let (_container, pool) = start_postgres().await;
        let mut conn = pool.get().await.expect("get connection");

        diesel::sql_query(
            "CREATE TABLE IF NOT EXISTS nul_test (id BIGSERIAL PRIMARY KEY, body TEXT NOT NULL)",
        )
        .execute(&mut *conn)
        .await
        .expect("create table");

        async fn insert(
            conn: &mut AsyncPgConnection,
            body: &str,
        ) -> Result<(), autumn_web::error::AutumnError> {
            diesel::sql_query("INSERT INTO nul_test (body) VALUES ($1)")
                .bind::<diesel::sql_types::Text, _>(body)
                .execute(conn)
                .await?;
            Ok(())
        }

        // Sanity: the same statement with clean text succeeds, so the failure
        // below is the byte and not the fixture.
        insert(&mut conn, "beforeafter")
            .await
            .expect("clean insert should succeed");

        let err: AutumnError = insert(&mut conn, "before\u{0}after")
            .await
            .expect_err("Postgres must reject an embedded NUL in a TEXT column");

        assert!(
            is_nul_byte_violation(&err),
            "expected the NUL-byte classification, got: {err}"
        );
        assert_eq!(
            err.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "an embedded NUL is malformed client input, not a server bug"
        );
    }
}
