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
CREATE DOMAIN cash AS MONEY;
CREATE TABLE users (
    id BIGSERIAL PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    balance NUMERIC(30, 12) NOT NULL,
    credit amount,
    fee MONEY,
    tip cash,
    fees MONEY[],
    grid MONEY[],
    ranks INT[],
    offs MONEY[],
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
INSERT INTO users (email, balance, credit, fee, tip, fees, grid, ranks, offs, ratio, created_at, born, avatar, prefs, tags, uid, active)
VALUES
  ('Ada@Example.com', 12345678901234567.123456789012, 98765432109876543210.01234567890123456789,
   1234567.89, 12.5, ARRAY[1.5, 2000]::money[], ARRAY[[1.5, 2], [3, NULL]]::money[],
   '[0:2]={1,2,3}', '[0:1]={1.5,2}',
   0.30000000000000004,
   '2026-01-02 03:04:05.123456+00', '1815-12-10', '\x00ff10'::bytea,
   '{"theme": "dark", "n": [1, 2.5, {"deep": null}]}', ARRAY['a', 'b "q"', 'ü'],
   'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', true),
  ('bob@example.com', 1, NULL, NULL, NULL, '{}', NULL, '{4,5}', NULL, NULL, '2026-01-01 00:00:00+00', NULL, NULL, NULL, NULL,
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
#[allow(
    clippy::too_many_lines,
    reason = "one round trip, checked step by step"
)]
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

    // `to_jsonb` does not show array bounds, so compare them on their own.
    let bounds = "SELECT array_dims(ranks) || ' ' || array_dims(offs) AS value \
                  FROM users WHERE id = 1";
    assert_eq!(text(&target_pool, bounds).await, "[0:2] [0:1]");

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

    // `money` travels as a plain number, with no currency symbol or group
    // separator from `lc_monetary`.
    assert_eq!(first.records("users")[0]["fee"], "1234567.89");
    // A domain over `money` keeps its base type in the manifest, so import
    // reads it through `numeric` too.
    assert_eq!(first.records("users")[0]["tip"], "12.50");
    let tip = users.fields.iter().find(|f| f.name == "tip").expect("tip");
    assert_eq!(tip.base_type.as_deref(), Some("money"));
    // A `money[]` travels as plain numbers too.
    assert_eq!(
        first.records("users")[0]["fees"],
        serde_json::json!(["1.50", "2000.00"])
    );
    assert_eq!(
        first.records("users")[0]["grid"],
        serde_json::json!([["1.50", "2.00"], ["3.00", null]])
    );
    // An array with bounds other than 1 travels as its array literal.
    assert_eq!(first.records("users")[0]["ranks"], "[0:2]={1,2,3}");
    assert_eq!(first.records("users")[0]["offs"], "[0:1]={1.50,2.00}");

    // A subject that the column type cannot read is bad input, not a fault.
    let err = export_subject(registry.capsule_models(), &source, "not-a-number")
        .await
        .expect_err("bad subject");
    assert!(matches!(err, DataCapsuleError::InvalidInput(_)), "{err:?}");

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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn postgres_rejects_a_subject_that_the_column_type_changes() {
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    PgConnection::establish(&url)
        .expect("connect")
        .batch_execute(
            "CREATE TABLE accounts (code VARCHAR(5) PRIMARY KEY, note TEXT); \
             CREATE TABLE codes (code CHAR(5) PRIMARY KEY, note TEXT); \
             CREATE DOMAIN inner_code AS VARCHAR(5); \
             CREATE DOMAIN outer_code AS inner_code; \
             CREATE TABLE nested (code outer_code PRIMARY KEY, note TEXT); \
             INSERT INTO nested VALUES ('ab123', 'mine'); \
             CREATE TABLE fees (id INT PRIMARY KEY, amount NUMERIC(6, 2), note TEXT); \
             INSERT INTO accounts VALUES ('ab123', 'mine'); \
             INSERT INTO codes VALUES ('ab123', 'mine'); \
             INSERT INTO fees VALUES (1, 1.23, 'mine');",
        )
        .expect("schema");
    let store = PgCapsuleStore::new(pool(&url));

    // An explicit cast to `varchar(5)` cuts `ab123-extra` to `ab123`, and an
    // explicit cast to `numeric(6, 2)` rounds `1.234` to `1.23`. Neither
    // request may get the records of another subject.
    for (model, subject) in [
        (
            CapsuleModel::new("accounts", "code").primary_key("code"),
            "ab123-extra",
        ),
        (
            CapsuleModel::new("codes", "code").primary_key("code"),
            "ab123-extra",
        ),
        (CapsuleModel::new("fees", "amount"), "1.234"),
        // A domain over a domain over `varchar(5)` cuts the value too.
        (
            CapsuleModel::new("nested", "code").primary_key("code"),
            "ab123-extra",
        ),
    ] {
        let err = export_subject(std::slice::from_ref(&model), &store, subject)
            .await
            .expect_err("lossy subject");
        assert!(
            matches!(err, DataCapsuleError::InvalidInput(_)),
            "{subject}: {err:?}"
        );
    }

    // The exact values still work, also with a different but equal form.
    let accounts = [CapsuleModel::new("accounts", "code").primary_key("code")];
    let capsule = export_subject(&accounts, &store, "ab123")
        .await
        .expect("export");
    assert_eq!(capsule.records("accounts").len(), 1);
    let codes = [CapsuleModel::new("codes", "code").primary_key("code")];
    let capsule = export_subject(&codes, &store, "ab123")
        .await
        .expect("export");
    assert_eq!(capsule.records("codes").len(), 1);
    let fees = [CapsuleModel::new("fees", "amount")];
    let capsule = export_subject(&fees, &store, "1.230")
        .await
        .expect("export");
    assert_eq!(capsule.records("fees").len(), 1);

    drop(container);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(
    clippy::too_many_lines,
    reason = "one schema, five target databases, checked step by step"
)]
async fn postgres_import_moves_sequences_in_their_direction_and_only_on_success() {
    const LEDGER: &str = "
        CREATE TABLE ledger (
            id INT GENERATED BY DEFAULT AS IDENTITY (START WITH -1 INCREMENT BY -1) PRIMARY KEY,
            owner INT NOT NULL
        );
        CREATE TABLE notes (id SERIAL PRIMARY KEY, owner INT NOT NULL, body TEXT);";
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("postgres");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let base = format!("postgres://postgres:postgres@{host}:{port}");
    let mut admin = PgConnection::establish(&format!("{base}/postgres")).expect("connect");
    for db in ["target", "busy", "deferred", "capped", "cached", "limited"] {
        admin
            .batch_execute(&format!("CREATE DATABASE {db}"))
            .expect("create db");
    }
    admin
        .batch_execute(&format!(
            "{LEDGER}
             INSERT INTO ledger (owner) VALUES (1), (1);
             INSERT INTO notes (id, owner, body) VALUES (500, 1, 'mine');"
        ))
        .expect("source");
    for db in ["target", "busy", "deferred", "capped", "cached", "limited"] {
        PgConnection::establish(&format!("{base}/{db}"))
            .expect("connect")
            .batch_execute(LEDGER)
            .expect("schema");
    }
    let models = [
        CapsuleModel::new("ledger", "owner"),
        CapsuleModel::new("notes", "owner"),
    ];
    let capsule = export_subject(
        &models,
        &PgCapsuleStore::new(pool(&format!("{base}/postgres"))),
        "1",
    )
    .await
    .expect("export");

    // A descending sequence moves down past the smallest imported key.
    let target = pool(&format!("{base}/target"));
    import_capsule(&capsule, &models, &PgCapsuleStore::new(target.clone()))
        .await
        .expect("import");
    let next = text(
        &target,
        "INSERT INTO ledger (owner) VALUES (2) RETURNING id::text AS value",
    )
    .await;
    assert_eq!(next, "-3");

    // A failed import leaves every sequence as it was: `setval` is not
    // undone by a rollback.
    let busy = pool(&format!("{base}/busy"));
    PgConnection::establish(&format!("{base}/busy"))
        .expect("connect")
        .batch_execute("INSERT INTO notes (id, owner, body) VALUES (500, 9, 'theirs')")
        .expect("conflict row");
    let err = import_capsule(&capsule, &models, &PgCapsuleStore::new(busy.clone()))
        .await
        .expect_err("conflict on notes");
    assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
    let last = text(
        &busy,
        "SELECT COALESCE(pg_sequence_last_value(pg_get_serial_sequence('ledger', 'id'))::text, \
         'unused') AS value",
    )
    .await;
    assert_eq!(last, "unused");

    // A deferred constraint fails only at commit. It must fail before any
    // sequence moves.
    let deferred = pool(&format!("{base}/deferred"));
    PgConnection::establish(&format!("{base}/deferred"))
        .expect("connect")
        .batch_execute(
            "ALTER TABLE notes ADD CONSTRAINT notes_body_key UNIQUE (body) \
             DEFERRABLE INITIALLY DEFERRED; \
             INSERT INTO notes (id, owner, body) VALUES (900, 9, 'mine')",
        )
        .expect("deferred constraint");
    let err = import_capsule(&capsule, &models, &PgCapsuleStore::new(deferred.clone()))
        .await
        .expect_err("deferred conflict on notes");
    assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
    let last = text(
        &deferred,
        "SELECT COALESCE(pg_sequence_last_value(pg_get_serial_sequence('ledger', 'id'))::text, \
         'unused') AS value",
    )
    .await;
    assert_eq!(last, "unused");

    // A later sequence that cannot take its key must stop the import before
    // the first `setval`: `notes` gets key 500, but its sequence stops at 100.
    let capped = pool(&format!("{base}/capped"));
    PgConnection::establish(&format!("{base}/capped"))
        .expect("connect")
        .batch_execute("ALTER SEQUENCE notes_id_seq MAXVALUE 100")
        .expect("cap");
    let err = import_capsule(&capsule, &models, &PgCapsuleStore::new(capped.clone()))
        .await
        .expect_err("key above MAXVALUE");
    assert!(matches!(err, DataCapsuleError::Conflict(_)), "{err:?}");
    let last = text(
        &capped,
        "SELECT COALESCE(pg_sequence_last_value(pg_get_serial_sequence('ledger', 'id'))::text, \
         'unused') AS value",
    )
    .await;
    assert_eq!(last, "unused");

    // Other sessions can hold cached values of a `CACHE 10` sequence, and
    // `setval` does not take them back. Import refuses such a sequence.
    let cached = pool(&format!("{base}/cached"));
    PgConnection::establish(&format!("{base}/cached"))
        .expect("connect")
        .batch_execute("ALTER TABLE ledger ALTER COLUMN id SET CACHE 10")
        .expect("cache");
    let err = import_capsule(&capsule, &models, &PgCapsuleStore::new(cached.clone()))
        .await
        .expect_err("cached sequence");
    assert!(matches!(err, DataCapsuleError::NotConfigured(_)), "{err:?}");
    assert_eq!(
        count(&cached, "SELECT COUNT(*) AS count FROM ledger").await,
        0,
        "the import rolls back"
    );

    // A role that may move the `ledger` sequence but not the `notes` one:
    // the import must fail before the first `setval`.
    admin
        .batch_execute("CREATE ROLE limited LOGIN PASSWORD 'limited'")
        .expect("role");
    PgConnection::establish(&format!("{base}/limited"))
        .expect("connect")
        .batch_execute(
            "GRANT USAGE ON SCHEMA public TO limited; \
             GRANT SELECT, INSERT ON ledger, notes TO limited; \
             GRANT SELECT, USAGE, UPDATE ON SEQUENCE ledger_id_seq TO limited; \
             GRANT SELECT, USAGE ON SEQUENCE notes_id_seq TO limited;",
        )
        .expect("grants");
    let limited_url = format!(
        "{}/limited",
        base.replace("postgres:postgres@", "limited:limited@")
    );
    let err = import_capsule(&capsule, &models, &PgCapsuleStore::new(pool(&limited_url)))
        .await
        .expect_err("no UPDATE on notes_id_seq");
    assert!(matches!(err, DataCapsuleError::Store(_)), "{err:?}");
    let last = text(
        &pool(&format!("{base}/limited")),
        "SELECT COALESCE(pg_sequence_last_value(pg_get_serial_sequence('ledger', 'id'))::text, \
         'unused') AS value",
    )
    .await;
    assert_eq!(last, "unused");

    drop(container);
}
