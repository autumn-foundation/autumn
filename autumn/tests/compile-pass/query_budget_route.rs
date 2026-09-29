//! `#[query_budget(N)]` composed with the real route macro and the real
//! `#[model]` / `#[repository]` / `preload` surface (#1667).
//!
//! The sibling `query_budget_valid.rs` fixture exercises every analysis shape
//! against local stand-in types; this one proves the attribute stacks on an
//! actual handler, in both orders, and counts the framework's own generated
//! finders and batched preloads.

mod schema {
    autumn_web::reexports::diesel::table! {
        qb_authors (id) {
            id -> Int8,
            name -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        qb_posts (id) {
            id -> Int8,
            title -> Text,
            author_id -> Int8,
        }
    }
}

use autumn_web::prelude::*;
use autumn_web::reexports::scoped_futures::ScopedFutureExt as _;
use autumn_web::reexports::diesel::prelude::*;
use autumn_web::reexports::diesel_async::RunQueryDsl;
use schema::{qb_authors, qb_posts};

#[autumn_web::model]
pub struct QbAuthor {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::model]
#[belongs_to(QbAuthor, fk = author_id)]
pub struct QbPost {
    #[id]
    pub id: i64,
    pub title: String,
    pub author_id: i64,
}

#[autumn_web::repository(QbPost)]
pub trait QbPostRepository {}

/// The AC's green build, on the real surface: one finder plus one batched
/// association load, and a loop over rows that issues nothing.
#[get("/qb-posts")]
#[query_budget(2)]
async fn index(repo: PgQbPostRepository) -> AutumnResult<String> {
    let posts = repo.find_all().await?;
    let posts = repo
        .preload(posts, QbPost::preload().author())
        .await?;

    let mut titles = String::new();
    for post in &posts {
        titles.push_str(&post.title);
    }
    Ok(titles)
}

/// The same gate with the attributes in the other order — `#[query_budget]`
/// reads the handler and emits it unchanged, so neither order changes the
/// analysis.
#[query_budget(1)]
#[get("/qb-posts/count")]
async fn count(repo: PgQbPostRepository) -> AutumnResult<String> {
    Ok(repo.count().await?.to_string())
}

/// A raw diesel executor handed the `Db` extractor is the third query-issuing
/// shape the framework owns, and it is counted too.
#[get("/qb-posts/raw")]
#[query_budget(1)]
async fn raw(mut db: Db) -> AutumnResult<String> {
    let posts: Vec<QbPost> = qb_posts::table
        .select(QbPost::as_select())
        .load(&mut *db)
        .await?;
    Ok(posts.len().to_string())
}

/// Stacked with the auth and rate guards, which rewrite the body into an
/// `async` block. The analysis walks through that, so the count is the same as
/// it would be without them — pinned here because nothing else would notice if
/// a guard's rewrite started hiding queries.
#[get("/qb-posts/secure")]
#[secured]
#[query_budget(1)]
async fn secure(repo: PgQbPostRepository) -> AutumnResult<String> {
    Ok(repo.count().await?.to_string())
}

#[get("/qb-posts/limited")]
#[throttle(limit = 5, per = "1m", key = "ip")]
#[query_budget(1)]
async fn limited(repo: PgQbPostRepository) -> AutumnResult<String> {
    Ok(repo.count().await?.to_string())
}

/// autumn's real transaction API: the callback runs once, and the `conn` it
/// hands over is tracked, so the two writes inside are counted — not reported
/// as a per-element closure.
#[post("/qb-posts")]
#[query_budget(3)]
async fn create(mut db: Db) -> AutumnResult<String> {
    let title: String = db
        .tx(move |conn| {
            async move {
                let created: QbPost = diesel::insert_into(qb_posts::table)
                    .values((
                        qb_posts::title.eq("hello"),
                        qb_posts::author_id.eq(1_i64),
                    ))
                    .returning(QbPost::as_returning())
                    .get_result(conn)
                    .await?;
                diesel::update(qb_authors::table.find(created.author_id))
                    .set(qb_authors::name.eq("seen"))
                    .execute(conn)
                    .await?;
                Ok::<_, autumn_web::AutumnError>(created.title)
            }
            .scope_boxed()
        })
        .await?;
    Ok(title)
}

fn main() {
    assert_eq!(__AUTUMN_QUERY_BUDGET_index.proven_max, Some(2));
    assert_eq!(__AUTUMN_QUERY_BUDGET_secure.proven_max, Some(1));
    assert_eq!(__AUTUMN_QUERY_BUDGET_limited.proven_max, Some(1));
    assert_eq!(__AUTUMN_QUERY_BUDGET_create.proven_max, Some(3));
    assert_eq!(__AUTUMN_QUERY_BUDGET_count.proven_max, Some(1));
    assert_eq!(__AUTUMN_QUERY_BUDGET_raw.proven_max, Some(1));
}
