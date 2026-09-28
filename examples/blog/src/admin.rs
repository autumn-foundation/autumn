use autumn_admin_plugin::prelude::*;
use autumn_admin_plugin::{
    AdminHistoryEntry, AdminHistoryPage, AdminImportRowResult, CsvImportMode,
};
use diesel::OptionalExtension;
use diesel::prelude::*;
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::deadpool::Pool;
use serde_json::Value;

use crate::models::{NewPost, Post, UpdatePost};
use crate::schema::posts;

#[derive(Clone, Copy, Default)]
pub struct PostAdmin;

impl PostAdmin {
    fn pool_error(error: impl std::fmt::Display) -> AdminError {
        AdminError::Database(error.to_string())
    }

    fn validation_error(error: impl std::fmt::Display) -> AdminError {
        AdminError::Validation(error.to_string())
    }

    fn other_error(error: impl std::fmt::Display) -> AdminError {
        AdminError::Other(error.to_string())
    }

    fn serialize_post(post: Post) -> Result<Value, AdminError> {
        serde_json::to_value(post).map_err(Self::other_error)
    }

    fn apply_filters<'a>(
        mut query: posts::BoxedQuery<'a, diesel::pg::Pg>,
        params: &'a ListParams,
    ) -> posts::BoxedQuery<'a, diesel::pg::Pg> {
        if let Some(search) = params.search.as_deref() {
            let pattern = format!("%{search}%");
            query = query.filter(
                posts::title
                    .ilike(pattern.clone())
                    .or(posts::slug.ilike(pattern.clone()))
                    .or(posts::body.ilike(pattern)),
            );
        }

        for (name, value) in &params.filters {
            match name.as_str() {
                "published" => match value.as_str() {
                    "true" | "1" | "yes" => query = query.filter(posts::published.eq(true)),
                    "false" | "0" | "no" => query = query.filter(posts::published.eq(false)),
                    _ => {}
                },
                "slug" => query = query.filter(posts::slug.ilike(value)),
                _ => {}
            }
        }

        query
    }

    fn apply_sort<'a>(
        mut query: posts::BoxedQuery<'a, diesel::pg::Pg>,
        params: &ListParams,
    ) -> posts::BoxedQuery<'a, diesel::pg::Pg> {
        match (params.sort_by.as_deref(), params.sort_dir) {
            (Some("id"), SortDirection::Asc) => query = query.order(posts::id.asc()),
            (Some("id"), SortDirection::Desc) => query = query.order(posts::id.desc()),
            (Some("title"), SortDirection::Asc) => query = query.order(posts::title.asc()),
            (Some("title"), SortDirection::Desc) => query = query.order(posts::title.desc()),
            (Some("slug"), SortDirection::Asc) => query = query.order(posts::slug.asc()),
            (Some("slug"), SortDirection::Desc) => query = query.order(posts::slug.desc()),
            (Some("published"), SortDirection::Asc) => query = query.order(posts::published.asc()),
            (Some("published"), SortDirection::Desc) => {
                query = query.order(posts::published.desc())
            }
            (Some("updated_at"), SortDirection::Asc) => {
                query = query.order(posts::updated_at.asc())
            }
            (Some("updated_at"), SortDirection::Desc) => {
                query = query.order(posts::updated_at.desc())
            }
            (_, SortDirection::Asc) => query = query.order(posts::created_at.asc()),
            _ => query = query.order(posts::created_at.desc()),
        }

        query
    }
}

impl AdminModel for PostAdmin {
    fn slug(&self) -> &'static str {
        "posts"
    }

    fn display_name(&self) -> &'static str {
        "Post"
    }

    fn display_name_plural(&self) -> &'static str {
        "Posts"
    }

    fn fields(&self) -> Vec<AdminField> {
        vec![
            AdminField::new("id", AdminFieldKind::Hidden)
                .readonly()
                .hide_from_list(),
            AdminField::new("title", AdminFieldKind::Text).searchable(),
            AdminField::new("slug", AdminFieldKind::Text).filterable(),
            AdminField::new("body", AdminFieldKind::TextArea)
                .searchable()
                .hide_from_list(),
            AdminField::new("published", AdminFieldKind::Boolean).filterable(),
            AdminField::new("created_at", AdminFieldKind::DateTime)
                .readonly()
                .optional(),
            AdminField::new("updated_at", AdminFieldKind::DateTime)
                .readonly()
                .optional()
                .hide_from_list(),
        ]
    }

    fn list(
        &self,
        pool: &Pool<AsyncPgConnection>,
        params: ListParams,
    ) -> AdminFuture<'_, ListResult> {
        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool.get().await.map_err(Self::pool_error)?;

            let total: i64 = Self::apply_filters(posts::table.into_boxed(), &params)
                .count()
                .get_result(&mut conn)
                .await
                .map_err(Self::pool_error)?;

            let mut query = Self::apply_sort(
                Self::apply_filters(posts::table.into_boxed(), &params),
                &params,
            );
            if params.per_page > 0 {
                let offset = params
                    .page
                    .saturating_sub(1)
                    .saturating_mul(params.per_page);
                query = query.limit(params.per_page as i64).offset(offset as i64);
            }

            let records = query
                .select(Post::as_select())
                .load::<Post>(&mut conn)
                .await
                .map_err(Self::pool_error)?
                .into_iter()
                .map(Self::serialize_post)
                .collect::<Result<Vec<_>, _>>()?;

            Ok(ListResult {
                records,
                total: total as u64,
                page: params.page,
                per_page: params.per_page,
            })
        })
    }

    fn get(&self, pool: &Pool<AsyncPgConnection>, id: i64) -> AdminFuture<'_, Option<Value>> {
        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool.get().await.map_err(Self::pool_error)?;
            let post = posts::table
                .find(id)
                .select(Post::as_select())
                .first::<Post>(&mut conn)
                .await
                .optional()
                .map_err(Self::pool_error)?;
            post.map(Self::serialize_post).transpose()
        })
    }

    fn create(&self, pool: &Pool<AsyncPgConnection>, data: Value) -> AdminFuture<'_, Value> {
        let pool = pool.clone();
        Box::pin(async move {
            let new_post: NewPost = serde_json::from_value(data).map_err(Self::validation_error)?;
            let new_post = new_post.validated().map_err(Self::validation_error)?;
            let mut conn = pool.get().await.map_err(Self::pool_error)?;

            let created = diesel::insert_into(posts::table)
                .values(&new_post)
                .returning(Post::as_returning())
                .get_result::<Post>(&mut conn)
                .await
                .map_err(Self::pool_error)?;

            Self::serialize_post(created)
        })
    }

    fn update(
        &self,
        pool: &Pool<AsyncPgConnection>,
        id: i64,
        data: Value,
    ) -> AdminFuture<'_, Value> {
        let pool = pool.clone();
        Box::pin(async move {
            let new_post: NewPost = serde_json::from_value(data).map_err(Self::validation_error)?;
            let new_post = new_post.validated().map_err(Self::validation_error)?;
            let changes = UpdatePost {
                title: Some(new_post.title),
                slug: Some(new_post.slug),
                body: Some(new_post.body),
                published: Some(new_post.published),
                // Bump the version token so the cached post card re-renders on
                // the next request (Postgres has no ON UPDATE trigger).
                updated_at: Some(chrono::Utc::now().naive_utc()),
            };
            let mut conn = pool.get().await.map_err(Self::pool_error)?;

            let updated = diesel::update(posts::table.find(id))
                .set(&changes)
                .returning(Post::as_returning())
                .get_result::<Post>(&mut conn)
                .await
                .optional()
                .map_err(Self::pool_error)?;

            updated
                .ok_or(AdminError::NotFound)
                .and_then(Self::serialize_post)
        })
    }

    fn delete(&self, pool: &Pool<AsyncPgConnection>, id: i64) -> AdminFuture<'_, ()> {
        let pool = pool.clone();
        Box::pin(async move {
            let mut conn = pool.get().await.map_err(Self::pool_error)?;
            let deleted = diesel::delete(posts::table.find(id))
                .execute(&mut conn)
                .await
                .map_err(Self::pool_error)?;
            if deleted == 0 {
                return Err(AdminError::NotFound);
            }
            Ok(())
        })
    }

    // ── Version history (issue #700) ─────────────────────────────────

    // Posts opt into the History pane in the admin panel.
    // In an application that uses `#[repository(Post, versioned = true)]`,
    // this returns `true` automatically (the macro generates the override).
    // This example wires it manually to demonstrate the History pane UI.
    // ── CSV export / import ──────────────────────────────────────────

    fn supports_csv_export(&self) -> bool {
        true
    }

    /// Export `id`, `title`, `slug`, `published`, and `created_at`.
    /// The `body` column is omitted by default to keep exports manageable.
    fn csv_export_columns(&self) -> Vec<&'static str> {
        vec![
            "id",
            "title",
            "slug",
            "published",
            "created_at",
            "updated_at",
        ]
    }

    /// Enable CSV import for the blog Posts model.
    fn supports_csv_import(&self) -> bool {
        true
    }

    /// Process a single CSV row: create a new post from the uploaded data.
    fn import_csv_row<'a>(
        &'a self,
        pool: &'a Pool<AsyncPgConnection>,
        line: u64,
        row: std::collections::HashMap<String, String>,
        mode: CsvImportMode,
    ) -> AdminFuture<'a, AdminImportRowResult> {
        let pool = pool.clone();
        Box::pin(async move {
            let title = row.get("title").cloned().unwrap_or_default();
            let slug = row.get("slug").cloned().unwrap_or_default();
            let body = row.get("body").cloned().unwrap_or_default();

            if title.trim().is_empty() {
                return Ok(AdminImportRowResult::FieldError {
                    column: "title".to_owned(),
                    message: format!("line {line}: title must not be empty"),
                });
            }

            let published = row
                .get("published")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false);

            let slug = if slug.is_empty() {
                autumn_web::slugify(&title)
            } else {
                slug
            };
            let new_post = crate::models::NewPost {
                title,
                slug,
                body,
                published,
            };
            let new_post = match new_post.validated() {
                Ok(p) => p,
                Err(e) => {
                    return Ok(AdminImportRowResult::RowError(format!(
                        "line {line}: validation failed: {e}"
                    )));
                }
            };

            // Dry-run: validated above, but don't write.
            if matches!(mode, CsvImportMode::DryRun) {
                return Ok(AdminImportRowResult::Inserted);
            }

            let conn_result = pool.get().await;
            let mut conn = match conn_result {
                Ok(c) => c,
                Err(e) => {
                    return Ok(AdminImportRowResult::RowError(format!(
                        "DB pool error: {e}"
                    )));
                }
            };

            let insert_result = diesel::insert_into(crate::schema::posts::table)
                .values(&new_post)
                .execute(&mut conn)
                .await;

            match insert_result {
                Ok(_) => Ok(AdminImportRowResult::Inserted),
                Err(e) => Ok(AdminImportRowResult::RowError(format!("Insert error: {e}"))),
            }
        })
    }

    fn has_history(&self) -> bool {
        true
    }

    /// Return paginated version history entries for a post.
    ///
    /// In production this queries `_autumn_version_history` via the
    /// generated `PgPostRepository::version_history(id, filter)` method.
    /// This example returns stub data so the History pane is visible in the
    /// blog's admin UI without requiring a live database.
    fn get_history<'a>(
        &'a self,
        _pool: &'a Pool<AsyncPgConnection>,
        record_id: i64,
        page: u64,
        per_page: u64,
    ) -> AdminFuture<'a, AdminHistoryPage> {
        Box::pin(async move {
            let entries = vec![
                AdminHistoryEntry {
                    id: 1,
                    actor: "admin".to_owned(),
                    op: "insert".to_owned(),
                    request_id: Some("req-example-1".to_owned()),
                    changes: vec![
                        serde_json::json!({"column": "title", "before": null, "after": "Hello World", "sensitive": false}),
                        serde_json::json!({"column": "published", "before": null, "after": false, "sensitive": false}),
                    ],
                    recorded_at: chrono::Utc::now() - chrono::Duration::hours(2),
                },
                AdminHistoryEntry {
                    id: 2,
                    actor: "admin".to_owned(),
                    op: "update".to_owned(),
                    request_id: Some("req-example-2".to_owned()),
                    changes: vec![
                        serde_json::json!({"column": "published", "before": false, "after": true, "sensitive": false}),
                    ],
                    recorded_at: chrono::Utc::now() - chrono::Duration::hours(1),
                },
            ];
            let total = entries.len() as u64;
            let _ = record_id;
            Ok(AdminHistoryPage {
                entries,
                total,
                page,
                per_page,
            })
        })
    }
}

/// Ledger findings harness for `PostAdmin::import_csv_row` (this file), the
/// one shipped implementation of `AdminModel::import_csv_row`
/// (`autumn-admin-plugin/src/traits.rs`) — the trait method
/// `model_import_csv` (`autumn-admin-plugin/src/routes.rs`,
/// `POST /admin/{slug}/import`) calls once per CSV data row.
///
/// Lives here rather than under `examples/blog/tests/` because `blog` is a
/// `--bin`-only crate (no `src/lib.rs`, see `Cargo.toml`): `admin::PostAdmin`
/// isn't `pub` outside the crate, so an external `tests/*.rs` file can't
/// reach it. A `--bin` unit test is the same home CLAUDE.md already
/// validates for a crate-private Docker test (`autumn/src/job.rs`'s Redis
/// job-admin suite). Not wired into `.github/workflows/ci.yml`'s
/// Docker-dependent sweep: no line there runs `-p blog` at all today (its
/// own pre-existing `create_post_round_trip` Docker test in
/// `tests/integration_test.rs` isn't swept either), so this doesn't
/// introduce a new CI gap — run manually with:
///
/// ```text
/// cargo test -p blog --bin blog -- --ignored ledger_import_csv --nocapture --test-threads=1
/// ```
///
/// **Findings, not a fix.** `import_csv_row` (this file, `import_csv_row`
/// above) does one `pool.get()` + one single-row
/// `INSERT INTO posts ... VALUES (...)` per CSV row, sequentially awaited —
/// `model_import_csv` loops `rdr.records()` and calls it once per row with
/// no batching. Measured against a 20,000-row `posts` background at three
/// tiers (a manual content fix, a page's worth of migrated posts, a
/// full-site migration): 50 rows -> 50 INSERT calls / 610 buffers, 500 rows
/// -> 500 calls / 5,952 buffers, 5,000 rows -> 5,000 calls / 52,329 buffers
/// (`pg_stat_statements`, ~10.5 buffers/call at every tier — a plain
/// point-INSERT plan, no scan/estimate defect; the workload is the
/// round-trip count, not the per-call plan, same shape as PR #2532).
///
/// Collapsing that into one multi-row `INSERT ... SELECT * FROM UNNEST(...)`
/// can't be scoped to `PostAdmin` alone the way an ordinary call-site
/// rewrite would be, because `import_csv_row`'s per-row return type
/// (`AdminImportRowResult`) is the *only* channel `model_import_csv` has for
/// per-row success/failure attribution back to the operator (which CSV line
/// failed, and why) — the same per-caller-observable-contract shape the
/// Ledger process already routed to a human for `WebhookOutboundManager::
/// dispatch` -> `JobClient::enqueue` (PR #2532), `WebPush::send_many`
/// (PR #2446), and `repository_commit_hooks` (PR #2300). Concretely, this
/// harness's own edge-case run below shows the DB-level half of that
/// contract can't be reconstructed from `pg_stat_statements`: a row that
/// fails on the `slug` UNIQUE constraint really does reach the INSERT (this
/// harness's own `RowError` assertion proves it), but Postgres only updates
/// a statement's `calls`/`buffers` on successful `ExecutorEnd` — an aborted,
/// constraint-violating execution is invisible to `pg_stat_statements`
/// entirely, evidence or not. A naive `INSERT ... VALUES (...), (...), ...`
/// batch aborts the WHOLE batch on the first conflict (losing "only that
/// row fails" for every row after it); `... ON CONFLICT (slug) DO NOTHING`
/// fixes the abort but converts every conflict into a silent `Skipped`
/// instead of a `RowError` naming the row, unless the caller also runs a
/// pre-check `SELECT slug FROM posts WHERE slug = ANY($1)` to attribute
/// failures back to specific input rows before inserting — an extra round
/// trip and a slug-in-the-same-batch-twice edge case (`ON CONFLICT DO
/// NOTHING` is fine with intra-batch duplicates; distinguishing "duplicate
/// within this CSV" from "already existed" for the operator's error message
/// is not free). That is a trait-interface decision affecting every future
/// `AdminModel::import_csv_row` implementor, not a `PostAdmin`-only rewrite.
///
/// **Recommendation for whoever picks this up:** add
/// `AdminModel::import_csv_rows(&self, pool, mode, rows: Vec<(u64,
/// HashMap<String, String>)>) -> AdminFuture<Vec<AdminImportRowResult>>`
/// with a default that calls today's per-row `import_csv_row` in a loop
/// (zero behavior change for every existing/future implementor that doesn't
/// override it), have `model_import_csv` call the batch method instead of
/// looping itself, and let `PostAdmin` override it: run per-row Rust-side
/// validation unchanged (collecting `FieldError`s with zero DB cost, exactly
/// like today), then one `INSERT ... SELECT * FROM UNNEST(...) ON CONFLICT
/// (slug) DO NOTHING RETURNING slug` for the validated rows, then attribute
/// `Inserted`/`RowError("slug already exists")` per row from the returned
/// slug set (deduping intra-batch repeats before the query, not after, so
/// "duplicate in this file" gets a different message than "already in the
/// table"). That is the smallest version of this that doesn't quietly
/// change today's per-row error semantics.
#[cfg(test)]
mod ledger_import_csv_batch_profile {
    use std::collections::HashMap;

    use autumn_admin_plugin::{AdminImportRowResult, AdminModel, CsvImportMode};
    use diesel::connection::SimpleConnection;
    use diesel::sql_types::{BigInt, Text};
    use diesel::{Connection, PgConnection, QueryableByName};
    use diesel_async::pooled_connection::AsyncDieselConnectionManager;
    use diesel_async::pooled_connection::deadpool::Pool;
    use testcontainers::ImageExt;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    use super::PostAdmin;

    const MIGRATION_SQL: &str = include_str!("../migrations/00000000000000_create_posts/up.sql");

    /// A 20,000-row `posts` background — a blog with real publishing
    /// history, not an empty table: 70% published, titles/bodies of varying
    /// length (short news items through long-form essays), real dead tuples
    /// from a follow-up `UPDATE` before `ANALYZE`, no `VACUUM`. `posts` has
    /// no nullable columns (see `up.sql`), so there is no NULL-density axis
    /// to vary here.
    const BACKGROUND_ROWS: i64 = 20_000;

    fn seed_background(conn: &mut PgConnection) {
        conn.batch_execute(&format!(
            "INSERT INTO posts (title, slug, body, published, created_at, updated_at) \
             SELECT \
               'Post #' || gs || ': ' || (CASE WHEN gs % 7 = 0 THEN 'a longer essay-style title \
                 that runs on for a while, the way a real editorial piece would' \
                 ELSE 'a short update' END), \
               'post-' || gs, \
               repeat('Lorem ipsum dolor sit amet. ', 5 + (gs % 40)), \
               (gs % 10) < 7, \
               TIMESTAMP '2024-01-01 00:00:00' + (gs || ' minutes')::interval, \
               TIMESTAMP '2024-01-01 00:00:00' + (gs || ' minutes')::interval \
             FROM generate_series(1, {BACKGROUND_ROWS}) AS gs"
        ))
        .expect("seed posts background");

        conn.batch_execute("UPDATE posts SET updated_at = NOW() WHERE id % 9 = 0")
            .expect("create dead tuples");
        conn.batch_execute("ANALYZE posts").expect("analyze");
    }

    fn reset_stats(conn: &mut PgConnection) {
        conn.batch_execute("SELECT pg_stat_statements_reset()")
            .expect("reset pg_stat_statements");
    }

    #[derive(QueryableByName, Debug)]
    struct StatementRow {
        #[diesel(sql_type = Text)]
        query: String,
        #[diesel(sql_type = BigInt)]
        calls: i64,
        #[diesel(sql_type = BigInt)]
        buffers: i64,
    }

    /// Prints every `posts`-touching statement from this run and returns
    /// `(calls, buffers)` for the single-row `INSERT ... VALUES` shape
    /// `import_csv_row` issues, isolated from this harness's own bulk
    /// `INSERT ... SELECT FROM generate_series` seed statement (structurally
    /// distinct SQL text, so pg_stat_statements never conflates the two).
    fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64) {
        use diesel::RunQueryDsl;
        println!("\n=== pg_stat_statements: {label} ===");
        let rows = diesel::sql_query(
            "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
             FROM pg_stat_statements \
             WHERE query ILIKE '%posts%' AND query NOT ILIKE '%pg_stat_statements%' \
             ORDER BY calls DESC",
        )
        .load::<StatementRow>(conn)
        .expect("query pg_stat_statements");

        let (mut insert_calls, mut insert_buffers) = (0i64, 0i64);
        for row in &rows {
            let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
            println!(
                "calls={:<6} buffers={:<8} {normalized}",
                row.calls, row.buffers
            );
            if normalized.contains("INSERT INTO") && normalized.contains("VALUES") {
                insert_calls += row.calls;
                insert_buffers += row.buffers;
            }
        }
        println!("-- per-row INSERT: calls={insert_calls} buffers={insert_buffers} --");
        (insert_calls, insert_buffers)
    }

    #[derive(QueryableByName, Debug)]
    struct ExplainLine {
        #[diesel(sql_type = Text, column_name = "QUERY PLAN")]
        line: String,
    }

    fn explain(conn: &mut PgConnection, label: &str, sql: &str) {
        use diesel::RunQueryDsl;
        println!("\n=== EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS): {label} ===");
        println!("{sql}");
        let lines = diesel::sql_query(format!(
            "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) {sql}"
        ))
        .load::<ExplainLine>(conn)
        .expect("explain");
        for line in lines {
            println!("{}", line.line);
        }
    }

    /// One CSV data row, in the exact `(line, column -> value)` shape
    /// `model_import_csv` (`autumn-admin-plugin/src/routes.rs`) builds from
    /// a parsed record before calling `import_csv_row` — this harness skips
    /// only the multipart/CSV decoding step, same as the bulk-delete
    /// harnesses skip only the form decoding for `execute_action`.
    fn csv_row(line: u64, slug: &str) -> (u64, HashMap<String, String>) {
        let mut row = HashMap::new();
        row.insert("title".to_owned(), format!("Migrated post {slug}"));
        row.insert("slug".to_owned(), slug.to_owned());
        row.insert(
            "body".to_owned(),
            format!("Body content imported for {slug} from the legacy CMS export."),
        );
        row.insert(
            "published".to_owned(),
            if line.is_multiple_of(3) {
                "true"
            } else {
                "false"
            }
            .to_owned(),
        );
        (line, row)
    }

    async fn run_tier(
        pool: &Pool<diesel_async::AsyncPgConnection>,
        conn: &mut PgConnection,
        label: &str,
        tier: &str,
        count: u64,
    ) -> (i64, i64) {
        let rows: Vec<_> = (1..=count)
            .map(|n| csv_row(n, &format!("{tier}-{n}")))
            .collect();

        reset_stats(conn);
        let mut inserted = 0u64;
        for (line, row) in rows {
            let outcome = PostAdmin
                .import_csv_row(pool, line, row, CsvImportMode::Insert)
                .await
                .expect("import_csv_row must not error for a valid row");
            assert!(
                matches!(outcome, AdminImportRowResult::Inserted),
                "row {line} in tier {label}: expected Inserted, got {outcome:?}"
            );
            inserted += 1;
        }
        assert_eq!(inserted, count, "every row in tier {label} must insert");

        let (calls, buffers) = print_profile(conn, &format!("{label} ({count} rows)"));
        assert_eq!(
            calls, count as i64,
            "tier {label}: import_csv_row's per-row INSERT must fire exactly \
             once per CSV row submitted -- {count} rows in, {count} statements \
             expected, not batched"
        );
        (calls, buffers)
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn ledger_import_csv_row_batch_profile() {
        let container = Postgres::default()
            .with_tag("16-alpine")
            .with_cmd([
                "-c",
                "fsync=off",
                "-c",
                "shared_preload_libraries=pg_stat_statements",
                "-c",
                "pg_stat_statements.track=all",
                "-c",
                "pg_stat_statements.max=2000",
            ])
            .start()
            .await
            .expect("failed to start postgres container");
        let host = container.get_host().await.expect("host");
        let port = container.get_host_port_ipv4(5432).await.expect("port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

        let mut conn = PgConnection::establish(&url).expect("sync db connection");
        conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
            .expect("create pg_stat_statements extension");
        conn.batch_execute(MIGRATION_SQL).expect("create posts");

        seed_background(&mut conn);

        let manager = AsyncDieselConnectionManager::<diesel_async::AsyncPgConnection>::new(&url);
        let pool = Pool::builder(manager).max_size(5).build().expect("pool");

        // Three tiers spanning two orders of magnitude: a manual content
        // fix, a page's worth of migrated posts, and a full-site migration
        // from another CMS.
        let small = run_tier(&pool, &mut conn, "small", "small", 50).await;
        let mid = run_tier(&pool, &mut conn, "mid", "mid", 500).await;
        let large = run_tier(&pool, &mut conn, "large", "large", 5_000).await;

        println!(
            "\n-- statement-count claim --\n\
             small:  50 rows -> calls={} buffers={}\n\
             mid:   500 rows -> calls={} buffers={}\n\
             large: 5,000 rows -> calls={} buffers={}\n\
             calls scale 1:1 with rows submitted at every tier -- the workload \
             is the round-trip count, not the per-call plan.",
            small.0, small.1, mid.0, mid.1, large.0, large.1
        );

        // A CSV import's per-row outcome contract that any batched
        // replacement has to preserve: a validation failure never reaches
        // the DB (rejected in Rust, before `pool.get()`), and a DB-level
        // conflict (the `slug` UNIQUE constraint) fails only THAT row, with
        // its own message -- the rest of the batch still commits. Run
        // outside the three measured tiers above so it doesn't perturb the
        // calls/buffers scaling claim.
        reset_stats(&mut conn);
        let empty_title = PostAdmin
            .import_csv_row(
                &pool,
                90_001,
                csv_row(1, "unused").1.tap_title_blank(),
                CsvImportMode::Insert,
            )
            .await
            .expect("field-invalid row returns Ok(FieldError), not Err");
        assert!(
            matches!(empty_title, AdminImportRowResult::FieldError { ref column, .. } if column == "title"),
            "empty title must be rejected in Rust, before any DB round trip: {empty_title:?}"
        );

        let colliding_slug = PostAdmin
            .import_csv_row(&pool, 90_002, csv_row(1, "post-1").1, CsvImportMode::Insert)
            .await
            .expect("unique-violation row returns Ok(RowError), not Err");
        assert!(
            matches!(colliding_slug, AdminImportRowResult::RowError(_)),
            "a slug already present in the background fixture must fail as \
             this one row, not abort the request: {colliding_slug:?}"
        );
        let (edge_calls, _) = print_profile(&mut conn, "edge cases (field error + slug conflict)");
        assert_eq!(
            edge_calls, 0,
            "the field-invalid row costs zero DB round trips (rejected before \
             pool.get()); the slug-conflict row DOES reach the INSERT (proven by \
             the RowError assertion above, which only a real unique_violation \
             produces), but pg_stat_statements only counts a statement's `calls` \
             on successful ExecutorEnd -- an aborted, constraint-violating \
             execution is invisible to it. That is itself part of the finding: \
             a batched multi-row INSERT can't reuse `pg_stat_statements.calls` to \
             detect which row(s) in the batch conflicted -- see the write-up in \
             this module's doc comment."
        );

        explain(
            &mut conn,
            "single-row INSERT (the per-CSV-row statement shape)",
            "INSERT INTO posts (title, slug, body, published) \
             VALUES ('Explain post', 'explain-post-example', 'body', false)",
        );
    }

    /// Small extension trait so `run_tier`'s `csv_row` fixture can be reused
    /// for the "empty title" edge case above without a second builder.
    trait TapTitleBlank {
        fn tap_title_blank(self) -> Self;
    }

    impl TapTitleBlank for HashMap<String, String> {
        fn tap_title_blank(mut self) -> Self {
            self.insert("title".to_owned(), String::new());
            self
        }
    }
}
