//! The Postgres backend, against a real Postgres.
//!
//! This is the **same contract** the in-memory suite asserts
//! (`memory_backend.rs`), re-run against SQL: ranking, pagination totals,
//! fail-closed blank queries, idempotent upserts, filter semantics, k-NN
//! ordering. If a behaviour differs between the two, one of them is wrong.
//!
//! The last suite here is the issue's falsifiable success metric end to end:
//! index a record, query it by keyword *and* by vector similarity, mutate the
//! record, and assert the index reflects it — with no hand-written index-sync
//! code anywhere.
//!
//! **Requires Docker.** `#[ignore]`d so a default `cargo test` never needs it;
//! CI's Docker sweep runs it with no workflow edit (see CLAUDE.md).

#![allow(clippy::items_after_statements)]

use std::sync::Arc;

use autumn_search::{
    BackfillOptions, DocumentSource as _, HashingEmbedder, IndexedDocument, KeywordQuery,
    PostgresSearchStore, ReindexArgs, SearchBackend as _, SearchClient, SearchFilter, VectorQuery,
};
use autumn_web::pagination::PageRequest;
use autumn_web::search::{IndexDefinition, SearchIndexed as _};
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

use super::support::{
    Article, Note, TenantArticle, article, note, tenant_article, untenanted_article, with_tenant,
};

const CREATE_TABLES_SQL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS search_articles (
        id    BIGSERIAL PRIMARY KEY,
        title TEXT NOT NULL,
        body  TEXT NOT NULL
    )",
    // The tenant-scoped model's table, plus a `deleted_at` column so the
    // soft-delete exclusion has something to exclude.
    "CREATE TABLE IF NOT EXISTS search_tenant_articles (
        id         BIGSERIAL PRIMARY KEY,
        title      TEXT NOT NULL,
        body       TEXT NOT NULL,
        tenant_id  TEXT,
        deleted_at TIMESTAMPTZ
    )",
    // Keyed on `note_id`, with a leftover `id` column whose values disagree.
    // A source reader that assumes `id` returns rows here without error — it
    // just keys every document off the wrong column.
    "CREATE TABLE IF NOT EXISTS search_notes (
        note_id BIGSERIAL PRIMARY KEY,
        id      BIGINT NOT NULL,
        title   TEXT NOT NULL,
        body    TEXT NOT NULL
    )",
];

/// A running Postgres plus a store wired to it.
struct Fixture {
    store: Arc<PostgresSearchStore>,
    pool: Pool<autumn_web::RuntimeConnection>,
    _container: testcontainers::ContainerAsync<Postgres>,
}

impl Fixture {
    async fn start() -> Self {
        Self::start_with_dimensions(None).await
    }

    async fn start_with_dimensions(dimensions: Option<usize>) -> Self {
        let container = Postgres::default()
            .start()
            .await
            .expect("failed to start postgres container");
        let host = container.get_host().await.expect("host");
        let port = container.get_host_port_ipv4(5432).await.expect("port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

        let manager = AsyncDieselConnectionManager::<autumn_web::RuntimeConnection>::new(&url);
        let pool = Pool::builder(manager).max_size(4).build().expect("pool");

        let mut conn = pool.get().await.expect("conn");
        for statement in CREATE_TABLES_SQL {
            diesel::sql_query(*statement)
                .execute(&mut conn)
                .await
                .expect("create source table");
        }
        drop(conn);

        let store = Arc::new(PostgresSearchStore::new(dimensions));
        store.install_pool(pool.clone());
        store
            .ensure_index(&Article::index_definition())
            .await
            .expect("ensure_index");
        store
            .ensure_index(&TenantArticle::index_definition())
            .await
            .expect("ensure_index (tenant)");
        store
            .ensure_index(&Note::index_definition())
            .await
            .expect("ensure_index (note)");

        Self {
            store,
            pool,
            _container: container,
        }
    }

    fn definition() -> IndexDefinition {
        Article::index_definition()
    }

    /// Insert a row into the *source* table (the system of record).
    async fn insert(&self, record: &Article) {
        let mut conn = self.pool.get().await.expect("conn");
        diesel::sql_query(
            "INSERT INTO search_articles (id, title, body) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET title = EXCLUDED.title, body = EXCLUDED.body",
        )
        .bind::<diesel::sql_types::BigInt, _>(record.id)
        .bind::<diesel::sql_types::Text, _>(record.title.clone())
        .bind::<diesel::sql_types::Text, _>(record.body.clone())
        .execute(&mut conn)
        .await
        .expect("insert source row");
    }

    /// Insert a row into the tenant-scoped source table.
    async fn insert_tenant(&self, record: &TenantArticle) {
        let mut conn = self.pool.get().await.expect("conn");
        diesel::sql_query(
            "INSERT INTO search_tenant_articles (id, title, body, tenant_id) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (id) DO UPDATE SET title = EXCLUDED.title, body = EXCLUDED.body, \
             tenant_id = EXCLUDED.tenant_id",
        )
        .bind::<diesel::sql_types::BigInt, _>(record.id)
        .bind::<diesel::sql_types::Text, _>(record.title.clone())
        .bind::<diesel::sql_types::Text, _>(record.body.clone())
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Text>, _>(record.tenant_id.clone())
        .execute(&mut conn)
        .await
        .expect("insert tenant source row");
    }

    /// Soft-delete a tenant-scoped source row.
    async fn soft_delete_tenant(&self, id: i64) {
        let mut conn = self.pool.get().await.expect("conn");
        diesel::sql_query("UPDATE search_tenant_articles SET deleted_at = NOW() WHERE id = $1")
            .bind::<diesel::sql_types::BigInt, _>(id)
            .execute(&mut conn)
            .await
            .expect("soft delete");
    }

    /// Index tenant-scoped documents directly.
    async fn index_tenant(&self, records: &[TenantArticle]) {
        let documents: Vec<IndexedDocument> = records
            .iter()
            .map(|r| IndexedDocument::new(r.search_document()))
            .collect();
        self.store
            .index(&TenantArticle::index_definition(), &documents)
            .await
            .expect("index tenant");
    }

    /// Keyword search the tenant-scoped index.
    async fn tenant_search(&self, query: &str, filter: SearchFilter) -> (Vec<i64>, u64) {
        let page = self
            .store
            .keyword_search(
                &TenantArticle::index_definition(),
                &KeywordQuery::new(query, PageRequest::default()).filter(filter),
            )
            .await
            .expect("keyword search");
        (
            page.content.iter().map(|hit| hit.id).collect(),
            page.total_elements,
        )
    }

    /// Insert a row into the `note_id`-keyed source table.
    async fn insert_note(&self, record: &Note) {
        let mut conn = self.pool.get().await.expect("conn");
        diesel::sql_query(
            "INSERT INTO search_notes (note_id, id, title, body) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (note_id) DO UPDATE SET title = EXCLUDED.title, body = EXCLUDED.body",
        )
        .bind::<diesel::sql_types::BigInt, _>(record.note_id)
        .bind::<diesel::sql_types::BigInt, _>(record.id)
        .bind::<diesel::sql_types::Text, _>(record.title.clone())
        .bind::<diesel::sql_types::Text, _>(record.body.clone())
        .execute(&mut conn)
        .await
        .expect("insert note row");
    }

    async fn delete_source(&self, id: i64) {
        let mut conn = self.pool.get().await.expect("conn");
        diesel::sql_query("DELETE FROM search_articles WHERE id = $1")
            .bind::<diesel::sql_types::BigInt, _>(id)
            .execute(&mut conn)
            .await
            .expect("delete source row");
    }

    /// Index documents directly, bypassing the source.
    async fn index(&self, documents: &[IndexedDocument]) {
        self.store
            .index(&Self::definition(), documents)
            .await
            .expect("index");
    }

    async fn search(&self, query: &str) -> Vec<i64> {
        self.search_filtered(query, SearchFilter::default()).await.0
    }

    async fn search_filtered(&self, query: &str, filter: SearchFilter) -> (Vec<i64>, u64) {
        let page = self
            .store
            .keyword_search(
                &Self::definition(),
                &KeywordQuery::new(query, PageRequest::default()).filter(filter),
            )
            .await
            .expect("keyword search");
        (
            page.content.iter().map(|hit| hit.id).collect(),
            page.total_elements,
        )
    }

    /// A `SearchClient` over this store (backend **and** document source).
    fn client(&self) -> SearchClient {
        SearchClient::builder()
            .backend(Arc::clone(&self.store) as Arc<dyn autumn_search::SearchBackend>)
            .source(Arc::clone(&self.store) as Arc<dyn autumn_search::DocumentSource>)
            .embedder(Arc::new(HashingEmbedder::new(64)))
            .index::<Article>()
            .index::<TenantArticle>()
            .index::<Note>()
            .build()
    }
}

fn doc(record: &Article) -> IndexedDocument {
    IndexedDocument::new(record.search_document())
}

fn corpus() -> Vec<Article> {
    vec![
        article(1, "Rust web frameworks", "A survey of the ecosystem."),
        article(2, "Getting started", "Autumn is a rust web framework."),
        article(3, "Gardening", "Tomatoes and basil."),
    ]
}

// ── Schema ──────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ensure_index_is_idempotent() {
    let fixture = Fixture::start().await;
    // `start()` already ran it once; running it again on every boot must be a
    // no-op, not an error.
    fixture
        .store
        .ensure_index(&Fixture::definition())
        .await
        .expect("second ensure_index");
    fixture
        .store
        .ensure_index(&Fixture::definition())
        .await
        .expect("third ensure_index");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_stock_postgres_without_pgvector_falls_back_to_the_portable_path() {
    // The plain `postgres` image has no `pgvector`. Boot must succeed and
    // vector search must still work — that is the whole point of the fallback.
    let fixture = Fixture::start_with_dimensions(Some(3)).await;
    assert_eq!(
        fixture.store.vector_mode(),
        Some(autumn_search::VectorMode::Array)
    );
}

// ── Keyword ─────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn keyword_search_ranks_a_title_match_above_a_body_match() {
    let fixture = Fixture::start().await;
    for record in corpus() {
        fixture.index(&[doc(&record)]).await;
    }

    assert_eq!(
        fixture.search("rust").await,
        vec![1, 2],
        "the weight-A title match must rank first"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn keyword_search_paginates_with_a_stable_total() {
    let fixture = Fixture::start().await;
    for record in corpus() {
        fixture.index(&[doc(&record)]).await;
    }

    let first = fixture
        .store
        .keyword_search(
            &Fixture::definition(),
            &KeywordQuery::new("rust", PageRequest::new(1, 1)),
        )
        .await
        .expect("page 1");
    let second = fixture
        .store
        .keyword_search(
            &Fixture::definition(),
            &KeywordQuery::new("rust", PageRequest::new(2, 1)),
        )
        .await
        .expect("page 2");

    assert_eq!(
        first.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(
        second.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(first.total_elements, 2);
    assert_eq!(second.total_elements, 2);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_blank_query_returns_nothing_rather_than_every_row() {
    let fixture = Fixture::start().await;
    for record in corpus() {
        fixture.index(&[doc(&record)]).await;
    }

    for blank in ["", "   ", "\t\n"] {
        let (ids, total) = fixture
            .search_filtered(blank, SearchFilter::default())
            .await;
        assert!(ids.is_empty(), "blank query {blank:?} matched {ids:?}");
        assert_eq!(total, 0);
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_hostile_query_can_neither_error_nor_widen_the_result_set() {
    let fixture = Fixture::start().await;
    for record in corpus() {
        fixture.index(&[doc(&record)]).await;
    }

    for hostile in [
        "rust OR gardening",
        "title:rust",
        "rust*",
        "') OR 1=1 --",
        "\"; DROP TABLE search_articles; --",
    ] {
        let ids = fixture.search(hostile).await;
        assert!(
            !ids.contains(&3),
            "query {hostile:?} must not match the unrelated record (got {ids:?})"
        );
    }

    // The source table must still be there.
    assert_eq!(fixture.search("rust").await, vec![1, 2]);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn indexing_the_same_record_twice_stores_one_document() {
    let fixture = Fixture::start().await;
    let record = article(1, "Rust web frameworks", "A survey.");
    fixture.index(&[doc(&record)]).await;
    fixture.index(&[doc(&record)]).await;

    let (ids, total) = fixture
        .search_filtered("rust", SearchFilter::default())
        .await;
    assert_eq!(ids, vec![1]);
    assert_eq!(total, 1);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn deleting_removes_the_document_and_replays_cleanly() {
    let fixture = Fixture::start().await;
    for record in corpus() {
        fixture.index(&[doc(&record)]).await;
    }

    fixture
        .store
        .delete(&Fixture::definition(), &[1])
        .await
        .expect("delete");
    assert_eq!(fixture.search("rust").await, vec![2]);

    // At-least-once delivery replays deletes: a repeat, and a delete of an id
    // that was never indexed, must both be no-ops.
    fixture
        .store
        .delete(&Fixture::definition(), &[1, 999])
        .await
        .expect("replayed delete");
    assert_eq!(fixture.search("rust").await, vec![2]);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn clearing_empties_the_index() {
    let fixture = Fixture::start().await;
    for record in corpus() {
        fixture.index(&[doc(&record)]).await;
    }

    fixture
        .store
        .clear(&Fixture::definition())
        .await
        .expect("clear");
    assert!(fixture.search("rust").await.is_empty());
}

// ── Filtering / tenancy ─────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_tenant_filter_never_crosses_tenants() {
    let fixture = Fixture::start().await;
    fixture
        .index_tenant(&[
            tenant_article(1, "Rust at acme", "internal", "acme"),
            tenant_article(2, "Rust at globex", "internal", "globex"),
            untenanted_article(3, "Rust in public", "no tenant"),
        ])
        .await;

    let (ids, total) = fixture
        .tenant_search("rust", SearchFilter::default().tenant("acme"))
        .await;
    assert_eq!(ids, vec![1]);
    assert_eq!(total, 1, "the total must reflect the filtered set");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_tenant_value_containing_sql_is_bound_not_interpolated() {
    let fixture = Fixture::start().await;
    fixture
        .index_tenant(&[tenant_article(1, "Rust", "x", "acme")])
        .await;

    let (ids, _) = fixture
        .tenant_search(
            "rust",
            SearchFilter::default().tenant("acme'; DROP TABLE autumn_search_documents; --"),
        )
        .await;
    assert!(ids.is_empty());

    // The index table survived, so the value was bound rather than executed.
    let (ids, _) = fixture
        .tenant_search("rust", SearchFilter::default().tenant("acme"))
        .await;
    assert_eq!(ids, vec![1]);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn id_allowlists_and_denylists_are_honoured() {
    let fixture = Fixture::start().await;
    for record in corpus() {
        fixture.index(&[doc(&record)]).await;
    }

    let (ids, _) = fixture
        .search_filtered("rust", SearchFilter::default().allow_ids([2_i64]))
        .await;
    assert_eq!(ids, vec![2]);

    let (ids, _) = fixture
        .search_filtered("rust", SearchFilter::default().exclude_ids([1_i64]))
        .await;
    assert_eq!(ids, vec![2]);

    let (ids, total) = fixture
        .search_filtered("rust", SearchFilter::default().allow_ids(Vec::<i64>::new()))
        .await;
    assert!(ids.is_empty(), "an empty allowlist must match nothing");
    assert_eq!(total, 0);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_equals_filter_matches_an_indexed_field() {
    let fixture = Fixture::start().await;
    for record in corpus() {
        fixture.index(&[doc(&record)]).await;
    }

    let (ids, _) = fixture
        .search_filtered(
            "rust",
            SearchFilter::default().equals("title", "Getting started"),
        )
        .await;
    assert_eq!(ids, vec![2]);
}

// ── Vector ──────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn vector_search_orders_by_cosine_similarity() {
    let fixture = Fixture::start().await;
    fixture
        .index(&[
            doc(&article(1, "a", "b")).with_embedding(vec![1.0, 0.0, 0.0]),
            doc(&article(2, "a", "b")).with_embedding(vec![0.9, 0.1, 0.0]),
            doc(&article(3, "a", "b")).with_embedding(vec![0.0, 1.0, 0.0]),
        ])
        .await;

    let hits = fixture
        .store
        .vector_search(
            &Fixture::definition(),
            &VectorQuery::new(vec![1.0, 0.0, 0.0], 2),
        )
        .await
        .expect("vector search");

    assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 2]);
    assert!(hits[0].score > hits[1].score);
    assert!(
        (hits[0].score - 1.0).abs() < 1e-4,
        "score {}",
        hits[0].score
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn vector_search_honours_the_filter_and_the_threshold() {
    let fixture = Fixture::start().await;
    fixture
        .index(&[
            doc(&article(1, "a", "b")).with_embedding(vec![1.0, 0.0, 0.0]),
            doc(&article(2, "a", "b")).with_embedding(vec![0.0, 1.0, 0.0]),
        ])
        .await;

    let filtered = fixture
        .store
        .vector_search(
            &Fixture::definition(),
            &VectorQuery::new(vec![1.0, 0.0, 0.0], 10)
                .filter(SearchFilter::default().allow_ids([2_i64])),
        )
        .await
        .expect("filtered");
    assert_eq!(filtered.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2]);

    let thresholded = fixture
        .store
        .vector_search(
            &Fixture::definition(),
            &VectorQuery::new(vec![1.0, 0.0, 0.0], 10).min_score(0.5),
        )
        .await
        .expect("thresholded");
    assert_eq!(
        thresholded.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1]
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn documents_of_a_different_embedding_width_are_skipped_not_mis_scored() {
    let fixture = Fixture::start().await;
    fixture
        .index(&[
            doc(&article(1, "a", "b")).with_embedding(vec![1.0, 0.0, 0.0]),
            doc(&article(2, "a", "b")).with_embedding(vec![1.0, 0.0]),
        ])
        .await;

    let hits = fixture
        .store
        .vector_search(
            &Fixture::definition(),
            &VectorQuery::new(vec![1.0, 0.0, 0.0], 10),
        )
        .await
        .expect("vector search");
    assert_eq!(
        hits.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1],
        "a mismatched-width document must be skipped, not scored against padding"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_embedding_round_trips_through_the_index() {
    let fixture = Fixture::start().await;
    fixture
        .index(&[doc(&article(1, "a", "b")).with_embedding(vec![0.5, -0.25, 0.75])])
        .await;

    let stored = fixture
        .store
        .embedding(&Fixture::definition(), 1, &SearchFilter::default())
        .await
        .expect("read back");
    assert_eq!(stored, Some(vec![0.5, -0.25, 0.75]));
    assert_eq!(
        fixture
            .store
            .embedding(&Fixture::definition(), 99, &SearchFilter::default())
            .await
            .expect("read back"),
        None
    );
}

// ── Document source ─────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_document_source_projects_a_model_table_generically() {
    let fixture = Fixture::start().await;
    fixture
        .insert_tenant(&tenant_article(1, "Rust", "a rust web framework", "acme"))
        .await;

    let documents = fixture
        .store
        .fetch(&TenantArticle::index_definition(), &[1])
        .await
        .expect("fetch");

    assert_eq!(documents.len(), 1);
    let document = &documents[0];
    assert_eq!(document.id, 1);
    assert_eq!(document.field("title"), Some("Rust"));
    assert_eq!(document.field("body"), Some("a rust web framework"));
    assert_eq!(document.tenant_id.as_deref(), Some("acme"));
    // `body` is the `#[searchable(embed)]` field.
    assert_eq!(document.embed_text.as_deref(), Some("a rust web framework"));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn fetching_a_missing_row_is_an_absence_not_an_error() {
    let fixture = Fixture::start().await;
    fixture.insert(&article(1, "Rust", "body")).await;

    let documents = fixture
        .store
        .fetch(&Fixture::definition(), &[1, 2, 3])
        .await
        .expect("fetch must tolerate missing rows");
    assert_eq!(documents.iter().map(|d| d.id).collect::<Vec<_>>(), vec![1]);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn scanning_walks_the_table_by_keyset() {
    let fixture = Fixture::start().await;
    for id in 1..=5 {
        fixture
            .insert(&article(id, &format!("Rust {id}"), "body"))
            .await;
    }

    let first = fixture
        .store
        .scan(&Fixture::definition(), None, 2)
        .await
        .expect("scan");
    assert_eq!(first.iter().map(|d| d.id).collect::<Vec<_>>(), vec![1, 2]);

    let second = fixture
        .store
        .scan(&Fixture::definition(), Some(2), 2)
        .await
        .expect("scan");
    assert_eq!(second.iter().map(|d| d.id).collect::<Vec<_>>(), vec![3, 4]);

    assert!(
        fixture
            .store
            .scan(&Fixture::definition(), Some(5), 2)
            .await
            .expect("scan")
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_model_keyed_on_something_other_than_id_backfills_and_searches() {
    // `Note` is keyed on `note_id`, over a table that also has a leftover `id`
    // whose values are offset by 100. Every source query — the keyset scan,
    // the id-list fetch, the ordering, the projected key — has to come from
    // the index definition. A hardcoded `id` does not error here; it returns
    // documents keyed 101/102/103, which no longer match the ids the sync
    // hooks write, so the index silently ends up holding two of everything.
    let fixture = Fixture::start().await;
    let client = fixture.client();
    let definition = Note::index_definition();

    for id in 1..=3 {
        fixture
            .insert_note(&note(id, &format!("Rust {id}"), "notes about rust"))
            .await;
    }

    // Keyset scan, in key order, paginated on the real key column.
    let first = fixture
        .store
        .scan(&definition, None, 2)
        .await
        .expect("scan");
    assert_eq!(first.iter().map(|d| d.id).collect::<Vec<_>>(), vec![1, 2]);
    let second = fixture
        .store
        .scan(&definition, Some(2), 2)
        .await
        .expect("scan");
    assert_eq!(second.iter().map(|d| d.id).collect::<Vec<_>>(), vec![3]);

    // Id-list fetch, the path a per-record reindex job takes.
    let fetched = fixture.store.fetch(&definition, &[2]).await.expect("fetch");
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0].id, 2);
    assert_eq!(
        fetched[0]
            .fields
            .iter()
            .find(|f| f.name == "title")
            .map(|f| f.value.as_str()),
        Some("Rust 2")
    );

    // And the whole thing end to end: backfill, then query.
    let report = client
        .backfill("search_notes", &BackfillOptions::default().batch_size(2))
        .await
        .expect("backfill");
    assert_eq!(report.indexed, 3);

    let page = client
        .search::<Note>("rust", &PageRequest::default())
        .await
        .expect("search");
    assert_eq!(page.total_elements, 3);
}

// ── End to end: the issue's success metric ──────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn backfill_then_keyword_and_vector_queries_both_return_ranked_results() {
    let fixture = Fixture::start().await;
    let client = fixture.client();

    fixture
        .insert(&article(
            1,
            "Rust web frameworks",
            "building apis with rust",
        ))
        .await;
    fixture
        .insert(&article(2, "Getting started", "a rust web framework"))
        .await;
    fixture
        .insert(&article(3, "Gardening", "tomato basil in summer"))
        .await;

    let report = client
        .backfill("search_articles", &BackfillOptions::default().batch_size(2))
        .await
        .expect("backfill");
    assert_eq!(report.indexed, 3);
    assert_eq!(report.batches, 2, "3 rows at 2/batch is 2 batches");

    let keyword = client
        .search::<Article>("rust", &PageRequest::default())
        .await
        .expect("keyword search");
    assert_eq!(
        keyword.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1, 2],
        "the weight-A title match ranks first"
    );

    let semantic = client
        .similar::<Article>("rust web framework", 2)
        .await
        .expect("semantic search");
    assert_eq!(semantic.len(), 2);
    assert!(
        !semantic.iter().any(|hit| hit.id == 3),
        "the unrelated record must not be a nearest neighbour: {semantic:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_record_update_changes_recall_with_no_manual_index_code() {
    // The falsifiable claim from the issue, driven exactly as the lifecycle
    // drives it: the reindex job's payload is `(index, id)` and nothing else.
    let fixture = Fixture::start().await;
    let client = fixture.client();

    fixture
        .insert(&article(1, "Rust web frameworks", "a survey"))
        .await;
    client
        .reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("initial reindex");
    assert_eq!(fixture.search("rust").await, vec![1]);

    // Mutate the record and let the same instruction converge the index.
    fixture
        .insert(&article(1, "Gardening weekly", "tomatoes only"))
        .await;
    client
        .reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("lifecycle reindex");

    assert!(
        fixture.search("rust").await.is_empty(),
        "the old text must no longer match"
    );
    assert_eq!(
        fixture.search("tomatoes").await,
        vec![1],
        "the new text must match"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_deleted_row_is_removed_from_the_index_by_the_same_instruction() {
    // Drift repair: a lost delete event, or a row removed by direct SQL, is
    // healed by an ordinary upsert reindex — the source has no row, so the
    // document goes away.
    let fixture = Fixture::start().await;
    let client = fixture.client();

    fixture
        .insert(&article(1, "Rust", "a rust framework"))
        .await;
    client
        .reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("index");
    assert_eq!(fixture.search("rust").await, vec![1]);

    fixture.delete_source(1).await;
    client
        .reindex(&ReindexArgs::upsert("search_articles", 1))
        .await
        .expect("converge");
    assert!(fixture.search("rust").await.is_empty());
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn backfill_with_purge_drops_documents_the_source_no_longer_produces() {
    let fixture = Fixture::start().await;
    let client = fixture.client();

    for id in 1..=3 {
        fixture
            .insert(&article(id, "Rust", "a rust framework"))
            .await;
    }
    client
        .backfill("search_articles", &BackfillOptions::default())
        .await
        .expect("initial");
    assert_eq!(fixture.search("rust").await.len(), 3);

    fixture.delete_source(3).await;

    let without_purge = client
        .backfill("search_articles", &BackfillOptions::default())
        .await
        .expect("no purge");
    assert_eq!(without_purge.indexed, 2);
    assert_eq!(
        fixture.search("rust").await.len(),
        3,
        "without purge the orphan survives"
    );

    let purged = client
        .backfill("search_articles", &BackfillOptions::default().purge(true))
        .await
        .expect("purge");
    assert!(purged.purged);
    assert_eq!(fixture.search("rust").await, vec![1, 2]);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_search_never_returns_a_record_the_visibility_filter_excluded() {
    // The end-to-end authorization property, on real SQL.
    let fixture = Fixture::start().await;
    let client = fixture.client();

    for record in [
        tenant_article(1, "Rust at acme", "a rust framework", "acme"),
        tenant_article(2, "Rust at globex", "a rust framework", "globex"),
    ] {
        fixture.insert_tenant(&record).await;
    }
    client
        .backfill("search_tenant_articles", &BackfillOptions::default())
        .await
        .expect("backfill");

    // No filter argument: the ambient tenant alone confines the query, and the
    // total is computed after the restriction.
    let page = with_tenant("acme", async {
        client
            .search::<TenantArticle>("rust", &PageRequest::default())
            .await
    })
    .await
    .expect("search");
    assert_eq!(
        page.content.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(page.total_elements, 1);

    // And an unscoped query is refused outright rather than run across both.
    let error = client
        .search::<TenantArticle>("rust", &PageRequest::default())
        .await
        .expect_err("an unscoped query on a tenant-scoped index must be refused");
    assert!(
        matches!(
            error,
            autumn_search::SearchError::TenantContextMissing { .. }
        ),
        "{error:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_soft_deleted_row_is_not_put_back_by_a_backfill() {
    // `after_delete_commit` removes the document; without a `deleted_at`
    // exclusion in the source projection, the next backfill would put it
    // straight back — searchable while `find` hides it.
    let fixture = Fixture::start().await;
    let client = fixture.client();

    fixture
        .insert_tenant(&tenant_article(1, "Rust one", "a rust framework", "acme"))
        .await;
    fixture
        .insert_tenant(&tenant_article(2, "Rust two", "a rust framework", "acme"))
        .await;
    client
        .backfill("search_tenant_articles", &BackfillOptions::default())
        .await
        .expect("backfill");
    assert_eq!(
        fixture
            .tenant_search("rust", SearchFilter::default())
            .await
            .0,
        vec![1, 2]
    );

    fixture.soft_delete_tenant(2).await;
    client
        .backfill(
            "search_tenant_articles",
            &BackfillOptions::default().purge(true),
        )
        .await
        .expect("backfill after soft delete");
    assert_eq!(
        fixture
            .tenant_search("rust", SearchFilter::default())
            .await
            .0,
        vec![1],
        "a soft-deleted row must not be re-indexed"
    );

    // A targeted reindex of the soft-deleted row removes it rather than
    // restoring it: the source no longer produces the row, so the reindex
    // converges to "absent".
    fixture
        .index_tenant(&[tenant_article(2, "Rust two", "a rust framework", "acme")])
        .await;
    client
        .reindex(&ReindexArgs::upsert("search_tenant_articles", 2))
        .await
        .expect("reindex");
    assert_eq!(
        fixture
            .tenant_search("rust", SearchFilter::default())
            .await
            .0,
        vec![1]
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn an_undeclared_filter_key_cannot_widen_the_query() {
    // A request-supplied facet key sits in an IDENTIFIER position. Unchecked,
    // `title' = 'x' OR '1'='1` would close the quote and OR away the tenant
    // restriction that precedes it.
    let fixture = Fixture::start().await;
    fixture
        .index_tenant(&[
            tenant_article(1, "Rust at acme", "internal", "acme"),
            tenant_article(2, "Rust at globex", "internal", "globex"),
        ])
        .await;

    let (ids, total) = fixture
        .tenant_search(
            "rust",
            SearchFilter::default()
                .tenant("acme")
                .equals("title' = 'x' OR '1'='1", "boom"),
        )
        .await;
    assert!(
        ids.is_empty(),
        "an undeclared key must match nothing, not widen: {ids:?}"
    );
    assert_eq!(total, 0);

    // The index is intact and the tenant restriction still holds.
    let (ids, _) = fixture
        .tenant_search("rust", SearchFilter::default().tenant("acme"))
        .await;
    assert_eq!(ids, vec![1]);
}
