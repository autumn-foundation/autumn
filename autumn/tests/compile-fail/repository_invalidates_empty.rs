// Compile-fail: an empty `invalidates()` declares nothing. Silently accepting
// it would let a repository look discharged while covering no cached read.
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

#[autumn_web::repository(Article, invalidates())]
pub trait ArticleRepository {}

fn main() {}
