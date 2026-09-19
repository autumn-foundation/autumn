// Compile-fail: an invalidation edge must name a real `#[cached]` function.
// The macro rewrites the path to the id constant `#[cached]` generates beside
// the function, so rustc — not a string table — rejects a bad target.
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

#[autumn_web::repository(Article, invalidates(not_a_cached_function))]
pub trait ArticleRepository {}

fn main() {}
