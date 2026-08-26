//! The N+1 red build against the **real** `#[model]` / `#[repository]` surface
//! — the generated `find_all` finder and a generated derived finder, with the
//! per-row lookup inside the loop over rows (#1667).
//!
//! The sibling `query_budget_n_plus_one.rs` pins the same diagnostic against
//! hand-written stand-in types, which keeps that fixture feature-independent.
//! This one proves the gate fires on code the framework itself generated.

mod schema {
    autumn_web::reexports::diesel::table! {
        qbf_authors (id) {
            id -> Int8,
            name -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        qbf_posts (id) {
            id -> Int8,
            title -> Text,
            author_id -> Int8,
        }
    }
}

use autumn_web::prelude::*;
use schema::{qbf_authors, qbf_posts};

#[autumn_web::model]
pub struct QbfAuthor {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::model]
#[belongs_to(QbfAuthor, fk = author_id)]
pub struct QbfPost {
    #[id]
    pub id: i64,
    pub title: String,
    pub author_id: i64,
}

#[autumn_web::repository(QbfPost)]
pub trait QbfPostRepository {}

#[autumn_web::repository(QbfAuthor)]
pub trait QbfAuthorRepository {}

#[get("/qbf-posts")]
#[query_budget(2)]
async fn index(
    repo: PgQbfPostRepository,
    authors: PgQbfAuthorRepository,
) -> AutumnResult<String> {
    let posts = repo.find_all().await?;
    let mut names = String::new();
    for post in &posts {
        // One query per row. `preload(posts, QbfPost::preload().author())`
        // batches the same data into a single `WHERE ... IN (...)`.
        if let Some(author) = authors.find_by_id(post.author_id).await? {
            names.push_str(&author.name);
        }
    }
    Ok(names)
}

fn main() {}
