// Compile-pass: #[repository(tenant_scoped, sharded)] generates a valid
// self-routing FromRequestParts extractor that resolves tenant → shard
// without requiring a ShardedDb extractor in the handler signature (§1209).

mod schema {
    autumn_web::reexports::diesel::table! {
        posts (id) {
            id -> Int8,
            title -> Text,
            tenant_id -> Nullable<Text>,
        }
    }
}

use schema::posts;

#[autumn_web::model]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub tenant_id: Option<String>,
}

// Standard sharded repository: tenant_scoped + sharded.
// The generated FromRequestParts will call __autumn_resolve_repo_seed.
#[autumn_web::repository(Post, tenant_scoped, sharded)]
pub trait PostRepository {}

// Sharded without tenant_scoped — routing uses ShardKeyOverride or tenancy config.
#[autumn_web::repository(Post, sharded, table = "posts")]
pub trait PostShardRepository {}

fn main() {}
