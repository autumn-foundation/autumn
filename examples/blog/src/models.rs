use autumn_web::error::{AutumnError, AutumnResult};
use autumn_web::slugify;
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
#[derive(Insertable, Deserialize, Default)]
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
    ///
    /// Used by the JSON API (`routes::api::create`) and the admin-plugin
    /// backend (`admin.rs`), both of which already have their own
    /// error-reporting conventions for a rejected submission (a JSON problem
    /// response and the admin plugin's generic form-redisplay respectively) —
    /// see [`Self::validate_fields`] for the HTML admin routes' own path.
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

    /// Same rule as [`Self::validated`] (title/body must have at least one
    /// non-whitespace character), but returns every violation as a
    /// `(field, message)` pair instead of stopping at the first one and
    /// failing the whole request. Used by `routes::posts::create`/`update` so
    /// a rejected submission can be redisplayed with each message next to its
    /// field and the author's draft intact, instead of losing the page to a
    /// generic error response.
    pub fn validate_fields(&self) -> Vec<(&'static str, &'static str)> {
        let mut errors = Vec::new();
        if self.title.trim().is_empty() {
            errors.push(("title", "Title must not be empty"));
        }
        if self.body.trim().is_empty() {
            errors.push(("body", "Body must not be empty"));
        }
        errors
    }

    /// Trim title/body and auto-generate the slug from the title when the
    /// author left it blank. Call only after [`Self::validate_fields`]
    /// reports no errors.
    pub fn normalized(self) -> Self {
        let title = self.title.trim().to_owned();
        let body = self.body.trim().to_owned();
        let slug = self.slug.trim();
        let slug = if slug.is_empty() {
            slugify(&title)
        } else {
            slugify(slug)
        };

        Self {
            title,
            slug,
            body,
            published: self.published,
        }
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
