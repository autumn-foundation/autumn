//! A delete through a repository also writes the models its `dependent(...)`
//! cascade names (#2451). The grant lists the parent and the child, but not the
//! grandchild a `destroy` child cascades into, so the build fails.

mod schema {
    autumn_web::reexports::diesel::table! {
        aa_cascade_posts (id) {
            id -> Int8,
            title -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        aa_cascade_comments (id) {
            id -> Int8,
            post_id -> Int8,
            body -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        aa_cascade_replies (id) {
            id -> Int8,
            comment_id -> Int8,
            body -> Text,
        }
    }
}

use autumn_web::prelude::*;
use schema::{aa_cascade_comments, aa_cascade_posts, aa_cascade_replies};

#[autumn_web::model]
#[has_many(AaCascadeComment, fk = "post_id", dependent = destroy)]
pub struct AaCascadePost {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::model]
#[has_many(AaCascadeReply, fk = "comment_id", dependent = destroy)]
pub struct AaCascadeComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub body: String,
}

#[autumn_web::model]
pub struct AaCascadeReply {
    #[id]
    pub id: i64,
    pub comment_id: i64,
    pub body: String,
}

#[autumn_web::repository(AaCascadePost)]
pub trait AaCascadePostRepository {}

#[autumn_web::repository(AaCascadeComment)]
pub trait AaCascadeCommentRepository {}

#[autumn_web::repository(AaCascadeReply)]
pub trait AaCascadeReplyRepository {}

autumn_web::authority_grant! {
    /// Lists the post and its comments, not the replies below them.
    pub PostRemover {
        writes: [AaCascadePost, AaCascadeComment],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[agent_operable(grant = PostRemover)]
async fn remove_post(posts: PgAaCascadePostRepository, id: i64) -> AutumnResult<()> {
    posts.delete_by_id(id).await?;
    Ok(())
}

fn main() {}
