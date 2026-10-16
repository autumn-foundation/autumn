//! `#[repository(.., mcp, grant = X)]` also checks the generated delete tool
//! against the repository's `dependent(...)` cascade (#2451). The grant lists
//! the post, but not the comments its delete also writes.

mod schema {
    autumn_web::reexports::diesel::table! {
        aa_gc_posts (id) {
            id -> Int8,
            title -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        aa_gc_comments (id) {
            id -> Int8,
            post_id -> Int8,
        }
    }
}

use schema::{aa_gc_comments, aa_gc_posts};

#[autumn_web::model]
#[has_many(AaGcComment, fk = "post_id", dependent = destroy)]
pub struct AaGcPost {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::model]
pub struct AaGcComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
}

#[autumn_web::repository(AaGcComment)]
pub trait AaGcCommentRepository {}

autumn_web::authority_grant! {
    /// Lists the post only.
    pub PostOnly {
        writes: [AaGcPost],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[autumn_web::repository(AaGcPost, api = "/api/aa-gc-posts", mcp, grant = PostOnly)]
pub trait AaGcPostRepository {}

fn main() {}
