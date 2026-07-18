use crate::schema::bookmarks;
use autumn_web::reexports::diesel::prelude::*;
use serde::{Deserialize, Serialize};
use validator::Validate;

#[derive(Debug, Clone, Queryable, Selectable, Serialize)]
#[diesel(table_name = bookmarks)]
pub struct Bookmark {
    pub id: i64,
    pub tenant_id: String,
    pub url: String,
    pub title: String,
    pub tag: String,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = bookmarks)]
pub struct NewBookmark {
    pub tenant_id: String,
    pub url: String,
    pub title: String,
    pub tag: String,
}

#[derive(Debug, Deserialize, Validate)]
pub struct CreateBookmark {
    #[validate(url)]
    pub url: String,
    #[validate(length(min = 1, max = 200))]
    pub title: String,
    #[serde(default)]
    pub tag: String,
}
