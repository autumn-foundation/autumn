//! A query inside a closure handed to an iterator adapter is the same N+1 in
//! functional clothing, and is rejected the same way.

use autumn_web::query_budget;

struct Post {
    author_id: i64,
}

struct Author;

struct PgAuthorRepository;

impl PgAuthorRepository {
    async fn find_by_id(&self, _id: i64) -> Result<Author, ()> {
        Ok(Author)
    }
}

#[query_budget(2)]
async fn index(repo: PgAuthorRepository, posts: Vec<Post>) -> Result<usize, ()> {
    let pending: Vec<_> = posts
        .iter()
        .map(|post| repo.find_by_id(post.author_id))
        .collect();
    Ok(pending.len())
}

fn main() {}
