// Compile-pass: the #1716 coherence surface — a declared dependency set, an
// acknowledged-stale opt-out, and a repository that discharges its obligation
// with an invalidation edge naming a real `#[cached]` function.
mod schema {
    autumn_web::reexports::diesel::table! {
        articles (id) {
            id -> Int8,
            title -> Text,
        }
    }
}

use autumn_web::cached;
use schema::articles;

#[autumn_web::model]
pub struct Article {
    #[id]
    pub id: i64,
    pub title: String,
}

#[cached(ttl = "5m", reads(Article))]
pub async fn recent_titles() -> Vec<String> {
    Vec::new()
}

#[cached(reads(Article), acknowledge_stale = "the ticker tolerates a 5s lag")]
pub async fn article_ticker() -> i64 {
    0
}

// A read whose dependency set the macro derives on its own, from the
// repository type in the signature — and the shape that makes `key(...)` load
// bearing: the handle is `Clone` but not `Hash`, so it must stay out of the
// key.
#[cached(key(article_id), acknowledge_stale = "derived reads are demo-only here")]
pub async fn derived_reader(article_id: i64, _repo: &PgArticleRepository) -> i64 {
    article_id
}

#[autumn_web::repository(Article, invalidates(recent_titles))]
pub trait ArticleRepository {
    #[invalidates(crate::article_ticker)]
    #[allow(dead_code)]
    async fn delete_by_title(&self, title: &str) -> ();
}

fn main() {
    let _ = PgArticleRepository::invalidate_declared_caches;
}
