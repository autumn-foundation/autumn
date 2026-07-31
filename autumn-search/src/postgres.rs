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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

use autumn_web::pagination::Page;
use autumn_web::search::{IndexDefinition, SearchDocument};
use diesel::sql_types::{Array, BigInt, Double, Nullable, Text};
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

/// A `sql_query` with its parameters still to be bound.
type BoxedQuery<'a> = diesel::query_builder::BoxedSqlQuery<
    'a,
    autumn_web::RuntimeBackend,
    diesel::query_builder::SqlQuery,
>;

/// Physical table every index's documents live in.
pub const DOCUMENTS_TABLE: &str = "autumn_search_documents";

/// `ts_rank_cd`'s `{D, C, B, A}` weight array, matching
/// [`weight_factor`](autumn_web::search::weight_factor).
///
/// Postgres' default is `{0.1, 0.2, 0.4, 1.0}`, but the framework's
/// cross-backend ranking contract is `D:1, C:2, B:5, A:10` — `0.5` for `B`,
/// not `0.4`. Leaving it defaulted means a query whose `B` and `C` matches
/// compete ranks differently here than in `MemorySearchBackend`, which the two
/// suites assert is the *same* contract. Kept as a literal (not formatted at
/// call time) so the statement text is stable for the prepared-statement
/// cache; `ts_rank_weights_track_the_framework_factors` fails if
/// `weight_factor` changes without this.
const TS_RANK_WEIGHTS: &str = "'{0.1, 0.2, 0.5, 1.0}'::float4[]";

/// How embeddings are physically stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
struct WatermarkRow {
    #[diesel(sql_type = Text)]
    watermark: String,
}

#[derive(diesel::QueryableByName)]
struct WidthRow {
    #[diesel(sql_type = BigInt)]
    width: i64,
}

#[derive(diesel::QueryableByName)]
struct ExistsRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    present: bool,
}

/// Which optional framework columns a model's table carries.
///
/// `bool_or` over zero rows is NULL, so both fields are nullable — a table
/// that does not exist yet reads as "neither column".
#[derive(diesel::QueryableByName)]
struct OptionalColumnsRow {
    #[diesel(sql_type = Nullable<diesel::sql_types::Bool>)]
    has_tenant: Option<bool>,
    #[diesel(sql_type = Nullable<diesel::sql_types::Bool>)]
    has_deleted_at: Option<bool>,
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
    /// Whether the shared DDL has already run in this process. The statements
    /// are identical for every index, so running them per index is N× the work
    /// and N× the concurrent-boot race window.
    schema_ready: AtomicBool,
    /// Cache of the optional framework columns each source table carries, so a
    /// backfill does not re-probe `pg_attribute` on every batch.
    optional_columns: RwLock<std::collections::HashMap<&'static str, OptionalColumns>>,
    /// Declared width of the PHYSICAL `embedding_vec` column, if it exists —
    /// which is NOT the same question as "is this process in pgvector mode".
    ///
    /// An earlier pgvector-enabled deployment can leave the column behind while
    /// this process falls back to `Array` (dimensions unset, or a width
    /// mismatch). Writes must still keep that column in step, or stale vectors
    /// survive every upsert and a pgvector-mode reader — another replica, or
    /// this one after a config change — silently ranks on obsolete content.
    vector_column_width: RwLock<Option<usize>>,
}

/// Which optional framework columns one source table carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OptionalColumns {
    tenant_id: bool,
    deleted_at: bool,
}

impl std::fmt::Debug for PostgresSearchStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresSearchStore")
            .field("pool_installed", &self.pool.get().is_some())
            .field("dimensions", &self.dimensions)
            .field("vector_mode", &self.vector_mode())
            // UFCS: `RunQueryDsl::load` is in scope and shadows `AtomicBool::load`.
            .field(
                "schema_ready",
                &AtomicBool::load(&self.schema_ready, Ordering::SeqCst),
            )
            .finish_non_exhaustive()
    }
}

impl PostgresSearchStore {
    /// Create a store whose pool is installed later.
    #[must_use]
    pub fn new(dimensions: Option<usize>) -> Self {
        Self {
            pool: OnceLock::new(),
            dimensions,
            vector_mode: RwLock::new(None),
            schema_ready: AtomicBool::new(false),
            optional_columns: RwLock::new(std::collections::HashMap::new()),
            vector_column_width: RwLock::new(None),
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

    /// Declared width of the physical `embedding_vec` column, if it exists.
    ///
    /// Independent of [`Self::vector_mode`]: the column can outlive the mode
    /// that created it.
    #[must_use]
    pub fn vector_column_width(&self) -> Option<usize> {
        self.vector_column_width
            .read()
            .ok()
            .and_then(|guard| *guard)
    }

    /// Upsert `documents`, optionally refusing to clobber rows written since
    /// `watermark`. The single write path behind both
    /// [`SearchBackend::index`] and [`SearchBackend::index_unless_newer`], so
    /// the conditional and unconditional forms cannot drift apart.
    async fn write_documents(
        &self,
        definition: &IndexDefinition,
        documents: &[IndexedDocument],
        watermark: Option<&str>,
    ) -> SearchResult<()> {
        use std::fmt::Write as _;

        checked(definition)?;
        if documents.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn().await?;
        // Keyed on the PHYSICAL column, not on this process's mode. The
        // column can outlive the mode that created it (dimensions unset, a
        // width mismatch, a mixed fleet), and skipping it then leaves a
        // stale `embedding_vec` behind after every upsert — which a
        // pgvector-mode reader silently ranks on. `None` width means the
        // column is absent and must stay out of the column list entirely.
        let vector_width = self.vector_column_width();

        for document in documents {
            // The weighted tsvector, built exactly as #842 does:
            // `setweight(to_tsvector(<lang>, <value>), <weight>)` per
            // field, concatenated.
            //
            // The weight letter is interpolated, so it comes from the
            // VALIDATED definition rather than from the document: a
            // hand-built document (or a third-party `DocumentSource`) can
            // carry any `char`. A field the index does not declare is
            // skipped entirely.
            //
            // Bound: $1 index_name, $2 record_id, $3 tenant_id, $4 fields
            // json, $5 language, $6 content, $7 embedding, ($8 vector),
            // then one per field value.
            let mut binds = vec![
                Bound::Text(definition.name.to_owned()),
                Bound::BigInt(document.id()),
                Bound::NullableText(document.document.tenant_id.clone()),
                Bound::Text(fields_json(document)?),
                Bound::Text(definition.language.to_owned()),
                Bound::Text(document.document.text()),
                Bound::NullableText(document.embedding.as_deref().map(array_literal)),
            ];
            if let Some(width) = vector_width {
                let literal = match document.embedding.as_deref() {
                    // A width the column cannot hold would fail the insert,
                    // and leaving the old value in place is exactly the
                    // staleness this avoids — so it never reaches SQL.
                    //
                    // Whether that is an ERROR depends on which column this
                    // process is actually reading. In pgvector mode k-NN
                    // runs off `embedding_vec`, so writing NULL would leave
                    // the row permanently invisible to semantic search
                    // while every write reported success — a silent hole,
                    // and the worst outcome available. Say so instead.
                    Some(embedding) if embedding.len() != width => {
                        if self.vector_mode().is_some_and(VectorMode::is_pgvector) {
                            return Err(SearchError::DimensionMismatch {
                                expected: width,
                                actual: embedding.len(),
                            });
                        }
                        // Not in pgvector mode: `embedding_vec` is a
                        // leftover column from a previous width, k-NN runs
                        // off the portable `embedding` array (bound above,
                        // at full width), and NULLing the stale copy is the
                        // correct repair rather than a loss.
                        None
                    }
                    other => other.map(vector_literal),
                };
                binds.push(Bound::NullableText(literal));
            }

            let mut vector_sql = String::new();
            let mut param = binds.len() + 1;
            for field in &document.document.fields {
                let Some(weight) = definition.weight_of(field.name) else {
                    continue;
                };
                if !vector_sql.is_empty() {
                    vector_sql.push_str(" || ");
                }
                // `$5::text::regconfig`, not `$5::regconfig`: the same
                // parameter is also the `language` column value, and the
                // untyped form is "inconsistent types deduced for
                // parameter" under any client that does not declare bind
                // types.
                let _ = write!(
                    vector_sql,
                    "setweight(to_tsvector($5::text::regconfig, ${param}), '{weight}')"
                );
                binds.push(Bound::Text(field.value.clone()));
                param += 1;
            }
            if vector_sql.is_empty() {
                "to_tsvector($5::text::regconfig, '')".clone_into(&mut vector_sql);
            }

            let (vec_column, vec_value, vec_update) = if vector_width.is_some() {
                (
                    ", embedding_vec",
                    ", $8::vector",
                    ", embedding_vec = EXCLUDED.embedding_vec",
                )
            } else {
                ("", "", "")
            };

            // The watermark guard. Without it a backfill batch — read
            // minutes ago, then delayed by an embedding round-trip —
            // overwrites whatever a per-record reindex wrote in the
            // meantime, and nothing re-runs that reindex. `updated_at` is
            // server-side `NOW()` and the watermark came from the same
            // clock, so the comparison is skew-free.
            //
            // Only the UPDATE arm is guarded: a row that does not exist
            // yet cannot have a newer version to protect, and skipping the
            // insert would leave it unindexed.
            let guard = if watermark.is_some() {
                param += 1;
                format!(
                    " WHERE {DOCUMENTS_TABLE}.updated_at <= ${}::timestamptz",
                    param - 1
                )
            } else {
                String::new()
            };
            if let Some(watermark) = watermark {
                binds.push(Bound::Text(watermark.to_owned()));
            }

            let sql = format!(
                "INSERT INTO {DOCUMENTS_TABLE} \
                       (index_name, record_id, tenant_id, language, fields, content, \
                        search_vector, embedding{vec_column}) \
                     VALUES ($1, $2, $3, $5, $4::jsonb, $6, {vector_sql}, \
                             $7::double precision[]{vec_value}) \
                     ON CONFLICT (index_name, record_id) DO UPDATE SET \
                       tenant_id = EXCLUDED.tenant_id, \
                       language = EXCLUDED.language, \
                       fields = EXCLUDED.fields, \
                       content = EXCLUDED.content, \
                       search_vector = EXCLUDED.search_vector, \
                       embedding = EXCLUDED.embedding{vec_update}, \
                       updated_at = NOW(){guard}"
            );

            bind_all(
                diesel::sql_query(sql).into_boxed::<autumn_web::RuntimeBackend>(),
                binds,
            )
            .execute(&mut conn)
            .await
            .map_err(SearchError::backend)?;
        }
        Ok(())
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
/// (`i64`, formatted by Rust) and field names taken from `definition`, which
/// `IndexDefinition::validate` has already checked are bare identifiers. Every
/// caller-supplied *value* is bound, never interpolated.
///
/// An `equals` key that `definition` does not declare renders as `AND FALSE`
/// rather than reaching SQL. A key is an identifier position, so an unchecked
/// one would let request text close the quote and `OR` away every restriction
/// that preceded it — a full widening past the visibility filter, not merely
/// an injection. Failing closed also matches [`SearchFilter::permits`], which
/// returns `false` for a field the document does not carry.
fn filter_sql(
    definition: &IndexDefinition,
    filter: &SearchFilter,
    next_param: &mut usize,
    binds: &mut Vec<Bound>,
) -> String {
    use std::fmt::Write as _;

    let mut sql = String::new();

    if let Some(tenant) = &filter.tenant_id {
        let _ = write!(sql, " AND tenant_id = ${next_param}");
        binds.push(Bound::Text(tenant.clone()));
        *next_param += 1;
    }
    if let Some(allowed) = &filter.allowed_ids {
        if allowed.is_empty() {
            // Unreachable in practice (callers short-circuit on
            // `matches_nothing`), but keep the SQL itself fail-closed.
            sql.push_str(" AND FALSE");
        } else {
            // `= ANY($n)` with a bound array, not an interpolated `IN (…)`
            // list: the literal form would mint a distinct prepared statement
            // for every distinct id set.
            let _ = write!(sql, " AND record_id = ANY(${next_param})");
            binds.push(Bound::Ids(allowed.clone()));
            *next_param += 1;
        }
    }
    if !filter.excluded_ids.is_empty() {
        let _ = write!(sql, " AND NOT (record_id = ANY(${next_param}))");
        binds.push(Bound::Ids(filter.excluded_ids.clone()));
        *next_param += 1;
    }
    for (field, value) in &filter.equals {
        if field == crate::backend::TENANT_FILTER_KEY {
            let _ = write!(sql, " AND tenant_id = ${next_param}");
        } else if definition.has_field(field) {
            // Allowlisted against the definition, whose field names
            // `IndexDefinition::validate` has already checked are bare
            // identifiers; the *value* is bound.
            let _ = write!(sql, " AND fields ->> '{field}' = ${next_param}");
        } else {
            // Not a declared field: match nothing, bind nothing, and do NOT
            // advance the parameter counter.
            sql.push_str(" AND FALSE");
            continue;
        }
        binds.push(Bound::Text(value.clone()));
        *next_param += 1;
    }
    sql
}

/// The ` AND <score> >= $n` fragment for a minimum-score bound, advancing
/// `next_param` when one is emitted.
///
/// The threshold is **bound, never interpolated**. `min_score` can come
/// straight off a request, and diesel's prepared-statement cache is keyed on
/// SQL text and never evicts — so a formatted value mints a permanent
/// statement per distinct threshold, and a caller sweeping values grows the
/// cache without bound until the server dies. That is the same failure the
/// LIMIT/OFFSET binds exist to prevent, and the same one that made
/// interpolated ids an OOM on every backfill.
///
/// Emitted only when a bound was asked for, because it repeats the
/// (non-trivial) score expression. A non-finite bound is dropped rather than
/// bound: "no threshold" is the safe reading of "not a number", and `>= NaN`
/// would silently match nothing.
fn score_threshold_sql(score_expr: &str, min_score: Option<f32>, next_param: &mut usize) -> String {
    if !min_score.is_some_and(f32::is_finite) {
        return String::new();
    }
    let sql = format!(" AND {score_expr} >= ${next_param}::double precision");
    *next_param += 1;
    sql
}

/// The bound value for [`score_threshold_sql`], if it emitted a fragment.
///
/// Kept next to it so the two cannot disagree about whether a parameter was
/// consumed — a mismatch would shift every later parameter by one.
fn score_threshold_bind(min_score: Option<f32>) -> Option<Bound> {
    min_score
        .filter(|min| min.is_finite())
        .map(|min| Bound::Double(f64::from(min)))
}

/// One bound parameter.
///
/// Exists so a query can be assembled as `(sql, Vec<Bound>)` and bound in one
/// place, rather than every call site tracking two parallel lists of differing
/// SQL types.
#[derive(Debug, Clone, PartialEq)]
enum Bound {
    Text(String),
    NullableText(Option<String>),
    BigInt(i64),
    Double(f64),
    Ids(Vec<i64>),
}

/// Bind `binds`, in order, onto a boxed query.
fn bind_all(mut query: BoxedQuery<'_>, binds: impl IntoIterator<Item = Bound>) -> BoxedQuery<'_> {
    for bound in binds {
        query = match bound {
            Bound::Text(value) => query.bind::<Text, _>(value),
            Bound::NullableText(value) => query.bind::<Nullable<Text>, _>(value),
            Bound::BigInt(value) => query.bind::<BigInt, _>(value),
            Bound::Double(value) => query.bind::<Double, _>(value),
            Bound::Ids(values) => query.bind::<Array<BigInt>, _>(values),
        };
    }
    query
}

impl SearchBackend for PostgresSearchStore {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::default()
            .with_vector(true)
            .with_weighted_fields(true)
            .with_embedding_readback(true)
            .with_conditional_index(true)
    }

    fn write_watermark(&self) -> BoxFuture<'_, SearchResult<Option<String>>> {
        Box::pin(async move {
            // The DATABASE's clock, not the process's. `updated_at` is set by
            // `NOW()` server-side, so comparing it against an app-side
            // timestamp would be at the mercy of clock skew between however
            // many app instances and the database — and the whole point of the
            // watermark is deciding which of two writes is newer.
            let mut conn = self.conn().await?;
            let row = diesel::sql_query("SELECT NOW()::text AS watermark")
                .get_result::<WatermarkRow>(&mut conn)
                .await
                .map_err(SearchError::backend)?;
            Ok(Some(row.watermark))
        })
    }

    fn index_unless_newer<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        documents: &'a [IndexedDocument],
        watermark: Option<&'a str>,
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move { self.write_documents(definition, documents, watermark).await })
    }

    fn ensure_index<'a>(
        &'a self,
        definition: &'a IndexDefinition,
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            checked(definition)?;
            let mut conn = self.conn().await?;

            // The DDL is the same for every index, so run it once per process
            // rather than once per registered model.
            if !self.schema_ready.swap(true, Ordering::SeqCst)
                && let Err(error) = Self::apply_schema(&mut conn).await
            {
                // Let a later index retry the DDL rather than leaving the
                // process permanently believing the schema exists.
                self.schema_ready.store(false, Ordering::SeqCst);
                return Err(error);
            }

            // Resolve the vector storage mode once per process. A managed
            // Postgres without `pgvector` must still work, so a failed
            // `CREATE EXTENSION` degrades rather than aborting boot.
            if self.vector_mode().is_none() {
                let mode = match self.dimensions {
                    // Threaded `&mut conn` rather than acquiring a second
                    // connection: holding two at boot deadlocks a pool of one.
                    Some(dimensions) if self.try_enable_pgvector(&mut conn, dimensions).await? => {
                        VectorMode::PgVector { dimensions }
                    }
                    _ => VectorMode::Array,
                };
                if let Ok(mut guard) = self.vector_mode.write() {
                    *guard = Some(mode);
                }

                // Probe the PHYSICAL column regardless of the resolved mode: it
                // may have been created by an earlier deployment, and writes
                // have to keep it in step either way.
                let width = Self::vector_column_width_of(&mut conn).await?;
                if let Ok(mut guard) = self.vector_column_width.write() {
                    *guard = width;
                }
                tracing::info!(
                    ?mode,
                    vector_column_width = ?width,
                    "autumn-search postgres vector storage resolved"
                );
            }
            Ok(())
        })
    }

    fn index<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        documents: &'a [IndexedDocument],
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move { self.write_documents(definition, documents, None).await })
    }

    fn delete<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        ids: &'a [i64],
    ) -> BoxFuture<'a, SearchResult<()>> {
        Box::pin(async move {
            checked(definition)?;
            // `IN ()` is a syntax error, so an empty slice must never reach SQL.
            if ids.is_empty() {
                return Ok(());
            }
            let mut conn = self.conn().await?;
            bind_all(
                diesel::sql_query(format!(
                    "DELETE FROM {DOCUMENTS_TABLE} \
                     WHERE index_name = $1 AND record_id = ANY($2)"
                ))
                .into_boxed::<autumn_web::RuntimeBackend>(),
                [
                    Bound::Text(definition.name.to_owned()),
                    Bound::Ids(ids.to_vec()),
                ],
            )
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
            let mut binds: Vec<Bound> = Vec::new();
            let predicate = filter_sql(definition, &query.filter, &mut param, &mut binds);
            let head = [
                Bound::Text(definition.name.to_owned()),
                Bound::Text(definition.language.to_owned()),
                Bound::Text(query.text.clone()),
            ];

            // `plainto_tsquery` (not `websearch_to_tsquery`): the documented
            // cross-backend contract is "every token must match, operators are
            // not syntax". It is parameterized, so a hostile query string can
            // neither inject nor widen the result set.
            let where_clause = format!(
                "index_name = $1 \
                 AND search_vector @@ plainto_tsquery($2::text::regconfig, $3){predicate}"
            );

            let total = bind_all(
                diesel::sql_query(format!(
                    "SELECT COUNT(*)::bigint AS total FROM {DOCUMENTS_TABLE} WHERE {where_clause}"
                ))
                .into_boxed::<autumn_web::RuntimeBackend>(),
                head.iter().cloned().chain(binds.iter().cloned()),
            )
            .get_result::<CountRow>(&mut conn)
            .await
            .map_err(SearchError::backend)?
            .total;

            // LIMIT/OFFSET are BOUND: interpolating them would mint a distinct
            // prepared statement per page, which the unbounded statement cache
            // never evicts.
            let limit_param = param;
            let offset_param = param + 1;
            let size = i64::from(query.page.size());
            let offset = i64::from(query.page.page().saturating_sub(1)) * size;
            let rows = bind_all(
                diesel::sql_query(format!(
                    "SELECT record_id, \
                            ts_rank_cd({TS_RANK_WEIGHTS}, search_vector, \
                                       plainto_tsquery($2::text::regconfig, $3))::double precision \
                              AS score \
                     FROM {DOCUMENTS_TABLE} WHERE {where_clause} \
                     ORDER BY score DESC, record_id ASC \
                     LIMIT ${limit_param} OFFSET ${offset_param}"
                ))
                .into_boxed::<autumn_web::RuntimeBackend>(),
                head.into_iter()
                    .chain(binds)
                    .chain([Bound::BigInt(size), Bound::BigInt(offset)]),
            )
            .load::<HitRow>(&mut conn)
            .await
            .map_err(SearchError::backend)?;

            Ok(Page::new(
                rows.into_iter()
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
            // $1 index_name, $2 the query vector (bound as text, cast in SQL).
            let mut param = 3_usize;
            let mut binds: Vec<Bound> = Vec::new();
            let predicate = filter_sql(definition, &query.filter, &mut param, &mut binds);

            let pgvector = self.vector_mode().is_some_and(VectorMode::is_pgvector);
            let width = query.vector.len();
            // Both modes score cosine SIMILARITY so the two orderings agree
            // (pgvector's `<=>` is cosine distance, hence `1 - d`).
            //
            // Ordering differs, though, and deliberately: an ivfflat index can
            // only serve `ORDER BY col <=> const ASC`. Ordering by the derived
            // `1 - d` expression is opaque to the planner and forces an exact
            // full scan, which would make the whole pgvector fast path buy
            // nothing. So pgvector mode orders by DISTANCE ascending and
            // converts to similarity only in the select list.
            let (query_vector, distance, score_expr, order_by, embedding_predicate) = if pgvector {
                (
                    vector_literal(&query.vector),
                    "(embedding_vec <=> $2::vector)".to_owned(),
                    "(1 - (embedding_vec <=> $2::vector))::double precision".to_owned(),
                    "ORDER BY (embedding_vec <=> $2::vector) ASC, record_id ASC".to_owned(),
                    "embedding_vec IS NOT NULL".to_owned(),
                )
            } else {
                let expr = "autumn_search_cosine(embedding, $2::double precision[])".to_owned();
                (
                    array_literal(&query.vector),
                    String::new(),
                    expr.clone(),
                    format!("ORDER BY {expr} DESC, record_id ASC"),
                    // `unnest(a, b)` pads the shorter array with NULLs and
                    // silently mis-scores, so a width mismatch is excluded
                    // rather than ranked. (pgvector's `<=>` errors instead —
                    // documented divergence.)
                    format!("embedding IS NOT NULL AND array_length(embedding, 1) = {width}"),
                )
            };
            let _ = distance;

            let threshold = score_threshold_sql(&score_expr, query.min_score, &mut param);

            let limit_param = param;
            let limit = i64::try_from(query.limit).unwrap_or(i64::MAX);
            let rows = bind_all(
                diesel::sql_query(format!(
                    "SELECT record_id, {score_expr} AS score FROM {DOCUMENTS_TABLE} \
                     WHERE index_name = $1 AND {embedding_predicate}{predicate}{threshold} \
                     {order_by} LIMIT ${limit_param}"
                ))
                .into_boxed::<autumn_web::RuntimeBackend>(),
                [
                    Bound::Text(definition.name.to_owned()),
                    Bound::Text(query_vector),
                ]
                .into_iter()
                .chain(binds)
                // The threshold parameter, when one was emitted, sits between
                // the filter binds and the limit — matching the order `param`
                // handed the numbers out above.
                .chain(score_threshold_bind(query.min_score))
                .chain([Bound::BigInt(limit)]),
            )
            .load::<HitRow>(&mut conn)
            .await
            .map_err(SearchError::backend)?;

            Ok(rows
                .into_iter()
                // A zero-norm vector makes pgvector's `<=>` return NaN, which
                // sorts FIRST under `DESC` — a garbage row would rank #1 with
                // a displayed score of 0. Drop those rather than surface them.
                .filter(|row| row.score.is_finite())
                .map(|row| SearchHit::new(definition.name, row.record_id, narrow_score(row.score)))
                .collect())
        })
    }

    fn embedding<'a>(
        &'a self,
        definition: &'a IndexDefinition,
        id: i64,
        filter: &'a SearchFilter,
    ) -> BoxFuture<'a, SearchResult<Option<Vec<f32>>>> {
        Box::pin(async move {
            checked(definition)?;
            // A record the filter excludes reads back as absent — this is a
            // query like any other, and the seed id is caller-supplied.
            if filter.matches_nothing() {
                return Ok(None);
            }
            let mut conn = self.conn().await?;
            // $1 index_name, $2 record_id.
            let mut param = 3_usize;
            let mut binds: Vec<Bound> = Vec::new();
            let predicate = filter_sql(definition, filter, &mut param, &mut binds);

            let rows = bind_all(
                diesel::sql_query(format!(
                    "SELECT embedding::text AS embedding FROM {DOCUMENTS_TABLE} \
                     WHERE index_name = $1 AND record_id = $2{predicate}"
                ))
                .into_boxed::<autumn_web::RuntimeBackend>(),
                [Bound::Text(definition.name.to_owned()), Bound::BigInt(id)]
                    .into_iter()
                    .chain(binds),
            )
            .load::<EmbeddingRow>(&mut conn)
            .await
            .map_err(SearchError::backend)?;

            Ok(rows
                .into_iter()
                .next()
                .and_then(|row| row.embedding)
                .map(|raw| parse_vector(&raw)))
        })
    }
}

impl PostgresSearchStore {
    /// Apply the shared DDL under an advisory lock.
    ///
    /// `CREATE TABLE/INDEX IF NOT EXISTS` and `CREATE OR REPLACE FUNCTION` are
    /// **not** race-free: concurrent boots hit `duplicate key value violates
    /// unique constraint "pg_type_typname_nsp_index"` and `tuple concurrently
    /// updated`. Without this, a rolling deploy would intermittently abort a
    /// replica's boot. A session-level advisory lock serializes them; it is
    /// released explicitly below and, if the process dies, when the connection
    /// closes.
    async fn apply_schema(
        conn: &mut diesel_async::pooled_connection::deadpool::Object<autumn_web::RuntimeConnection>,
    ) -> SearchResult<()> {
        const LOCK: &str = "hashtext('autumn_search_schema')::bigint";

        diesel::sql_query(format!("SELECT pg_advisory_lock({LOCK})"))
            .execute(&mut *conn)
            .await
            .map_err(SearchError::backend)?;

        let mut outcome = Ok(());
        for statement in SCHEMA_STATEMENTS {
            if let Err(error) = diesel::sql_query(*statement).execute(&mut *conn).await {
                outcome = Err(SearchError::backend(error));
                break;
            }
        }

        // Released on every path, including the error one. Each DDL statement
        // is its own transaction under autocommit, so a failure above does not
        // leave the session unable to run this.
        let _ = diesel::sql_query(format!("SELECT pg_advisory_unlock({LOCK})"))
            .execute(&mut *conn)
            .await;

        outcome
    }

    /// Declared width of the physical `embedding_vec` column, or `None` when
    /// the column does not exist.
    ///
    /// `atttypmod` carries a `vector`'s declared dimension count. Resolved via
    /// `to_regclass` so it follows the same `search_path` the queries do.
    async fn vector_column_width_of(
        conn: &mut diesel_async::pooled_connection::deadpool::Object<autumn_web::RuntimeConnection>,
    ) -> SearchResult<Option<usize>> {
        let rows = diesel::sql_query(format!(
            "SELECT COALESCE(atttypmod, 0)::bigint AS width FROM pg_attribute \
             WHERE attrelid = to_regclass('{DOCUMENTS_TABLE}') \
               AND attname = 'embedding_vec' AND NOT attisdropped"
        ))
        .get_results::<WidthRow>(conn)
        .await
        .map_err(SearchError::backend)?;

        Ok(rows
            .into_iter()
            .next()
            .and_then(|row| usize::try_from(row.width).ok()))
    }

    /// Try to install `pgvector` and add the accelerated column + index.
    ///
    /// Returns `false` (rather than erroring) when the extension is not
    /// available: the portable array path is a complete implementation, so a
    /// managed Postgres without `pgvector` must boot, not crash.
    ///
    /// Takes the caller's connection: acquiring a second one while the caller
    /// holds the first deadlocks a pool of size 1 at boot.
    async fn try_enable_pgvector(
        &self,
        conn: &mut diesel_async::pooled_connection::deadpool::Object<autumn_web::RuntimeConnection>,
        dimensions: usize,
    ) -> SearchResult<bool> {
        if diesel::sql_query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(&mut *conn)
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
        .get_result::<ExistsRow>(&mut *conn)
        .await
        .map_err(SearchError::backend)?
        .present;
        if !present {
            return Ok(false);
        }

        // `dimensions` is a `usize` from config, formatted by Rust — no caller
        // text reaches this statement.
        diesel::sql_query(format!(
            "ALTER TABLE {DOCUMENTS_TABLE} \
             ADD COLUMN IF NOT EXISTS embedding_vec vector({dimensions})"
        ))
        .execute(&mut *conn)
        .await
        .map_err(SearchError::backend)?;

        // `ADD COLUMN IF NOT EXISTS` is a silent no-op against an existing
        // column of a DIFFERENT width, which would leave the store believing
        // it is in `PgVector{new}` while every insert fails with "expected
        // <old> dimensions". Verify what is actually there.
        let existing_width = Self::vector_column_width_of(conn)
            .await?
            .and_then(|w| i64::try_from(w).ok());
        if let Some(width) = existing_width
            && width > 0
            && usize::try_from(width).unwrap_or(0) != dimensions
        {
            tracing::warn!(
                configured = dimensions,
                existing = width,
                "autumn-search: the pgvector column has a different width than \
                 `search.embedding_dimensions`; falling back to the portable \
                 double precision[] path. Drop `autumn_search_documents.embedding_vec` \
                 and re-run a full backfill to adopt the new width."
            );
            return Ok(false);
        }

        // Adopt embeddings written while the store was in Array mode —
        // otherwise every pre-existing document silently vanishes from k-NN
        // (the query filters `embedding_vec IS NOT NULL`) until a full
        // backfill happens to run. Rows written *after* this point stay in step
        // via `index()`, which keys on the physical column rather than on the
        // mode.
        diesel::sql_query(format!(
            "UPDATE {DOCUMENTS_TABLE} SET embedding_vec = embedding::text::vector \
             WHERE embedding IS NOT NULL AND embedding_vec IS NULL \
               AND array_length(embedding, 1) = {dimensions}"
        ))
        .execute(&mut *conn)
        .await
        .map_err(SearchError::backend)?;

        // Best-effort: an ivfflat index needs training data and fails on an
        // empty table in some versions. Its absence costs speed, not
        // correctness, so never fail boot for it.
        let _ = diesel::sql_query(format!(
            "CREATE INDEX IF NOT EXISTS autumn_search_documents_embedding_vec_idx \
             ON {DOCUMENTS_TABLE} USING ivfflat (embedding_vec vector_cosine_ops) \
             WITH (lists = 100)"
        ))
        .execute(&mut *conn)
        .await;

        Ok(true)
    }
}

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
            self.load_documents(definition, Selection::Ids(ids)).await
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
            self.load_documents(definition, Selection::After { after, limit })
                .await
        })
    }
}

/// Which source rows to project.
enum Selection<'a> {
    /// Exactly these ids (a reindex).
    Ids(&'a [i64]),
    /// The next `limit` rows after `after`, by ascending id (a backfill).
    After { after: Option<i64>, limit: usize },
}

impl PostgresSearchStore {
    /// The optional framework columns `definition`'s source table carries,
    /// cached per index.
    ///
    /// Resolved through `to_regclass`, which follows the connection's
    /// `search_path` exactly as the projection below does. An
    /// `information_schema` probe filtered on `current_schema()` would answer
    /// about a different table whenever the model lives in a non-first schema
    /// — silently blanking `tenant_id` and re-indexing soft-deleted rows.
    async fn optional_columns(
        &self,
        definition: &IndexDefinition,
        conn: &mut diesel_async::pooled_connection::deadpool::Object<autumn_web::RuntimeConnection>,
    ) -> SearchResult<OptionalColumns> {
        if let Ok(cache) = self.optional_columns.read()
            && let Some(columns) = cache.get(definition.name)
        {
            return Ok(*columns);
        }

        let row = diesel::sql_query(
            "SELECT bool_or(attname = 'tenant_id')  AS has_tenant, \
                    bool_or(attname = 'deleted_at') AS has_deleted_at \
             FROM pg_attribute \
             WHERE attrelid = to_regclass($1) AND attnum > 0 AND NOT attisdropped",
        )
        .bind::<Text, _>(definition.name.to_owned())
        .get_result::<OptionalColumnsRow>(conn)
        .await
        .map_err(SearchError::backend)?;

        let columns = OptionalColumns {
            tenant_id: row.has_tenant.unwrap_or(false),
            deleted_at: row.has_deleted_at.unwrap_or(false),
        };
        if let Ok(mut cache) = self.optional_columns.write() {
            cache.insert(definition.name, columns);
        }
        Ok(columns)
    }

    /// Project a model's table into `(id, fields json, tenant_id)`.
    ///
    /// Fully generic: the column list comes from the validated
    /// [`IndexDefinition`], so no per-model code is generated anywhere. The
    /// values are returned as one JSON object rather than N columns, which is
    /// what lets a single `QueryableByName` struct serve every model.
    ///
    /// The key column comes from [`IndexDefinition::key_column`], which
    /// `#[model]` fills in from the `#[id]` field — so a model keyed on
    /// `article_id` backfills as readily as one keyed on `id`. `validate()`
    /// has already checked it is a bare identifier; it is quoted here anyway,
    /// so a column named after a reserved word still works.
    async fn load_documents(
        &self,
        definition: &IndexDefinition,
        selection: Selection<'_>,
    ) -> SearchResult<Vec<SearchDocument>> {
        // ONE connection for both statements: acquiring a second while holding
        // the first deadlocks a small pool.
        let mut conn = self.conn().await?;
        let columns = self.optional_columns(definition, &mut conn).await?;
        let key = format!("\"{}\"", definition.key_column);

        let mut binds: Vec<Bound> = Vec::new();
        let mut param = 1_usize;
        let mut predicates: Vec<String> = Vec::new();
        let mut limit_clause = String::new();

        match selection {
            Selection::Ids(ids) => {
                predicates.push(format!("{key} = ANY(${param})"));
                binds.push(Bound::Ids(ids.to_vec()));
            }
            Selection::After { after, limit } => {
                if let Some(after) = after {
                    predicates.push(format!("{key} > ${param}"));
                    binds.push(Bound::BigInt(after));
                    param += 1;
                }
                limit_clause = format!(" LIMIT ${param}");
                binds.push(Bound::BigInt(i64::try_from(limit).unwrap_or(i64::MAX)));
            }
        }
        // BOTH conditions: the column has to exist, and the model's repository
        // has to actually be `soft_delete`.
        //
        // Column presence alone is not the question. A `deleted_at` that is
        // audit history — a supported shape, whose finders return those rows —
        // would otherwise be read as a tombstone here: reindex would see the
        // row as absent and delete its document, and a purging backfill would
        // drop it entirely. The index would hide records the app still shows.
        //
        // When the repository IS `soft_delete`, the filter is required for the
        // mirror-image reason: `after_delete_commit` removes the document, and
        // a later backfill would put it straight back — searchable while
        // `find` hides it.
        if columns.deleted_at && definition.soft_delete {
            predicates.push("deleted_at IS NULL".to_owned());
        }
        let where_clause = if predicates.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", predicates.join(" AND "))
        };

        let tenant = if columns.tenant_id {
            "tenant_id::text"
        } else {
            "NULL::text"
        };

        // `::bigint`: the row is decoded as `BigInt`, so an `integer`/`serial`
        // key column would be a runtime deserialization error the compiler
        // cannot see. Aliased to `id` because that is what `SourceRow` names.
        let rows = bind_all(
            diesel::sql_query(format!(
                "SELECT {key}::bigint AS id, {} AS fields, {tenant} AS tenant_id \
                 FROM \"{}\" {where_clause} ORDER BY {key} ASC{limit_clause}",
                fields_projection(definition),
                definition.name,
            ))
            .into_boxed::<autumn_web::RuntimeBackend>(),
            binds,
        )
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

/// The `jsonb_build_object(...)` projection for `definition`'s fields.
///
/// Chunked and concatenated with `||`: `jsonb_build_object` is variadic-"any"
/// and Postgres caps a function call at **100 arguments**, so a model with
/// more than 50 searchable fields would otherwise be a hard
/// `cannot pass more than 100 arguments to a function` error.
fn fields_projection(definition: &IndexDefinition) -> String {
    /// 40 pairs = 80 arguments, comfortably under the cap.
    const PAIRS_PER_CHUNK: usize = 40;

    let pairs: Vec<String> = definition
        .fields
        .iter()
        .map(|field| format!("'{0}', COALESCE(\"{0}\"::text, '')", field.name))
        .collect();
    if pairs.is_empty() {
        return "'{}'::jsonb::text".to_owned();
    }
    let merged = pairs
        .chunks(PAIRS_PER_CHUNK)
        .map(|chunk| format!("jsonb_build_object({})", chunk.join(", ")))
        .collect::<Vec<_>>()
        .join(" || ");
    // Parenthesized before the cast: `::` binds TIGHTER than `||`, so
    // `a || b::text` casts only the final chunk and then asks Postgres for
    // `jsonb || text` — which is not a jsonb merge. The result is either an
    // operator error or two objects concatenated into invalid JSON that
    // `load_documents` then fails to deserialize. Only reachable past the
    // chunk boundary, which is exactly the case the chunking exists to serve.
    format!("({merged})::text")
}

/// The `fields` JSON one document is stored with.
fn fields_json(document: &IndexedDocument) -> SearchResult<String> {
    serde_json::to_string(
        &document
            .document
            .fields
            .iter()
            .map(|f| (f.name, f.value.clone()))
            .collect::<std::collections::BTreeMap<_, _>>(),
    )
    .map_err(SearchError::backend)
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
        IndexDefinition::new("articles", "english", FIELDS, Some("body"), false)
    }

    #[test]
    fn a_min_score_is_bound_so_the_statement_cache_stays_bounded() {
        // The property that matters is not "the value is correct" but "the SQL
        // TEXT does not vary with the value" — diesel's statement cache is
        // keyed on that text and never evicts, so a request-controlled
        // threshold would otherwise leak one permanent prepared statement per
        // distinct value.
        let expr = "(1 - (embedding_vec <=> $2::vector))::double precision";
        let mut a = 5;
        let mut b = 5;
        let first = score_threshold_sql(expr, Some(0.25), &mut a);
        let second = score_threshold_sql(expr, Some(0.999_999), &mut b);
        assert_eq!(
            first, second,
            "two thresholds must produce ONE statement, not two"
        );
        assert_eq!(a, b, "and consume the same parameter slot");
        assert!(first.contains("$5"), "{first}");
        assert!(
            !first.contains("0.25"),
            "the value must not reach the SQL text: {first}"
        );

        // No bound: no fragment, no parameter consumed.
        let mut param = 5;
        assert!(score_threshold_sql(expr, None, &mut param).is_empty());
        assert_eq!(param, 5);

        // Non-finite: dropped rather than bound. `>= NaN` matches nothing.
        let mut param = 5;
        assert!(score_threshold_sql(expr, Some(f32::NAN), &mut param).is_empty());
        assert!(score_threshold_sql(expr, Some(f32::INFINITY), &mut param).is_empty());
        assert_eq!(param, 5);

        // The fragment and its bind must agree on whether a slot was used —
        // a disagreement would shift every later parameter by one.
        for candidate in [None, Some(0.5), Some(f32::NAN), Some(f32::INFINITY)] {
            let mut param = 5;
            let emitted = !score_threshold_sql(expr, candidate, &mut param).is_empty();
            assert_eq!(
                emitted,
                score_threshold_bind(candidate).is_some(),
                "fragment/bind disagreement for {candidate:?}"
            );
            assert_eq!(param, if emitted { 6 } else { 5 });
        }
    }

    #[test]
    fn ts_rank_weights_track_the_framework_factors() {
        // Postgres' own default is `{0.1, 0.2, 0.4, 1.0}`; the framework's
        // contract normalizes to `{0.1, 0.2, 0.5, 1.0}`. `B` is the one that
        // differs, which is precisely the weight `#[searchable(weight = "B")]`
        // gives a body field — so leaving it defaulted makes Postgres rank
        // differently from `MemorySearchBackend` on the most common model
        // shape there is.
        use autumn_web::search::weight_factor;

        // Compared numerically, not as text: a string comparison would depend
        // on float formatting (`1.0` renders as `1`) and a fixed-precision
        // format would hide a genuine drift in a small ratio.
        let declared = parse_vector(
            TS_RANK_WEIGHTS
                .trim_end_matches("::float4[]")
                .trim_matches('\''),
        );
        let top = weight_factor('A');
        let expected: Vec<f32> = ['D', 'C', 'B', 'A']
            .into_iter()
            .map(|weight| weight_factor(weight) / top)
            .collect();
        assert_eq!(
            declared.len(),
            expected.len(),
            "ts_rank_cd takes exactly {{D, C, B, A}}: {TS_RANK_WEIGHTS}"
        );
        for (index, (got, want)) in declared.iter().zip(&expected).enumerate() {
            assert!(
                (got - want).abs() < 1e-6,
                "weight {} is {got}, expected {want} — the SQL array must stay in step with \
                 `weight_factor`",
                ['D', 'C', 'B', 'A'][index]
            );
        }
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
            filter_sql(
                &definition(),
                &SearchFilter::default(),
                &mut param,
                &mut binds
            ),
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
            &definition(),
            &SearchFilter::default().tenant("acme'; DROP TABLE users; --"),
            &mut param,
            &mut binds,
        );
        assert_eq!(sql, " AND tenant_id = $2");
        assert_eq!(
            binds,
            vec![Bound::Text("acme'; DROP TABLE users; --".to_owned())]
        );
        assert_eq!(param, 3);
    }

    #[test]
    fn an_empty_allowlist_renders_a_fail_closed_predicate() {
        let mut param = 2;
        let mut binds = Vec::new();
        let sql = filter_sql(
            &definition(),
            &SearchFilter::default().allow_ids(Vec::<i64>::new()),
            &mut param,
            &mut binds,
        );
        assert!(sql.contains("AND FALSE"), "{sql}");
    }

    #[test]
    fn id_lists_are_bound_as_arrays_never_interpolated() {
        let mut param = 2;
        let mut binds = Vec::new();
        let sql = filter_sql(
            &definition(),
            &SearchFilter::default()
                .allow_ids([1_i64, 2])
                .exclude_ids([3_i64]),
            &mut param,
            &mut binds,
        );
        // Bound arrays, not interpolated `IN (…)` lists: the literal form
        // would mint a distinct (never-evicted) prepared statement for every
        // distinct id set a caller happens to filter on.
        assert!(sql.contains("record_id = ANY($2)"), "{sql}");
        assert!(sql.contains("NOT (record_id = ANY($3))"), "{sql}");
        assert!(
            !sql.contains("IN ("),
            "ids must never be interpolated: {sql}"
        );
        assert_eq!(binds, vec![Bound::Ids(vec![1, 2]), Bound::Ids(vec![3])]);
    }

    #[test]
    fn equals_predicates_bind_values_and_number_parameters_in_order() {
        let mut param = 4;
        let mut binds = Vec::new();
        let sql = filter_sql(
            &definition(),
            &SearchFilter::default()
                .equals("title", "Hello")
                .equals("body", "World"),
            &mut param,
            &mut binds,
        );
        // BTreeMap iterates in key order: body, then title.
        assert!(sql.contains("fields ->> 'body' = $4"), "{sql}");
        assert!(sql.contains("fields ->> 'title' = $5"), "{sql}");
        assert_eq!(
            binds,
            vec![
                Bound::Text("World".to_owned()),
                Bound::Text("Hello".to_owned())
            ]
        );
        assert_eq!(param, 6);
    }

    #[test]
    fn an_undeclared_equals_key_never_reaches_sql() {
        // A field name is an IDENTIFIER position. Left unchecked, a
        // request-supplied facet key like `title' = 'x' OR '1'='1` would close
        // the quote and OR away the tenant and allowlist predicates that
        // precede it — widening past the visibility filter, not merely
        // injecting. It must fail closed instead.
        let mut param = 4;
        let mut binds = Vec::new();
        let sql = filter_sql(
            &definition(),
            &SearchFilter::default().equals("title' = 'x' OR '1'='1", "boom"),
            &mut param,
            &mut binds,
        );

        assert_eq!(sql, " AND FALSE", "{sql}");
        assert!(!sql.contains("OR"), "{sql}");
        assert!(binds.is_empty(), "nothing may be bound for a dropped key");
        assert_eq!(param, 4, "the parameter counter must not advance");
    }

    #[test]
    fn an_undeclared_key_does_not_disturb_the_numbering_of_declared_ones() {
        let mut param = 4;
        let mut binds = Vec::new();
        let sql = filter_sql(
            &definition(),
            &SearchFilter::default()
                .equals("aaa_not_a_field", "x")
                .equals("title", "Hello"),
            &mut param,
            &mut binds,
        );

        // BTreeMap order puts the bogus key first; the real one must still
        // bind at $4.
        assert!(sql.contains("fields ->> 'title' = $4"), "{sql}");
        assert!(sql.contains("AND FALSE"), "{sql}");
        assert_eq!(binds, vec![Bound::Text("Hello".to_owned())]);
        assert_eq!(param, 5);
    }

    #[test]
    fn the_reserved_tenant_key_is_still_honoured() {
        let mut param = 2;
        let mut binds = Vec::new();
        let sql = filter_sql(
            &definition(),
            &SearchFilter::default().equals(crate::backend::TENANT_FILTER_KEY, "acme"),
            &mut param,
            &mut binds,
        );
        assert_eq!(sql, " AND tenant_id = $2");
        assert_eq!(binds, vec![Bound::Text("acme".to_owned())]);
    }

    #[test]
    fn the_fields_projection_casts_the_whole_merge_not_just_the_last_chunk() {
        // `::` binds tighter than `||`, so an unparenthesized
        // `a || b::text` would cast only `b` and stop being a jsonb merge.
        const PAIRS_PER_CHUNK: usize = 40;

        // Under the chunk boundary: one call, still parenthesized.
        let single = fields_projection(&definition());
        assert!(single.starts_with("(jsonb_build_object("), "{single}");
        assert!(single.ends_with(")::text"), "{single}");
        assert_eq!(single.matches("jsonb_build_object(").count(), 1);

        // Past it: several calls merged with `||`, and the cast must apply to
        // the whole expression.
        let fields: Vec<SearchIndexField> = (0..=(PAIRS_PER_CHUNK * 2))
            .map(|i| SearchIndexField::new(format!("field_{i}").leak(), 'D'))
            .collect();
        let mut wide = definition();
        wide.fields = std::borrow::Cow::Owned(fields);
        wide.embed_field = None;

        let projection = fields_projection(&wide);
        assert_eq!(
            projection.matches("jsonb_build_object(").count(),
            3,
            "81 pairs at 40/chunk is 3 calls: {projection}"
        );
        assert!(projection.contains(" || "), "{projection}");
        assert!(projection.starts_with('('), "{projection}");
        assert!(projection.ends_with(")::text"), "{projection}");
        // The cast must not sit directly on a chunk.
        assert!(
            !projection.contains(")::text ||"),
            "the cast must wrap the merge, not a chunk: {projection}"
        );
    }

    #[test]
    fn an_empty_field_list_still_yields_valid_json() {
        let mut empty = definition();
        empty.fields = std::borrow::Cow::Borrowed(&[]);
        assert_eq!(fields_projection(&empty), "'{}'::jsonb::text");
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
