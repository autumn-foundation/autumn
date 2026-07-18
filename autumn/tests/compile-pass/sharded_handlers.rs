// Compile-pass: handler signatures using the sharding extractors, and the
// generated `with_pool_untracked` / `from_shard` repository constructors.

mod schema {
    autumn_web::reexports::diesel::table! {
        notes (id) {
            id -> Int8,
            content -> Text,
        }
    }
}

use autumn_web::prelude::*;
use schema::notes;

#[autumn_web::model]
pub struct Note {
    #[id]
    pub id: i64,
    pub content: String,
}

#[autumn_web::repository(Note)]
pub trait NoteRepository {
    fn find_by_content(content: String) -> Vec<Note>;
}

// Implicit tenant-routed extraction.
#[get("/notes")]
async fn list_notes(db: ShardedDb) -> AutumnResult<String> {
    Ok(format!("shard {}", db.shard()))
}

// Explicit per-key routing plus fan-out.
#[get("/notes/{user_id}")]
async fn user_notes(shards: Shards, Path(user_id): Path<i64>) -> AutumnResult<&'static str> {
    let _db = shards.db_for(user_id).await?;
    let _ro = shards.read_for(user_id).await?;
    let _admin = shards.db_on("shard0").await?;
    let _counts = shards
        .each_shard(|shard, _db| {
            let _name = shard.name().to_owned();
            async move { Ok(0i64) }
        })
        .await;
    Ok("ok")
}

// Repository constructed over an explicit shard pool (untracked escape hatch).
async fn repo_on_shard_untracked(shards: &Shards, tenant: &str) -> AutumnResult<PgNoteRepository> {
    let shard = shards.set().route(tenant).await?;
    Ok(PgNoteRepository::with_pool_untracked(shard.primary_pool().clone()))
}

// Repository constructed from a ShardedDb — preserves full instrumentation.
async fn repo_from_shard(db: &ShardedDb) -> PgNoteRepository {
    PgNoteRepository::from_shard(db)
}

// Explicit replica-only extractor: 503 if no replica is configured/healthy.
#[get("/analytics")]
async fn analytics(db: ShardedReadDb) -> AutumnResult<String> {
    Ok(format!("replica of shard {}", db.shard()))
}

fn main() {
    let _ = list_notes;
    let _ = user_notes;
    let _ = repo_on_shard_untracked;
    let _ = repo_from_shard;
    let _ = analytics;
}
