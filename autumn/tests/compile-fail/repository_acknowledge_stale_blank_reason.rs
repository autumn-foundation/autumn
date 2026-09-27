// Compile-fail: a repository-wide staleness opt-out must carry its reason.
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
    pub title: String,
}

#[autumn_web::repository(Article, acknowledge_stale = "")]
pub trait ArticleRepository {}

fn main() {}
