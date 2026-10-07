//! #2439 — the generated API `422` for an embedded NUL names the field.
//!
//! Postgres reports a NUL in `TEXT` without a column, so the generated write
//! handlers look for the byte in the payload. Covers the plain and the
//! policy-backed handlers, create and update.
//!
//! ```text
//! cargo test -p autumn-web --test integration_tests repository_nul_field -- --ignored
//! ```

use autumn_web::authorization::{BoxFuture, Policy, PolicyContext};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use diesel::prelude::*;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use http::StatusCode;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

diesel::table! {
    nul_field_notes (id) {
        id -> Int8,
        title -> Text,
        body -> Text,
    }
}

#[autumn_web::model(table = "nul_field_notes")]
pub struct NulFieldNote {
    #[id]
    pub id: i64,
    pub title: String,
    pub body: String,
}

#[autumn_web::repository(NulFieldNote, table = "nul_field_notes", api = "/api/nul-plain")]
pub trait NulFieldNoteRepository {}

diesel::table! {
    nul_field_policy_notes (id) {
        id -> Int8,
        title -> Text,
        body -> Text,
    }
}

#[autumn_web::model(table = "nul_field_policy_notes")]
pub struct NulFieldPolicyNote {
    #[id]
    pub id: i64,
    pub title: String,
    pub body: String,
}

#[autumn_web::repository(
    NulFieldPolicyNote,
    table = "nul_field_policy_notes",
    api = "/api/nul-policy",
    policy = AllowAll
)]
pub trait NulFieldPolicyNoteRepository {}

#[derive(Default, Clone)]
pub struct AllowAll;

impl<T: Send + Sync + 'static> Policy<T> for AllowAll {
    fn can_show<'a>(&'a self, _: &'a PolicyContext, _: &'a T) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn can_create<'a>(&'a self, _: &'a PolicyContext) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn can_update<'a>(&'a self, _: &'a PolicyContext, _: &'a T) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
    fn can_delete<'a>(&'a self, _: &'a PolicyContext, _: &'a T) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
}

async fn setup() -> (
    autumn_web::test::TestClient,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default().start().await.expect("start Postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    for table in ["nul_field_notes", "nul_field_policy_notes"] {
        diesel::sql_query(format!(
            "CREATE TABLE IF NOT EXISTS {table} (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, body TEXT NOT NULL)"
        ))
        .execute(&mut conn)
        .await
        .expect("create table");
    }
    drop(conn);

    let client = TestApp::new()
        .with_db(pool)
        .routes(vec![
            __autumn_route_info_nul_field_note_api_create(),
            __autumn_route_info_nul_field_note_api_update(),
            __autumn_route_info_nul_field_policy_note_api_create(),
            __autumn_route_info_nul_field_policy_note_api_update(),
        ])
        .policy::<NulFieldPolicyNote, _>(AllowAll)
        .build();
    (client, container)
}

/// The field names in a `422` Problem Details body.
fn error_fields(response: &autumn_web::test::TestResponse) -> Vec<String> {
    let body: serde_json::Value = response.json();
    body["errors"]
        .as_array()
        .expect("errors[] present")
        .iter()
        .map(|e| e["field"].as_str().expect("field").to_owned())
        .collect()
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn create_names_the_field_that_carried_the_nul() {
    let (client, _container) = setup().await;
    for path in ["/api/nul-plain", "/api/nul-policy"] {
        let response = client
            .post(path)
            .json(&serde_json::json!({"title": "ok", "body": "a\u{0}b"}))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        assert_eq!(error_fields(&response), ["body"], "{path}");
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn update_names_the_field_that_carried_the_nul() {
    let (client, _container) = setup().await;
    for path in ["/api/nul-plain", "/api/nul-policy"] {
        let created = client
            .post(path)
            .json(&serde_json::json!({"title": "ok", "body": "fine"}))
            .send()
            .await;
        assert_eq!(created.status, StatusCode::CREATED, "{path}");
        let id = created.json::<serde_json::Value>()["id"].as_i64().expect("id");

        let response = client
            .put(&format!("{path}/{id}"))
            .json(&serde_json::json!({"title": "t\u{0}"}))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        assert_eq!(error_fields(&response), ["title"], "{path}");
    }
}
