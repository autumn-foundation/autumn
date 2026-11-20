use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use serde::{Deserialize, Serialize};

use autumn_web::db::SqlTimestamp;
use autumn_web::error::{AutumnError, AutumnResult};
use autumn_web::time::Timestamp;

use crate::schema::{api_tokens, posts};

// ── Post ─────────────────────────────────────────────────────────────────────

#[derive(Queryable, Selectable, Serialize, Clone)]
#[diesel(table_name = posts)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Post {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub published: bool,
    pub author: String,
    #[diesel(deserialize_as = SqlTimestamp)]
    #[serde(with = "autumn_web::time::compat::serde_autosi")]
    pub created_at: Timestamp,
    #[diesel(deserialize_as = SqlTimestamp)]
    #[serde(with = "autumn_web::time::compat::serde_autosi")]
    pub updated_at: Timestamp,
}

impl Post {
    pub async fn all(db: &mut AsyncPgConnection) -> AutumnResult<Vec<Self>> {
        Ok(posts::table
            .order(posts::created_at.desc())
            .limit(50)
            .select(Self::as_select())
            .load(db)
            .await?)
    }

    pub async fn find(id: i64, db: &mut AsyncPgConnection) -> AutumnResult<Self> {
        posts::table
            .find(id)
            .select(Self::as_select())
            .first(db)
            .await
            .map_err(AutumnError::not_found)
    }
}

// ── NewPost ───────────────────────────────────────────────────────────────────

#[derive(Insertable, Deserialize, Clone)]
#[diesel(table_name = posts)]
pub struct NewPost {
    pub title: String,
    pub body: String,
    pub published: bool,
    pub author: String,
}

// ── PostUpdate ────────────────────────────────────────────────────────────────

#[derive(AsChangeset, Deserialize, Clone)]
#[diesel(table_name = posts)]
pub struct PostUpdate {
    pub title: Option<String>,
    pub body: Option<String>,
    pub published: Option<bool>,
    pub author: Option<String>,
}

// ── ApiToken ──────────────────────────────────────────────────────────────────

pub struct ApiToken;

impl ApiToken {
    pub async fn verify(raw: &str, db: &mut AsyncPgConnection) -> AutumnResult<Option<String>> {
        let principal = api_tokens::table
            .filter(api_tokens::token.eq(raw))
            .select(api_tokens::principal)
            .first(db)
            .await
            .optional()?;
        Ok(principal)
    }
}
