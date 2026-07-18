use autumn_web::error::{AutumnError, AutumnResult};
use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use serde::{Deserialize, Serialize};

use crate::schema::posts;

/// A blog post loaded from the database.
#[derive(Queryable, Selectable, Serialize)]
#[diesel(table_name = posts)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Post {
    pub id: i64,
    pub title: String,
    pub slug: String,
    pub body: String,
    pub published: bool,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

impl Post {
    /// Load all posts ordered by creation date (newest first).
    pub async fn all(db: &mut AsyncPgConnection) -> AutumnResult<Vec<Self>> {
        Ok(posts::table
            .order(posts::created_at.desc())
            .select(Self::as_select())
            .load(db)
            .await?)
    }

    /// Load only published posts ordered by creation date (newest first).
    pub async fn published(db: &mut AsyncPgConnection) -> AutumnResult<Vec<Self>> {
        Ok(posts::table
            .filter(posts::published.eq(true))
            .order(posts::created_at.desc())
            .select(Self::as_select())
            .load(db)
            .await?)
    }

    /// Find a single post by ID, returning 404 if not found.
    pub async fn find(id: i64, db: &mut AsyncPgConnection) -> AutumnResult<Self> {
        posts::table
            .find(id)
            .select(Self::as_select())
            .first(db)
            .await
            .map_err(AutumnError::not_found)
    }

    /// Find a published post by slug, returning 404 if not found.
    pub async fn find_by_slug(slug: &str, db: &mut AsyncPgConnection) -> AutumnResult<Self> {
        posts::table
            .filter(posts::slug.eq(slug))
            .filter(posts::published.eq(true))
            .select(Self::as_select())
            .first(db)
            .await
            .map_err(AutumnError::not_found)
    }
}

/// Data needed to insert a new post.
#[derive(Insertable, Deserialize)]
#[diesel(table_name = posts)]
pub struct NewPost {
    pub title: String,
    pub slug: String,
    pub body: String,
    /// Defaults to `false` when the checkbox is unchecked (browser
    /// omits unchecked checkboxes from form data entirely).
    #[serde(default)]
    pub published: bool,
}

impl NewPost {
    /// Validate the post data. Returns 422 if title or body is empty.
    pub fn validated(self) -> AutumnResult<Self> {
        let title = self.title.trim().to_owned();
        let body = self.body.trim().to_owned();
        let slug = self.slug.trim().to_owned();

        if title.is_empty() {
            return Err(AutumnError::unprocessable_msg("Title must not be empty"));
        }
        if body.is_empty() {
            return Err(AutumnError::unprocessable_msg("Body must not be empty"));
        }

        // Auto-generate slug from title if not provided
        let slug = if slug.is_empty() {
            slugify(&title)
        } else {
            slugify(&slug)
        };

        Ok(Self {
            title,
            slug,
            body,
            published: self.published,
        })
    }
}

/// Data for updating an existing post.
#[derive(AsChangeset, Deserialize)]
#[diesel(table_name = posts)]
pub struct UpdatePost {
    pub title: Option<String>,
    pub slug: Option<String>,
    pub body: Option<String>,
    /// HTML checkboxes: absent when unchecked → `None` via `#[serde(default)]`.
    /// The handler converts `None` → `Some(false)` before saving so
    /// unchecking the checkbox actually unpublishes the post.
    #[serde(default)]
    pub published: Option<bool>,
    /// Bumped to the current time on every save. Postgres has no `ON UPDATE`
    /// trigger, so without this the `updated_at` column would keep its
    /// insert-time value — which would freeze the `post_card` fragment-cache
    /// key (see `routes::posts::post_card`) and serve a stale card forever.
    /// Never deserialized from form/JSON input; the handler always sets it.
    #[serde(skip)]
    pub updated_at: Option<chrono::NaiveDateTime>,
}

/// Convert a string into a URL-safe slug.
///
/// ⚡ Bolt Optimization:
/// This avoids multiple heap allocations (an intermediate `String` and `Vec`)
/// by iterating through characters in a single pass and pushing to a pre-allocated String.
pub fn slugify(s: &str) -> String {
    let mut slug = String::with_capacity(s.len());
    let mut last_was_dash = true; // Start true to prevent leading dashes

    for c in s.chars() {
        if c.is_alphanumeric() {
            // Using flat_map to handle potential multiple chars from lowercase conversion
            for lc in c.to_lowercase() {
                slug.push(lc);
            }
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    if slug.ends_with('-') {
        slug.pop();
    }

    slug
}
