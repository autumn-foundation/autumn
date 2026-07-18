//! Tenant-facing bookmark CRUD.
//!
//! `ShardedDb` resolves the tenant id from the `X-Tenant-Id` header (per
//! `[tenancy]` in autumn.toml), hashes it onto a logical slot, and checks
//! out a connection to the owning shard's primary. The handlers never
//! mention shards — routing is the framework's job.

use autumn_web::prelude::*;
use autumn_web::reexports::diesel::prelude::*;
use autumn_web::reexports::diesel_async::RunQueryDsl;

use crate::models::{Bookmark, CreateBookmark, NewBookmark};
use crate::schema::bookmarks;

#[derive(serde::Serialize)]
pub struct BookmarkList {
    /// Which shard served the request — surfaced so the README's curl
    /// walkthrough can show the same tenant always landing on one shard.
    pub shard: String,
    pub bookmarks: Vec<Bookmark>,
}

#[get("/api/bookmarks")]
pub async fn list(tenant: Tenant, mut db: ShardedDb) -> AutumnResult<Json<BookmarkList>> {
    // Several tenants share a shard, so queries still filter by tenant.
    let rows = bookmarks::table
        .filter(bookmarks::tenant_id.eq(&tenant.0))
        .order(bookmarks::created_at.desc())
        .load::<Bookmark>(&mut *db)
        .await
        .map_err(AutumnError::from)?;

    Ok(Json(BookmarkList {
        shard: db.shard().to_owned(),
        bookmarks: rows,
    }))
}

#[post("/api/bookmarks")]
pub async fn create(
    tenant: Tenant,
    mut db: ShardedDb,
    Valid(payload): Valid<Json<CreateBookmark>>,
) -> AutumnResult<Json<Bookmark>> {
    let new = NewBookmark {
        tenant_id: tenant.0,
        url: payload.0.url,
        title: payload.0.title,
        tag: payload.0.tag,
    };
    let row = autumn_web::reexports::diesel::insert_into(bookmarks::table)
        .values(new)
        .get_result::<Bookmark>(&mut *db)
        .await
        .map_err(AutumnError::from)?;

    Ok(Json(row))
}
