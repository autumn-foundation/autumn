// Compile-pass: #[repository(soft_delete)] with a model that has deleted_at.

mod schema {
    autumn_web::reexports::diesel::table! {
        articles (id) {
            id -> Int8,
            title -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }
}

use schema::articles;

#[autumn_web::model]
pub struct Article {
    #[id]
    pub id: i64,
    pub title: String,
    pub deleted_at: Option<autumn_web::time::civil::DateTime>,
}

#[autumn_web::repository(Article, soft_delete)]
pub trait ArticleRepository {}

fn main() {}
