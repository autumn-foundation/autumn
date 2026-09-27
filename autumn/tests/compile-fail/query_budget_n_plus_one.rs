//! The classic N+1: a repository call inside a loop over runtime-sized rows.
//! The build fails on every branch, whether or not a test exercises this one.

use autumn_web::query_budget;

struct Post {
    author_id: i64,
}

struct Author;

struct PgPostRepository;

impl PgPostRepository {
    async fn find_all(&self) -> Result<Vec<Post>, ()> {
        Ok(Vec::new())
    }
    async fn find_author(&self, _id: i64) -> Result<Author, ()> {
        Ok(Author)
    }
}

#[query_budget(2)]
async fn index(repo: PgPostRepository) -> Result<usize, ()> {
    let posts = repo.find_all().await?;
    let mut authors = Vec::new();
    for post in &posts {
        authors.push(repo.find_author(post.author_id).await?);
    }
    Ok(authors.len())
}

fn main() {}
