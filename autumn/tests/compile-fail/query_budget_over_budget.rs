//! A straight-line handler that issues one query more than it declared. No
//! loop, no dynamism — just an arithmetic overrun the compiler now catches.

use autumn_web::query_budget;

struct Post;

struct PgPostRepository;

impl PgPostRepository {
    async fn find_all(&self) -> Result<Vec<Post>, ()> {
        Ok(Vec::new())
    }
    async fn count(&self) -> Result<i64, ()> {
        Ok(0)
    }
}

#[query_budget(1)]
async fn dashboard(repo: PgPostRepository) -> Result<usize, ()> {
    let posts = repo.find_all().await?;
    let total = repo.count().await?;
    Ok(posts.len() + total as usize)
}

fn main() {}
