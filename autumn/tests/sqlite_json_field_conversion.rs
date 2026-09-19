//! `serde_json::Value` model-field round-trip on the `SQLite` runtime backend
//! (issue #1341).
//!
//! Unlike the `DateTime<Utc>`/`Attachment` conversions #1924 wired
//! (`sqlite_field_conversions.rs`), a `json`/`jsonb` scaffold field needs **no**
//! `autumn-web` `FromSql`/`ToSql` code at all: diesel itself already implements
//! `FromSql`/`ToSql<Json, Sqlite> for serde_json::Value` (diesel 2.3+, behind the
//! `serde_json` + `sqlite` cargo features, both already unconditionally enabled
//! by this crate's `db` feature). The schema uses diesel's `Json` sql-type
//! specifically — not `Text` (no built-in `serde_json::Value` conversion for it)
//! and not `Jsonb` (`SQLite`'s proprietary binary encoding, not plain-text JSON) —
//! see `autumn-cli/src/generate/dsl.rs`'s `FieldKind::sqlite_schema_type`.
//!
//! This test declares a model carrying a required `serde_json::Value` and an
//! `Option<serde_json::Value>`, boots an in-memory `SQLite` pool through the
//! real [`autumn_web::db::create_pool`] path, and round-trips both (insert →
//! read back → assert equality) for an object, an array, and — the one
//! genuinely fiddly case — a bare top-level JSON string/number/bool/null value,
//! plus a `NULL` optional column.
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]` so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_json_field_conversion`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{diesel, diesel_async};

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use serde_json::json;

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        json_rows (id) {
            id -> Int8,
            label -> Text,
            // Diesel's own `Json` sql-type (issue #1341) — plain-text JSON
            // stored as `TEXT`, distinct from both `Text` and `Jsonb`.
            payload -> Json,
            metadata -> Nullable<Json>,
        }
    }
}

use schema::json_rows;

#[autumn_web::model]
pub struct JsonRow {
    #[id]
    pub id: i64,
    pub label: String,
    pub payload: serde_json::Value,
    pub metadata: Option<serde_json::Value>,
}

#[autumn_web::repository(JsonRow)]
pub trait JsonRowRepository {}

async fn boot_pool(db_name: &str) -> SqlitePool {
    // A shared-cache in-memory database so every pooled checkout observes the
    // same schema (a bare `:memory:` target is private per connection). Each
    // test passes a DISTINCT name so parallel `#[tokio::test]`s never share
    // rows through the process-wide shared cache.
    let config = DatabaseConfig {
        url: Some(format!("sqlite://file:{db_name}?mode=memory&cache=shared")),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        diesel::sql_query(
            "CREATE TABLE json_rows (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 label TEXT NOT NULL, \
                 payload TEXT NOT NULL, \
                 metadata TEXT\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create json_rows table");
    }

    pool
}

#[tokio::test]
async fn object_and_array_payloads_round_trip_on_sqlite() {
    let pool = boot_pool("json_field_object_array").await;
    let repo = PgJsonRowRepository::with_pool_untracked(pool);

    let payload = json!({"theme": "dark", "retries": 3, "tags": ["a", "b"]});
    let metadata = json!([1, 2, 3]);

    let created = repo
        .save(&NewJsonRow {
            label: "object-and-array".to_string(),
            payload: payload.clone(),
            metadata: Some(metadata.clone()),
        })
        .await
        .expect("save inserts a row and returns it");
    assert!(created.id > 0, "autoincrement id assigned");

    let found = repo
        .find_by_id(created.id)
        .await
        .expect("find_by_id query")
        .expect("row exists");
    assert_eq!(found.label, "object-and-array");
    assert_eq!(
        found.payload, payload,
        "a JSON object round-trips exactly via diesel's Json/Sqlite conversion"
    );
    assert_eq!(
        found.metadata,
        Some(metadata),
        "a JSON array round-trips exactly through the nullable Json column"
    );

    // AC4: the model's derived `Serialize` emits `payload`/`metadata` as
    // native JSON (object/array), not a stringified/double-encoded blob —
    // exactly what a `Json<Model>` JSON API response would send.
    let wire = serde_json::to_value(&found).expect("model serializes");
    assert!(
        wire["payload"].is_object(),
        "payload must serialize as a native JSON object, not a string: {wire}"
    );
    assert!(
        wire["metadata"].is_array(),
        "metadata must serialize as a native JSON array, not a string: {wire}"
    );
}

#[tokio::test]
async fn null_metadata_round_trips_as_none() {
    let pool = boot_pool("json_field_null_metadata").await;
    let repo = PgJsonRowRepository::with_pool_untracked(pool);

    let created = repo
        .save(&NewJsonRow {
            label: "no-metadata".to_string(),
            payload: json!({}),
            metadata: None,
        })
        .await
        .expect("save row with NULL metadata");
    let found = repo
        .find_by_id(created.id)
        .await
        .expect("find_by_id query")
        .expect("row exists");
    assert_eq!(found.metadata, None, "NULL metadata round-trips as None");
}

#[tokio::test]
async fn bare_scalar_json_values_round_trip_on_sqlite() {
    // The fiddliest case: a top-level JSON string/number/bool/null (not an
    // object/array) still round-trips correctly through diesel's `Json`
    // conversion — proving the column genuinely stores/parses JSON syntax
    // rather than treating the value as an opaque pre-serialized string.
    let pool = boot_pool("json_field_bare_scalars").await;
    let repo = PgJsonRowRepository::with_pool_untracked(pool);

    for (label, payload) in [
        ("string", json!("hello \"world\"")),
        ("number", json!(-5)),
        ("bool", json!(true)),
        ("null", serde_json::Value::Null),
    ] {
        let created = repo
            .save(&NewJsonRow {
                label: label.to_string(),
                payload: payload.clone(),
                metadata: None,
            })
            .await
            .unwrap_or_else(|err| panic!("save row for {label}: {err}"));
        let found = repo
            .find_by_id(created.id)
            .await
            .expect("find_by_id query")
            .expect("row exists");
        assert_eq!(found.payload, payload, "{label} round-trips exactly");
    }
}
