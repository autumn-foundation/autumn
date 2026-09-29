//! A helper handed the database handle is opaque to the analysis. Rather than
//! assume it queries nothing — which would be a false negative — the gate
//! reports it and names the two annotations that resolve it.

use autumn_web::query_budget;

struct Db;

struct Link;

async fn load_links(_db: &mut Db, _id: i64) -> Result<Vec<Link>, ()> {
    Ok(Vec::new())
}

#[query_budget(5)]
async fn show(mut db: Db) -> Result<usize, ()> {
    let links = load_links(&mut db, 1).await?;
    Ok(links.len())
}

fn main() {}
