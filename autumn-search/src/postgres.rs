//! The Postgres [`SearchBackend`] and [`DocumentSource`].
//!
//! Reuses the in-core FTS primitives from #842 — `to_tsvector` / `setweight` /
//! `plainto_tsquery` / `ts_rank_cd` — and adds vectors, with `pgvector` when
//! the extension is available.
//!
//! # Why a framework-owned index table
//!
//! Documents live in **one** table, `autumn_search_documents`, keyed
//! `(index_name, record_id)`, rather than in each model's own `search_vector`
//! column. That buys three things the per-table column cannot:
//!
//! - the index is **engine-agnostic** — swapping to Meilisearch changes the
//!   backend, not every model's migration;
//! - it is **observable and repairable** — you can count, diff, and purge the
//!   index without touching the system of record;
//! - a **backfill** and an incremental reindex write through the exact same
//!   path, so bootstrapping and steady state cannot disagree.
//!
//! The model's own `#[searchable]` column (and `#[repository(searchable)]`'s
//! `search()`) is untouched: this subsumes #842 as one backend, it does not
//! replace it.
//!
//! # Vectors: `pgvector` when present, portable when not
//!
//! `ensure_index` probes for the `vector` extension. When it is installed
//! **and** `search.embedding_dimensions` is configured, embeddings are written
//! to a `vector(N)` column with an ivfflat index and k-NN uses the `<=>`
//! cosine-distance operator. Otherwise they are written to a portable
//! `double precision[]` column and ranked with an `autumn_search_cosine()` SQL
//! function created by the same migration. Same API, same ordering, different
//! speed — so the plugin is deployable on a managed Postgres without
//! `pgvector`, and gets the fast path for free where it exists.

use std::sync::{OnceLock, RwLock};

use autumn_web::pagination::Page;
use autumn_web::search::{IndexDefinition, SearchDocument};
use diesel::sql_types::{BigInt, Double, Nullable, Text};
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::deadpool::Pool;

use crate::backend::{
    BackendCapabilities, BoxFuture, IndexedDocument, KeywordQuery, SearchBackend, SearchFilter,
    SearchHit, VectorQuery, empty_page,
};
use crate::error::{SearchError, SearchResult};
use crate::source::DocumentSource;
use crate::text::query_tokens;

type RuntimePool = Pool<autumn_web::RuntimeConnection>;

/// Physical table every index's documents live in.
pub const DOCUMENTS_TABLE: &str = "autumn_search_documents";

/// How embeddings are physically stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorMode {
    /// `pgvector` is installed and a dimension is configured: a `vector(N)`
    /// column plus an ivfflat index, queried with `<=>`.
    PgVector {
        /// Declared embedding width.
        dimensions: usize,
    },
    /// Portable fallback: `double precision[]` ranked by an
    /// `autumn_search_cosine()` SQL function.
    Array,
}

impl VectorMode {
    /// Whether this mode uses the `pgvector` extension.
    #[must_use]
    pub const fn is_pgvector(self) -> bool {
        matches!(self, Self::PgVector { .. })
    }
}

// ── Rows ────────────────────────────────────────────────────────────────────

#[derive(diesel::QueryableByName)]
struct HitRow {
    #[diesel(sql_type = BigInt)]
    record_id: i64,
    #[diesel(sql_type = Double)]
    score: f64,
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    total: i64,
}

#[derive(diesel::QueryableByName)]
struct EmbeddingRow {
    #[diesel(sql_type = Nullable<Text>)]
    embedding: Option<String>,
}

#[derive(diesel::QueryableByName)]
struct SourceRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Text)]
    fields: String,
    #[diesel(sql_type = Nullable<Text>)]
    tenant_id: Option<String>,
}

#[derive(diesel::QueryableByName)]
struct ExistsRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    present: bool,
}

// ── The store ───────────────────────────────────────────────────────────────

/// Postgres-backed search storage.
///
/// Implements **both** [`SearchBackend`] (the index) and [`DocumentSource`]
/// (reading records back out of their own tables), because both need the same
/// connection pool and the same identifier-safety rules. `SearchPlugin::postgres()`
/// installs one instance as both.
///
/// The pool is installed at startup ([`PostgresSearchStore::install_pool`]),
/// so the store can be constructed by the plugin builder before an `AppState`
/// exists.
pub struct PostgresSearchStore {
    pool: OnceLock<RuntimePool>,
    /// Configured embedding width; `None` disables the `pgvector` fast path.
    dimensions: Option<usize>,
    /// Resolved at `ensure_index` time, once per process.
    vector_mode: RwLock<Option<VectorMode>>,
}

impl std::fmt::Debug for PostgresSearchStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresSearchStore")
            .field("pool_installed", &self.pool.get().is_some())
            .field("dimensions", &self.dimensions)
            .field("vector_mode", &self.vector_mode())
            .finish()
    }
}

impl PostgresSearchStore {
    /// Create a store whose pool is installed later.
    #[must_use]
    pub const fn new(dimensions: Option<usize>) -> Self {
        Self {
            pool: OnceLock::new(),
            dimensions,
            vector_mode: RwLock::new(None),
        }
    }

    /// Install the application's connection pool. Idempotent; the first call
    /// wins.
    pub fn install_pool(&self, pool: RuntimePool) {
        let _ = self.pool.set(pool);
    }

    /// The resolved vector storage mode, once `ensure_index` has run.
    #[must_use]
    pub fn vector_mode(&self) -> Option<VectorMode> {
        self.vector_mode.read().ok().and_then(|guard| *guard)
    }

    fn pool(&self) -> SearchResult<&RuntimePool> {
        self.pool.get().ok_or_else(|| {
            SearchError::Backend(
                "the search store has no database pool; is `database.primary_url` configured?"
                    .to_owned(),
            )
        })
    }

    async fn conn(
        &self,
    ) -> SearchResult<
        diesel_async::pooled_connection::deadpool::Object<autumn_web::RuntimeConnection>,
    > {
        self.pool()?.get().await.map_err(SearchError::backend)
    }
}

/// Narrow a SQL `double precision` score to the `f32` a [`SearchHit`] carries.
///
/// Relevance scores are ranking-only and comparable within a single result
/// set, so `f32` precision is ample; the saturating cast keeps an out-of-range
/// value finite rather than turning it into an infinity that would poison a
/// sort.
#[allow(
    clippy::cast_possible_truncation,
    reason = "scores are ranking-only; f32 precision is deliberate"
)]
const fn narrow_score(score: f64) -> f32 {
    if score.is_finite() { score as f32 } else { 0.0 }
}

/// Validate a definition and return it, so no caller can interpolate an
/// unvalidated identifier into SQL.
fn checked(definition: &IndexDefinition) -> SearchResult<()> {
    definition
        .validate()
        .map_err(|e| SearchError::InvalidIndex(e.to_string()))
}

/// Render an embedding as a Postgres `double precision[]` literal.
///
/// Values are `f32`, so every element is a finite decimal produced by Rust's
/// own float formatting — there is no path for caller text to reach this
/// string. Non-finite values are clamped to `0`, which keeps a malformed
/// embedding from producing invalid SQL.
fn array_literal(vector: &[f32]) -> String {
    let mut out = String::with_capacity(vector.len() * 8 + 2);
    out.push('{');
    for (index, value) in vector.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let value = if value.is_finite() { *value } else { 0.0 };
        out.push_str(&value.to_string());
    }
    out.push('}');
    out
}

/// Render an embedding as a `pgvector` literal (`[1,2,3]`).
fn vector_literal(vector: &[f32]) -> String {
    let mut out = String::with_capacity(vector.len() * 8 + 2);
    out.push('[');
    for (index, value) in vector.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let value = if value.is_finite() { *value } else { 0.0 };
        out.push_str(&value.to_string());
    }
    out.push(']');
    out
}

/// Parse a `double precision[]` / `vector` text representation back to `f32`s.
fn parse_vector(raw: &str) -> Vec<f32> {
    raw.trim_matches(['{', '}', '[', ']'])
        .split(',')
        .filter(|part| !part.trim().is_empty())
        .filter_map(|part| part.trim().parse::<f32>().ok())
        .collect()
}

/// SQL fragment restricting a query to `filter`, plus the parameters it needs.
///
/// Only **literal, developer-controlled** values are interpolated: record ids
/// (`i64`, formatted by Rust) and field names already validated as bare
/// identifiers. Every caller-supplied *value* is bound, never interpolated.
fn filter_sql(filter: &SearchFilter, next_param: &mut usize, binds: &mut Vec<String>) -> String {
    use std::fmt::Write as _;

    let mut sql = String::new();

    if let Some(tenant) = &filter.tenant_id {
        let _ = write!(sql, " AND tenant_id = ${next_param}");
        binds.push(tenant.clone());
        *next_param += 1;
    }
    if let Some(allowed) = &filter.allowed_ids {
        if allowed.is_empty() {
            // Unreachable in practice (callers short-circuit on
            // `matches_nothing`), but keep the SQL itself fail-closed.
            sql.push_str(" AND FALSE");
        } else {
            let ids: Vec<String> = allowed.iter().map(i64::to_string).collect();
            let _ = write!(sql, " AND record_id IN ({})", ids.join(","));
        }
    }
    if !filter.excluded_ids.is_empty() {
        let ids: Vec<String> = filter.excluded_ids.iter().map(i64::to_string).collect();
        let _ = write!(sql, " AND record_id NOT IN ({})", ids.join(","));
    }
    for (field, value) in &filter.equals {
        if field == crate::backend::TENANT_FILTER_KEY {
            let _ = write!(sql, " AND tenant_id = ${next_param}");
        } else {
            // `field` is an indexed field name, already validated as a bare
            // identifier by `IndexDefinition::validate`; the *value* is bound.
            let _ = write!(sql, " AND fields ->> '{field}' = ${next_param}");
        }
        binds.push(value.clone());
        *next_param += 1;
    }
    sql
}

impl SearchBackend for PostgresSearchStore {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            keyword: true,
            vector: true,
            weighted_fields: true,
            embedding_readback: true,
        }
    }

    fn ensure_index<'a>(
        &'a self,
        definition: &'a IndexDefinition,
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            checked(definition)?;
            let mut conn = self.conn().await?;

            for statement in SCHEMA_STATEMENTS {
                diesel::sql_query(*statement)
                    .execute(&mut conn)
                    .await
                    .map_err(SearchError::backend)?;
            }

            // Resolve the vector storage mode once per process. A managed
            // Postgres without `pgvector` must still work, so a failed
            // `CREATE EXTENSION` degrades rather than aborting boot.
            if self.vector_mode().is_none() {
                let mode = match self.dimensions {
                    Some(dimensions) if self.try_enable_pgvector(dimensions).await? => {
                        VectorMode::PgVector { dimensions }
                    }
                    _ => VectorMode::Array,
                };
                if let Ok(mut guard) = self.vector_mode.write() {
                    *guard = Some(mode);
                }
                tracing::info!(?mode, "autumn-search postgres vector storage resolved");
            }
            Ok(())
        })
    }

    fn index<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        documents: &'a [IndexedDocument],
    ) -> BoxFuture<'a, SearchResult<()>> {
        use std::fmt::Write as _;

        Box::pin(async move {
            checked(definition)?;
            if documents.is_empty() {
                return Ok(());
            }
            let mut conn = self.conn().await?;
            let pgvector = self.vector_mode().is_some_and(VectorMode::is_pgvector);

            for document in documents {
                // The weighted tsvector, built exactly as #842 does:
                // `setweight(to_tsvector(<lang>, <value>), <weight>)` per
                // field, concatenated. Every value is BOUND; only the language
                // dictionary and the weight letter — both from the validated
                // definition — are interpolated.
                //
                // Bound parameters: $1 index_name, $2 tenant_id, $3 fields
                // json, $4 language, $5 content, then one per field value.
                let mut vector_sql = String::new();
                let mut binds: Vec<String> = Vec::new();
                for (param, field) in (6_usize..).zip(document.document.fields.iter()) {
                    if !vector_sql.is_empty() {
                        vector_sql.push_str(" || ");
                    }
                    let _ = write!(
                        vector_sql,
                        "setweight(to_tsvector($4::regconfig, ${param}), '{}')",
                        field.weight
                    );
                    binds.push(field.value.clone());
                }
                if vector_sql.is_empty() {
                    "to_tsvector($4::regconfig, '')".clone_into(&mut vector_sql);
                }

                let embedding = document.embedding.as_ref().map_or_else(
                    || "NULL".to_owned(),
                    |v| format!("'{}'::double precision[]", array_literal(v)),
                );

                // `embedding_vec` only exists when `try_enable_pgvector`
                // added it, so it must be absent from the column list in the
                // portable mode — naming a column that does not exist would
                // fail every write.
                let (vec_column, vec_value, vec_update) = if pgvector {
                    let value = document.embedding.as_ref().map_or_else(
                        || "NULL".to_owned(),
                        |v| format!("'{}'::vector", vector_literal(v)),
                    );
                    (
                        ", embedding_vec",
                        format!(", {value}"),
                        ", embedding_vec = EXCLUDED.embedding_vec",
                    )
                } else {
                    ("", String::new(), "")
                };

                let sql = format!(
                    "INSERT INTO {DOCUMENTS_TABLE} \
                       (index_name, record_id, tenant_id, language, fields, content, \
                        search_vector, embedding{vec_column}) \
                     VALUES ($1, {id}, $2, $4, $3::jsonb, $5, {vector_sql}, {embedding}{vec_value}) \
                     ON CONFLICT (index_name, record_id) DO UPDATE SET \
                       tenant_id = EXCLUDED.tenant_id, \
                       language = EXCLUDED.language, \
                       fields = EXCLUDED.fields, \
                       content = EXCLUDED.content, \
                       search_vector = EXCLUDED.search_vector, \
                       embedding = EXCLUDED.embedding{vec_update}, \
                       updated_at = NOW()",
                    id = document.id(),
                );

                let fields_json = serde_json::to_string(
                    &document
                        .document
                        .fields
                        .iter()
                        .map(|f| (f.name, f.value.clone()))
                        .collect::<std::collections::BTreeMap<_, _>>(),
                )
                .map_err(SearchError::backend)?;

                let mut query = diesel::sql_query(sql)
                    .into_boxed::<autumn_web::RuntimeBackend>()
                    .bind::<Text, _>(definition.name.to_owned())
                    .bind::<Nullable<Text>, _>(document.document.tenant_id.clone())
                    .bind::<Text, _>(fields_json)
                    .bind::<Text, _>(definition.language.to_owned())
                    .bind::<Text, _>(document.document.text());
                for value in binds {
                    query = query.bind::<Text, _>(value);
                }
                query
                    .execute(&mut conn)
                    .await
                    .map_err(SearchError::backend)?;
            }
            Ok(())
        })
    }

    fn delete<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        ids: &'a [i64],
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            checked(definition)?;
            if ids.is_empty() {
                return Ok(());
            }
            let mut conn = self.conn().await?;
            let list: Vec<String> = ids.iter().map(i64::to_string).collect();
            diesel::sql_query(format!(
                "DELETE FROM {DOCUMENTS_TABLE} WHERE index_name = $1 AND record_id IN ({})",
                list.join(",")
            ))
            .bind::<Text, _>(definition.name.to_owned())
            .execute(&mut conn)
            .await
            .map_err(SearchError::backend)?;
            Ok(())
        })
    }

    fn clear<'a>(&'a self, definition: &'a IndexDefinition) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            checked(definition)?;
            let mut conn = self.conn().await?;
            diesel::sql_query(format!(
                "DELETE FROM {DOCUMENTS_TABLE} WHERE index_name = $1"
            ))
            .bind::<Text, _>(definition.name.to_owned())
            .execute(&mut conn)
            .await
            .map_err(SearchError::backend)?;
            Ok(())
        })
    }

    fn keyword_search<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        query: &'a KeywordQuery,
    ) -> BoxFuture<'a, SearchResult<Page<SearchHit>>> {
        Box::pin(async move {
            checked(definition)?;
            // Fail closed: a blank query and an impossible filter both return
            // an empty page having issued NO query — never a full scan.
            if query_tokens(&query.text).is_none() || query.filter.matches_nothing() {
                return Ok(empty_page(&query.page));
            }

            let mut conn = self.conn().await?;
            // $1 index_name, $2 language, $3 query text.
            let mut param = 4_usize;
            let mut binds: Vec<String> = Vec::new();
            let predicate = filter_sql(&query.filter, &mut param, &mut binds);

            // `plainto_tsquery` (not `websearch_to_tsquery`): the documented
            // cross-backend contract is "every token must match, operators are
            // not syntax". It is parameterized, so a hostile query string can
            // neither inject nor widen the result set.
            let where_clause = format!(
                "index_name = $1 AND search_vector @@ plainto_tsquery($2::regconfig, $3){predicate}"
            );

            let mut count = diesel::sql_query(format!(
                "SELECT COUNT(*)::bigint AS total FROM {DOCUMENTS_TABLE} WHERE {where_clause}"
            ))
            .into_boxed::<autumn_web::RuntimeBackend>()
            .bind::<Text, _>(definition.name.to_owned())
            .bind::<Text, _>(definition.language.to_owned())
            .bind::<Text, _>(query.text.clone());
            for value in &binds {
                count = count.bind::<Text, _>(value.clone());
            }
            let total = count
                .get_result::<CountRow>(&mut conn)
                .await
                .map_err(SearchError::backend)?
                .total;

            let size = i64::from(query.page.size());
            let offset = i64::from(query.page.page().saturating_sub(1)) * size;
            let mut rows = diesel::sql_query(format!(
                "SELECT record_id, \
                        ts_rank_cd(search_vector, plainto_tsquery($2::regconfig, $3))::double precision AS score \
                 FROM {DOCUMENTS_TABLE} WHERE {where_clause} \
                 ORDER BY score DESC, record_id ASC LIMIT {size} OFFSET {offset}"
            ))
            .into_boxed::<autumn_web::RuntimeBackend>()
            .bind::<Text, _>(definition.name.to_owned())
            .bind::<Text, _>(definition.language.to_owned())
            .bind::<Text, _>(query.text.clone());
            for value in &binds {
                rows = rows.bind::<Text, _>(value.clone());
            }
            let hits = rows
                .load::<HitRow>(&mut conn)
                .await
                .map_err(SearchError::backend)?;

            Ok(Page::new(
                hits.into_iter()
                    .map(|row| {
                        SearchHit::new(definition.name, row.record_id, narrow_score(row.score))
                    })
                    .collect(),
                total,
                &query.page,
            ))
        })
    }

    fn vector_search<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        query: &'a VectorQuery,
    ) -> BoxFuture<'a, SearchResult<Vec<SearchHit>>> {
        Box::pin(async move {
            checked(definition)?;
            if !definition.supports_vector_search() {
                return Err(SearchError::VectorUnsupported {
                    index: definition.name.to_owned(),
                });
            }
            if query.filter.matches_nothing() || query.limit == 0 || query.vector.is_empty() {
                return Ok(Vec::new());
            }

            let mut conn = self.conn().await?;
            let mut param = 2_usize; // $1 index_name
            let mut binds: Vec<String> = Vec::new();
            let predicate = filter_sql(&query.filter, &mut param, &mut binds);

            // Cosine *similarity* in both modes, so the two paths order
            // identically: pgvector's `<=>` is cosine distance, hence `1 - d`.
            //
            // Width mismatch: `<=>` errors on it, while `unnest(a, b)` would
            // silently pad the shorter array with NULLs and return a wrong
            // score. The portable path therefore filters mismatched documents
            // out with `array_length`, so a re-embedding at a new width
            // degrades to "no results" rather than to wrong results. (The
            // in-memory backend raises `DimensionMismatch` instead — it can
            // afford to inspect every document; SQL cannot.)
            let width = query.vector.len();
            let (score_expr, embedding_predicate) =
                if self.vector_mode().is_some_and(VectorMode::is_pgvector) {
                    (
                        format!(
                            "(1 - (embedding_vec <=> '{}'::vector))::double precision",
                            vector_literal(&query.vector)
                        ),
                        "embedding_vec IS NOT NULL".to_owned(),
                    )
                } else {
                    (
                        format!(
                            "autumn_search_cosine(embedding, '{}'::double precision[])",
                            array_literal(&query.vector)
                        ),
                        format!("embedding IS NOT NULL AND array_length(embedding, 1) = {width}"),
                    )
                };

            // Only emit the threshold when one was asked for: it repeats the
            // (non-trivial) score expression, so an unconditional
            // `>= -3.4e38` would be pure cost.
            let threshold = query
                .min_score
                .map_or_else(String::new, |min| format!(" AND {score_expr} >= {min}"));
            let limit = query.limit;
            let mut rows = diesel::sql_query(format!(
                "SELECT record_id, {score_expr} AS score FROM {DOCUMENTS_TABLE} \
                 WHERE index_name = $1 AND {embedding_predicate}{predicate}{threshold} \
                 ORDER BY score DESC, record_id ASC LIMIT {limit}"
            ))
            .into_boxed::<autumn_web::RuntimeBackend>()
            .bind::<Text, _>(definition.name.to_owned());
            for value in &binds {
                rows = rows.bind::<Text, _>(value.clone());
            }
            let hits = rows
                .load::<HitRow>(&mut conn)
                .await
                .map_err(SearchError::backend)?;

            Ok(hits
                .into_iter()
                .map(|row| SearchHit::new(definition.name, row.record_id, narrow_score(row.score)))
                .collect())
        })
    }

    fn embedding<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        id: i64,
    ) -> BoxFuture<'a, SearchResult<Option<Vec<f32>>>> {
        Box::pin(async move {
            checked(definition)?;
            let mut conn = self.conn().await?;
            let row = diesel::sql_query(format!(
                "SELECT embedding::text AS embedding FROM {DOCUMENTS_TABLE} \
                 WHERE index_name = $1 AND record_id = {id}"
            ))
            .bind::<Text, _>(definition.name.to_owned())
            .get_results::<EmbeddingRow>(&mut conn)
            .await
            .map_err(SearchError::backend)?;

            Ok(row
                .into_iter()
                .next()
                .and_then(|row| row.embedding)
                .map(|raw| parse_vector(&raw)))
        })
    }
}

impl PostgresSearchStore {
    /// Try to install `pgvector` and add the accelerated column + index.
    ///
    /// Returns `false` (rather than erroring) when the extension is not
    /// available: the portable array path is a complete implementation, so a
    /// managed Postgres without `pgvector` must boot, not crash.
    async fn try_enable_pgvector(&self, dimensions: usize) -> SearchResult<bool> {
        let mut conn = self.conn().await?;

        if diesel::sql_query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(&mut conn)
            .await
            .is_err()
        {
            tracing::info!(
                "pgvector is not available; autumn-search will store embeddings as \
                 double precision[] and rank with autumn_search_cosine()"
            );
            return Ok(false);
        }

        let present = diesel::sql_query(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector') AS present",
        )
        .get_result::<ExistsRow>(&mut conn)
        .await
        .map_err(SearchError::backend)?
        .present;
        if !present {
            return Ok(false);
        }

        // `dimensions` is a `usize` from config, formatted by Rust — no caller
        // text reaches this statement.
        diesel::sql_query(format!(
            "ALTER TABLE {DOCUMENTS_TABLE} ADD COLUMN IF NOT EXISTS embedding_vec vector({dimensions})"
        ))
        .execute(&mut conn)
        .await
        .map_err(SearchError::backend)?;

        // Best-effort: an ivfflat index needs training data and fails on an
        // empty table in some versions. Its absence costs speed, not
        // correctness, so never fail boot for it.
        let _ = diesel::sql_query(format!(
            "CREATE INDEX IF NOT EXISTS autumn_search_documents_embedding_vec_idx \
             ON {DOCUMENTS_TABLE} USING ivfflat (embedding_vec vector_cosine_ops)"
        ))
        .execute(&mut conn)
        .await;

        Ok(true)
    }
}

// ── Document source ─────────────────────────────────────────────────────────

impl DocumentSource for PostgresSearchStore {
    fn fetch<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        ids: &'a [i64],
    ) -> BoxFuture<'a, SearchResult<Vec<SearchDocument>>> {
        Box::pin(async move {
            checked(definition)?;
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let list: Vec<String> = ids.iter().map(i64::to_string).collect();
            self.load_documents(
                definition,
                &format!("WHERE id IN ({})", list.join(",")),
                None,
            )
            .await
        })
    }

    fn scan<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        after: Option<i64>,
        limit: usize,
    ) -> BoxFuture<'a, SearchResult<Vec<SearchDocument>>> {
        Box::pin(async move {
            checked(definition)?;
            let where_clause =
                after.map_or_else(String::new, |after| format!("WHERE id > {after}"));
            self.load_documents(definition, &where_clause, Some(limit))
                .await
        })
    }
}

impl PostgresSearchStore {
    /// Project a model's table into `(id, fields json, tenant_id)`.
    ///
    /// Fully generic: the column list comes from the validated
    /// [`IndexDefinition`], so no per-model code is generated anywhere. The
    /// values are returned as one JSON object rather than N columns, which is
    /// what lets a single `QueryableByName` struct serve every model.
    async fn load_documents(
        &self,
        definition: &IndexDefinition,
        where_clause: &str,
        limit: Option<usize>,
    ) -> SearchResult<Vec<SearchDocument>> {
        // ONE connection for both statements: acquiring a second while holding
        // the first deadlocks a small pool.
        let mut conn = self.conn().await?;

        let has_tenant = diesel::sql_query(
            "SELECT EXISTS ( \
               SELECT 1 FROM information_schema.columns \
               WHERE table_schema = current_schema() \
                 AND table_name = $1 \
                 AND column_name = 'tenant_id' \
             ) AS present",
        )
        .bind::<Text, _>(definition.name.to_owned())
        .get_result::<ExistsRow>(&mut conn)
        .await
        .map_err(SearchError::backend)?
        .present;

        // Identifiers only — every name here passed `IndexDefinition::validate`.
        let projection: Vec<String> = definition
            .fields
            .iter()
            .map(|field| format!("'{0}', COALESCE(\"{0}\"::text, '')", field.name))
            .collect();
        let tenant = if has_tenant {
            "tenant_id::text"
        } else {
            "NULL::text"
        };
        let limit_clause = limit.map_or_else(String::new, |limit| format!(" LIMIT {limit}"));

        let rows = diesel::sql_query(format!(
            "SELECT id, jsonb_build_object({})::text AS fields, {tenant} AS tenant_id \
             FROM \"{}\" {where_clause} ORDER BY id ASC{limit_clause}",
            projection.join(", "),
            definition.name,
        ))
        .load::<SourceRow>(&mut conn)
        .await
        .map_err(SearchError::backend)?;

        let mut documents = Vec::with_capacity(rows.len());
        for row in rows {
            let values: std::collections::BTreeMap<String, String> =
                serde_json::from_str(&row.fields).map_err(SearchError::backend)?;
            let mut document = SearchDocument::new(definition.name, row.id);
            for field in definition.fields.iter() {
                let value = values.get(field.name).cloned().unwrap_or_default();
                document = document.with_field(field.name, field.weight, value);
            }
            if let Some(embed) = definition.embed_field
                && let Some(text) = values.get(embed)
                && !text.is_empty()
            {
                document.embed_text = Some(text.clone());
            }
            document.tenant_id = row.tenant_id;
            documents.push(document);
        }
        Ok(documents)
    }
}

/// DDL for the framework-owned index, applied idempotently on every boot.
///
/// Not a `diesel_migrations` set: the table is plugin-owned infrastructure,
/// not application schema, and every statement is `IF NOT EXISTS`, so running
/// it on each boot is both safe and self-repairing.
const SCHEMA_STATEMENTS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS autumn_search_documents (
        index_name    TEXT        NOT NULL,
        record_id     BIGINT      NOT NULL,
        tenant_id     TEXT,
        language      TEXT        NOT NULL DEFAULT 'simple',
        fields        JSONB       NOT NULL DEFAULT '{}'::jsonb,
        content       TEXT        NOT NULL DEFAULT '',
        search_vector TSVECTOR    NOT NULL DEFAULT to_tsvector('simple', ''),
        embedding     DOUBLE PRECISION[],
        updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        PRIMARY KEY (index_name, record_id)
    )",
    "CREATE INDEX IF NOT EXISTS autumn_search_documents_vector_idx
        ON autumn_search_documents USING GIN (search_vector)",
    "CREATE INDEX IF NOT EXISTS autumn_search_documents_tenant_idx
        ON autumn_search_documents (index_name, tenant_id)",
    // Portable cosine similarity for the non-pgvector path. `unnest(a, b)`
    // walks both arrays in lockstep; a zero-norm vector yields 0 rather than a
    // division by zero.
    "CREATE OR REPLACE FUNCTION autumn_search_cosine(a DOUBLE PRECISION[], b DOUBLE PRECISION[])
     RETURNS DOUBLE PRECISION LANGUAGE sql IMMUTABLE AS $$
        SELECT COALESCE(
            SUM(t.x * t.y) / NULLIF(SQRT(SUM(t.x * t.x)) * SQRT(SUM(t.y * t.y)), 0),
            0
        )
        FROM unnest(a, b) AS t(x, y)
     $$",
];

#[cfg(test)]
mod tests {
    use autumn_web::search::SearchIndexField;

    use super::*;

    const FIELDS: &[SearchIndexField] = &[
        SearchIndexField::new("title", 'A'),
        SearchIndexField::new("body", 'B'),
    ];

    fn definition() -> IndexDefinition {
        IndexDefinition::new("articles", "english", FIELDS, Some("body"))
    }

    #[test]
    fn array_literals_round_trip() {
        let vector = vec![1.0_f32, -0.5, 0.25];
        assert_eq!(parse_vector(&array_literal(&vector)), vector);
        assert_eq!(parse_vector(&vector_literal(&vector)), vector);
    }

    #[test]
    fn non_finite_values_never_produce_invalid_sql() {
        let literal = array_literal(&[f32::NAN, f32::INFINITY, 1.0]);
        assert_eq!(literal, "{0,0,1}");
        assert!(!literal.contains("NaN"));
        assert!(!literal.contains("inf"));
    }

    #[test]
    fn parsing_tolerates_empty_and_malformed_elements() {
        assert!(parse_vector("{}").is_empty());
        assert!(parse_vector("[]").is_empty());
        assert_eq!(parse_vector("{1, ,2}"), vec![1.0, 2.0]);
        assert_eq!(parse_vector("{1,oops,2}"), vec![1.0, 2.0]);
    }

    #[test]
    fn an_empty_filter_adds_no_predicate() {
        let mut param = 2;
        let mut binds = Vec::new();
        assert_eq!(
            filter_sql(&SearchFilter::default(), &mut param, &mut binds),
            ""
        );
        assert_eq!(param, 2);
        assert!(binds.is_empty());
    }

    #[test]
    fn a_tenant_filter_binds_its_value_rather_than_interpolating_it() {
        let mut param = 2;
        let mut binds = Vec::new();
        let sql = filter_sql(
            &SearchFilter::default().tenant("acme'; DROP TABLE users; --"),
            &mut param,
            &mut binds,
        );
        assert_eq!(sql, " AND tenant_id = $2");
        assert_eq!(binds, vec!["acme'; DROP TABLE users; --".to_owned()]);
        assert_eq!(param, 3);
    }

    #[test]
    fn an_empty_allowlist_renders_a_fail_closed_predicate() {
        let mut param = 2;
        let mut binds = Vec::new();
        let sql = filter_sql(
            &SearchFilter::default().allow_ids(Vec::<i64>::new()),
            &mut param,
            &mut binds,
        );
        assert!(sql.contains("AND FALSE"), "{sql}");
    }

    #[test]
    fn id_lists_are_rendered_from_integers_only() {
        let mut param = 2;
        let mut binds = Vec::new();
        let sql = filter_sql(
            &SearchFilter::default()
                .allow_ids([1_i64, 2])
                .exclude_ids([3_i64]),
            &mut param,
            &mut binds,
        );
        assert!(sql.contains("record_id IN (1,2)"), "{sql}");
        assert!(sql.contains("record_id NOT IN (3)"), "{sql}");
        assert!(binds.is_empty(), "ids are integers, nothing to bind");
    }

    #[test]
    fn equals_predicates_bind_values_and_number_parameters_in_order() {
        let mut param = 4;
        let mut binds = Vec::new();
        let sql = filter_sql(
            &SearchFilter::default()
                .equals("title", "Hello")
                .equals("body", "World"),
            &mut param,
            &mut binds,
        );
        // BTreeMap iterates in key order: body, then title.
        assert!(sql.contains("fields ->> 'body' = $4"), "{sql}");
        assert!(sql.contains("fields ->> 'title' = $5"), "{sql}");
        assert_eq!(binds, vec!["World".to_owned(), "Hello".to_owned()]);
        assert_eq!(param, 6);
    }

    #[test]
    fn an_invalid_definition_is_rejected_before_any_sql_is_built() {
        let mut bad = definition();
        bad.name = "articles\"; DROP TABLE users; --";
        assert!(matches!(checked(&bad), Err(SearchError::InvalidIndex(_))));
        assert!(checked(&definition()).is_ok());
    }

    #[test]
    fn the_schema_is_idempotent_by_construction() {
        for statement in SCHEMA_STATEMENTS {
            let statement = statement.trim_start();
            assert!(
                statement.contains("IF NOT EXISTS") || statement.starts_with("CREATE OR REPLACE"),
                "not idempotent: {statement}"
            );
        }
    }

    #[test]
    fn a_store_without_a_pool_reports_the_missing_configuration() {
        let store = PostgresSearchStore::new(None);
        let Err(error) = store.pool() else {
            panic!("a store with no installed pool must not report one");
        };
        assert!(
            error.to_string().contains("database.primary_url"),
            "{error}"
        );
        assert!(store.vector_mode().is_none());
    }

    #[test]
    fn vector_mode_reports_whether_pgvector_is_in_use() {
        assert!(VectorMode::PgVector { dimensions: 4 }.is_pgvector());
        assert!(!VectorMode::Array.is_pgvector());
    }
}
