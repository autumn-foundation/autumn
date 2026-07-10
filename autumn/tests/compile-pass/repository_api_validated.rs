//! Compile-pass: `#[repository(api = ...)]` on a model whose generated
//! `NewModel` derives `validator::Validate` (a `#[validate(...)]` field is
//! present). The generated create/update handlers must invoke the autoref
//! `MaybeValidate` machinery without a trait-bound error.

mod schema {
    autumn_web::reexports::diesel::table! {
        articles (id) {
            id -> Int8,
            title -> Text,
        }
    }
}

use schema::articles;

#[autumn_web::model]
pub struct Article {
    #[id]
    pub id: i64,
    #[validate(length(min = 1, max = 200))]
    pub title: String,
}

#[autumn_web::repository(Article, api = "/api/articles")]
pub trait ArticleRepository {}

fn main() {
    // All five handlers compile, including the validating write path.
    let _ = article_api_list;
    let _ = article_api_get;
    let _ = article_api_create;
    let _ = article_api_update;
    let _ = article_api_delete;
}
