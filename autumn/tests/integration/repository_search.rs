//! Database-level integration tests for Postgres full-text search (issue #842).
//!
//! **Requires Docker** to be running.

#![cfg(feature = "db")]
#![allow(
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    unused_imports
)]

use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

use testcontainers::ImageExt;

diesel::table! {
    test_search_records (id) {
        id -> Int8,
        title -> Text,
        body -> Text,
    }
}

#[autumn_web::model(table = "test_search_records")]
#[searchable(language = "english")]
#[derive(PartialEq, Eq)]
pub struct SearchRecord {
    #[id]
    pub id: i64,
    #[searchable(weight = "A")]
    pub title: String,
    #[searchable(weight = "B")]
    pub body: String,
}

#[autumn_web::repository(SearchRecord, table = "test_search_records", searchable)]
pub trait SearchRecordRepository {}

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .with_tag("16-alpine")
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
        "CREATE TABLE IF NOT EXISTS test_search_records (\
            id BIGSERIAL PRIMARY KEY, \
            title TEXT NOT NULL, \
            body TEXT NOT NULL, \
            search_vector tsvector GENERATED ALWAYS AS (\
                setweight(to_tsvector('english', coalesce(title, '')), 'A') || \
                setweight(to_tsvector('english', coalesce(body, '')), 'B') \
            ) STORED\
         )",
    )
    .execute(&mut conn)
    .await
    .expect("create test_search_records");

    diesel::sql_query(
        "CREATE INDEX IF NOT EXISTS idx_test_search_records_search_vector \
         ON test_search_records USING gin(search_vector)",
    )
    .execute(&mut conn)
    .await
    .expect("create idx_test_search_records_search_vector");

    (pool, container)
}

const fn build_repo(pool: Pool<AsyncPgConnection>) -> PgSearchRecordRepository {
    PgSearchRecordRepository {
        pool,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

// ── Tests (RED - expects compile errors until macro/codegen is implemented) ──

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_search_basic_and_ranking() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    // Save test documents
    let doc1 = repo
        .save(&NewSearchRecord {
            title: "Rust programming language".to_string(),
            body: "A systems programming language focused on safety, speed, and concurrency."
                .to_string(),
        })
        .await
        .unwrap();

    let doc2 = repo
        .save(&NewSearchRecord {
            title: "Web development in Go".to_string(),
            body: "Go is a great language for fast web servers and microservices.".to_string(),
        })
        .await
        .unwrap();

    let doc3 = repo
        .save(&NewSearchRecord {
            title: "Postgres database optimization".to_string(),
            body: "How to use indexes and analyze queries in Postgres databases.".to_string(),
        })
        .await
        .unwrap();

    // 1. Basic search
    let results = repo.search("programming").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, doc1.id);

    // 2. Weight precedence (title match with weight 'A' should rank higher than body match with weight 'B')
    // We add another doc where "language" is in the body, whereas doc1 has it in the title.
    let doc4 = repo
        .save(&NewSearchRecord {
            title: "Some Python info".to_string(),
            body: "Python is a popular programming language.".to_string(),
        })
        .await
        .unwrap();

    let results_weight = repo.search("programming").await.unwrap();
    assert_eq!(results_weight.len(), 2);
    // doc1 has "programming" in the title (weight A); doc4 has it in the body (weight B).
    // doc1 must come first due to weight precedence.
    assert_eq!(results_weight[0].id, doc1.id);
    assert_eq!(results_weight[1].id, doc4.id);

    // 3. Websearch operators (e.g. quotes, OR, exclusion)
    let results_or = repo.search("Go OR Postgres").await.unwrap();
    assert_eq!(results_or.len(), 2);
    assert!(results_or.iter().any(|r| r.id == doc2.id));
    assert!(results_or.iter().any(|r| r.id == doc3.id));

    let results_exclude = repo.search("language -Go").await.unwrap();
    assert_eq!(results_exclude.len(), 2);
    assert!(results_exclude.iter().any(|r| r.id == doc1.id));
    assert!(results_exclude.iter().any(|r| r.id == doc4.id));
    assert!(!results_exclude.iter().any(|r| r.id == doc2.id)); // Go excluded

    // 4. Unicode matching
    let doc_unicode = repo
        .save(&NewSearchRecord {
            title: "Café und Tee".to_string(),
            body: "Guten Morgen Österreich und Zürich.".to_string(),
        })
        .await
        .unwrap();

    let results_unicode = repo.search("Zürich").await.unwrap();
    assert_eq!(results_unicode.len(), 1);
    assert_eq!(results_unicode[0].id, doc_unicode.id);

    let results_cafe = repo.search("Café").await.unwrap();
    assert_eq!(results_cafe.len(), 1);
    assert_eq!(results_cafe[0].id, doc_unicode.id);

    // 5. Empty query short-circuits with no SQL/database execution and returns empty list
    let results_empty = repo.search("   ").await.unwrap();
    assert!(results_empty.is_empty());

    // 6. Pagination stability
    let page_req = autumn_web::pagination::PageRequest::new(1, 1);
    let page = repo.search_page("programming", &page_req).await.unwrap();
    assert_eq!(page.content.len(), 1);
    assert_eq!(page.total_elements, 2);
    assert_eq!(page.content[0].id, doc1.id);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_explain_scan_verification() {
    #[derive(diesel::QueryableByName)]
    struct ExplainRow {
        #[diesel(column_name = "QUERY PLAN", sql_type = diesel::sql_types::Text)]
        query_plan: String,
    }

    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool.clone());

    // Generate 10k mock rows to make the planner favor index scan over sequential scan
    let mut batch = Vec::with_capacity(10000);
    for i in 0..10000 {
        batch.push(NewSearchRecord {
            title: format!("Record title number {i}"),
            body: if i == 5000 {
                "This is the rare golden record containing rustacean".to_string()
            } else {
                "This is a common record with standard text".to_string()
            },
        });
    }

    // Insert in chunks of 2,000 to be fast and safe from parameter limit
    for chunk in batch.chunks(2000) {
        repo.save_many(chunk).await.unwrap();
    }

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("ANALYZE test_search_records")
        .execute(&mut conn)
        .await
        .unwrap();

    diesel::sql_query("SET enable_seqscan = off")
        .execute(&mut conn)
        .await
        .unwrap();

    let explain_rows: Vec<ExplainRow> = diesel::sql_query(
        "EXPLAIN SELECT id FROM test_search_records \
         WHERE search_vector @@ websearch_to_tsquery('english'::regconfig, 'rustacean')",
    )
    .load::<ExplainRow>(&mut conn)
    .await
    .unwrap();

    let mut plan = String::new();
    for row in explain_rows {
        plan.push_str(&row.query_plan);
        plan.push('\n');
    }

    println!("Query Plan:\n{plan}");
    // Verify GIN index scan is utilized and Sequential Scan is avoided
    assert!(
        !plan.contains("Seq Scan"),
        "Sequential Scan was used instead of GIN Index Scan!"
    );
    assert!(
        plan.contains("Bitmap Index Scan") || plan.contains("Index Scan") || plan.contains("GIN"),
        "FTS index was not utilized!"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_search_special_characters() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    // Save record with special characters in body
    let doc = repo
        .save(&NewSearchRecord {
            title: "Special characters test".to_string(),
            body: "Rust's syntax uses & references, | pipes, and : colons.".to_string(),
        })
        .await
        .unwrap();

    // Verify FTS search handles special characters gracefully
    // These queries shouldn't crash Postgres or throw syntax errors.
    let results = repo.search("Rust's").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, doc.id);

    // Assert searching other special characters returns safely (even if ignored or cleaned)
    let _ = repo.search("&").await.unwrap();
    let _ = repo.search("|").await.unwrap();
    let _ = repo.search(":").await.unwrap();
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_search_migration_backfill() {
    #[derive(diesel::QueryableByName, PartialEq, Debug)]
    struct BackfilledRow {
        #[diesel(column_name = "id", sql_type = diesel::sql_types::BigInt)]
        id: i64,
        #[diesel(column_name = "title", sql_type = diesel::sql_types::Text)]
        title: String,
    }

    let (pool, _container) = setup_pool().await;
    let mut conn = pool.get().await.expect("conn");

    // 1. Create table without FTS column
    diesel::sql_query(
        "CREATE TABLE test_backfill_records (\
            id BIGSERIAL PRIMARY KEY, \
            title TEXT NOT NULL, \
            body TEXT NOT NULL \
         )",
    )
    .execute(&mut conn)
    .await
    .expect("create test_backfill_records");

    // 2. Pre-populate with existing rows
    diesel::sql_query(
        "INSERT INTO test_backfill_records (title, body) VALUES \
         ('Legacy document', 'This is pre-existing data that needs to be backfilled.')",
    )
    .execute(&mut conn)
    .await
    .expect("insert pre-existing rows");

    // 3. Run the FTS migration (adds generated column and GIN index)
    diesel::sql_query(
        "ALTER TABLE test_backfill_records ADD COLUMN search_vector tsvector GENERATED ALWAYS AS (\
            setweight(to_tsvector('english', coalesce(title, '')), 'A') || \
            setweight(to_tsvector('english', coalesce(body, '')), 'B') \
         ) STORED",
    )
    .execute(&mut conn)
    .await
    .expect("alter table to add search_vector");

    diesel::sql_query(
        "CREATE INDEX idx_test_backfill_records_search_vector \
         ON test_backfill_records USING gin(search_vector)",
    )
    .execute(&mut conn)
    .await
    .expect("create GIN index on search_vector");

    // 4. Query the backfilled records and assert we can search them successfully

    let results: Vec<BackfilledRow> = diesel::sql_query(
        "SELECT id, title FROM test_backfill_records \
         WHERE search_vector @@ websearch_to_tsquery('english'::regconfig, 'backfilled')",
    )
    .load::<BackfilledRow>(&mut conn)
    .await
    .expect("query backfilled search");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "Legacy document");

    // Cleanup
    diesel::sql_query("DROP TABLE test_backfill_records")
        .execute(&mut conn)
        .await
        .unwrap();
}
