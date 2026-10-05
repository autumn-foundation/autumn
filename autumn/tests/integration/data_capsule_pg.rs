//! Postgres round trip of a portable data capsule (issue #1811).
//!
//! **Requires Docker.** Export one user from a source database, write and
//! verify the capsule, import it into a fresh database, and export again. Each
//! field must be equal, also for `numeric`, `double precision`, `bytea`,
//! `jsonb`, arrays, and time stamps.

#![cfg(all(feature = "db", not(feature = "sqlite")))]

use autumn_web::gdpr::GdprRegistry;
use autumn_web::gdpr::portability::{
    CapsuleModel, CapsuleSigner, DataCapsule, DataCapsuleError, PgCapsuleStore, export_subject,
    import_capsule,
};
use diesel::Connection as _;
use diesel::PgConnection;
use diesel::connection::SimpleConnection as _;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl as _};
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const SCHEMA: &str = r"
CREATE DOMAIN amount AS NUMERIC(40, 20);
CREATE TABLE users (
    id BIGSERIAL PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    balance NUMERIC(30, 12) NOT NULL,
    credit amount,
    ratio DOUBLE PRECISION,
    created_at TIMESTAMPTZ NOT NULL,
    born DATE,
    avatar BYTEA,
    prefs JSONB,
    tags TEXT[],
    uid UUID NOT NULL,
    active BOOLEAN NOT NULL,
    email_lower TEXT GENERATED ALWAYS AS (lower(email)) STORED
);
CREATE TABLE posts (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    author_id BIGINT NOT NULL REFERENCES users (id),
    parent_id BIGINT REFERENCES posts (id),
    title TEXT NOT NULL,
    score REAL
);
CREATE TABLE comments (
    id SERIAL PRIMARY KEY,
    author_id BIGINT NOT NULL REFERENCES users (id),
    post_id BIGINT NOT NULL REFERENCES posts (id),
    body TEXT NOT NULL
);
";

const SEED: &str = r#"
INSERT INTO users (email, balance, credit, ratio, created_at, born, avatar, prefs, tags, uid, active)
VALUES
  ('Ada@Example.com', 12345678901234567.123456789012, 98765432109876543210.01234567890123456789,
   0.30000000000000004,
   '2026-01-02 03:04:05.123456+00', '1815-12-10', '\x00ff10'::bytea,
   '{"theme": "dark", "n": [1, 2.5, {"deep": null}]}', ARRAY['a', 'b "q"', 'ü'],
   'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', true),
  ('bob@example.com', 1, NULL, NULL, '2026-01-01 00:00:00+00', NULL, NULL, NULL, NULL,
   'b0eebc99-9c0b-4ef8-bb6d-6bb9bd380a12', false);
INSERT INTO posts (author_id, parent_id, title, score) VALUES
  (1, NULL, 'First <post>', 3.14159),
  (2, NULL, 'Bob post', NULL);
INSERT INTO posts (author_id, parent_id, title, score) VALUES (1, 1, 'Reply to self', 'NaN');
INSERT INTO comments (author_id, post_id, body) VALUES
  (1, 1, 'Ada on Ada'),
  (1, 3, 'Ada again'),
  (2, 2, 'Bob on Bob');
"#;

fn registry() -> GdprRegistry {
    GdprRegistry::new()
        .capsule(CapsuleModel::new("users", "id"))
        .capsule(
            CapsuleModel::new("posts", "author_id")
                .belongs_to("author_id", "users")
                .belongs_to("parent_id", "posts"),
        )
        .capsule(
            CapsuleModel::new("comments", "author_id")
                .belongs_to("author_id", "users")
                .belongs_to("post_id", "posts"),
        )
}

#[derive(diesel::QueryableByName)]
struct TextRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    value: String,
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

fn pool(url: &str) -> Pool<AsyncPgConnection> {
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    Pool::builder(manager).max_size(2).build().expect("pool")
}

async fn text(pool: &Pool<AsyncPgConnection>, sql: &str) -> String {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(sql)
        .get_result::<TextRow>(&mut conn)
        .await
        .expect("query")
        .value
}

async fn count(pool: &Pool<AsyncPgConnection>, sql: &str) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(sql)
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("query")
        .count
}

/// All columns of the subject rows, as Postgres prints them.
async fn subject_rows(pool: &Pool<AsyncPgConnection>, table: &str, column: &str) -> String {
    text(
        pool,
        &format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(t) ORDER BY t.id), '[]')::text AS value \
             FROM {table} t WHERE {column} = 1"
        ),
    )
    .await
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postgres_round_trip_is_lossless_at_field_level() {
    // Generated columns need Postgres 12 or later.
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let base = format!("postgres://postgres:postgres@{host}:{port}");

    let mut admin = PgConnection::establish(&format!("{base}/postgres")).expect("connect");
    admin
        .batch_execute("CREATE DATABASE fresh")
        .expect("create db");
    admin.batch_execute(SCHEMA).expect("schema");
    admin.batch_execute(SEED).expect("seed");
    let mut fresh = PgConnection::establish(&format!("{base}/fresh")).expect("connect");
    fresh.batch_execute(SCHEMA).expect("schema");

    let source_pool = pool(&format!("{base}/postgres"));
    let target_pool = pool(&format!("{base}/fresh"));
    let source = PgCapsuleStore::new(source_pool.clone());
    let target = PgCapsuleStore::new(target_pool.clone());
    let registry = registry();
    let signer = CapsuleSigner::new(b"pg-capsule-secret-0123456789abcdef");

    let first = export_subject(registry.capsule_models(), &source, "1")
        .await
        .expect("export");
    assert_eq!(first.records("users").len(), 1);
    assert_eq!(first.records("posts").len(), 2);
    assert_eq!(first.records("comments").len(), 2);
    let users = first.manifest.model("users").expect("users");
    let generated: Vec<&str> = users
        .fields
        .iter()
        .filter(|f| f.generated)
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(generated, ["email_lower"]);
    // `numeric` travels as text: no digit is lost in JSON.
    assert_eq!(
        first.records("users")[0]["balance"],
        "12345678901234567.123456789012"
    );

    let dir = tempfile::tempdir().expect("tmp");
    let root = dir.path().join("capsule");
    first.write_dir(&root, &signer).expect("write");
    let loaded = DataCapsule::read_dir(&root, &signer).expect("read");
    let summary = import_capsule(&loaded, registry.capsule_models(), &target)
        .await
        .expect("import");
    assert_eq!(summary.records, 5);

    // Field-level equality in the database, generated columns included.
    for (table, column) in [
        ("users", "id"),
        ("posts", "author_id"),
        ("comments", "author_id"),
    ] {
        assert_eq!(
            subject_rows(&source_pool, table, column).await,
            subject_rows(&target_pool, table, column).await,
            "{table}"
        );
    }

    // Export again: the records are the same.
    let second = export_subject(registry.capsule_models(), &target, "1")
        .await
        .expect("second export");
    for table in ["users", "posts", "comments"] {
        assert_eq!(first.records(table), second.records(table), "{table}");
    }

    // Sequences moved past the imported keys, so a new row does not collide.
    let next_post = text(
        &target_pool,
        "INSERT INTO posts (author_id, title) VALUES (1, 'new') RETURNING id::text AS value",
    )
    .await;
    assert_eq!(next_post, "4");
    let next_comment = text(
        &target_pool,
        "INSERT INTO comments (author_id, post_id, body) VALUES (1, 1, 'new') \
         RETURNING id::text AS value",
    )
    .await;
    assert_eq!(next_comment, "3");

    // A second import conflicts and writes nothing.
    let before = count(&target_pool, "SELECT COUNT(*) AS count FROM users").await;
    let err = import_capsule(&loaded, registry.capsule_models(), &target)
        .await
        .expect_err("conflict");
    assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
    assert_eq!(
        count(&target_pool, "SELECT COUNT(*) AS count FROM users").await,
        before
    );

    // The subject id is compared as the column type, not as text.
    let typed = export_subject(registry.capsule_models(), &source, "01")
        .await
        .expect("export with 01");
    assert_eq!(typed.records("users").len(), 1);

    // A domain over `numeric` also travels as text.
    assert_eq!(
        first.records("users")[0]["credit"],
        "98765432109876543210.01234567890123456789"
    );

    drop(container);
}

/// A conflict in a later table rolls back the earlier tables, and a foreign
/// key that points outside the capsule is a conflict, not a server error.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postgres_import_is_atomic_and_reports_a_missing_parent() {
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let base = format!("postgres://postgres:postgres@{host}:{port}");
    let mut admin = PgConnection::establish(&format!("{base}/postgres")).expect("connect");
    for db in ["fresh", "other"] {
        admin
            .batch_execute(&format!("CREATE DATABASE {db}"))
            .expect("create db");
    }
    admin.batch_execute(SCHEMA).expect("schema");
    admin.batch_execute(SEED).expect("seed");
    for db in ["fresh", "other"] {
        let mut conn = PgConnection::establish(&format!("{base}/{db}")).expect("connect");
        conn.batch_execute(SCHEMA).expect("schema");
    }
    let source = PgCapsuleStore::new(pool(&format!("{base}/postgres")));
    let registry = registry();
    let capsule = export_subject(registry.capsule_models(), &source, "1")
        .await
        .expect("export");

    // `fresh` has a comment with id 2 already (on its own user and post).
    let fresh_pool = pool(&format!("{base}/fresh"));
    let mut fresh = PgConnection::establish(&format!("{base}/fresh")).expect("connect");
    fresh
        .batch_execute(
            "INSERT INTO users (id, email, balance, created_at, uid, active) \
             VALUES (99, 'x@example.com', 0, now(), gen_random_uuid(), true); \
             INSERT INTO posts (id, author_id, title) OVERRIDING SYSTEM VALUE \
             VALUES (99, 99, 'x'); \
             INSERT INTO comments (id, author_id, post_id, body) VALUES (2, 99, 99, 'x');",
        )
        .expect("seed conflict");
    let err = import_capsule(
        &capsule,
        registry.capsule_models(),
        &PgCapsuleStore::new(fresh_pool.clone()),
    )
    .await
    .expect_err("conflict on comments");
    assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
    assert_eq!(
        count(&fresh_pool, "SELECT COUNT(*) AS count FROM users").await,
        1,
        "users must roll back"
    );
    assert_eq!(
        count(&fresh_pool, "SELECT COUNT(*) AS count FROM posts").await,
        1,
        "posts must roll back"
    );

    // Without the `users` model the posts point at a user that is not there.
    let partial = GdprRegistry::new()
        .capsule(CapsuleModel::new("posts", "author_id").belongs_to("author_id", "users"));
    let posts_only = export_subject(partial.capsule_models(), &source, "1")
        .await
        .expect("export posts");
    let err = import_capsule(
        &posts_only,
        partial.capsule_models(),
        &PgCapsuleStore::new(pool(&format!("{base}/other"))),
    )
    .await
    .expect_err("missing parent");
    assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");

    drop(container);
}
