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
                crate::models::slugify(&title)
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
