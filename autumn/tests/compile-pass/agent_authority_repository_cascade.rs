//! A delete through a repository is checked against every model its
//! `dependent(...)` cascade writes (#2451). The grant lists them all, so the
//! build passes. A `restrict` leg writes nothing, and a model that cascades
//! into itself adds no cycle.

mod schema {
    autumn_web::reexports::diesel::table! {
        aa_ok_posts (id) {
            id -> Int8,
            title -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        aa_ok_comments (id) {
            id -> Int8,
            post_id -> Int8,
            body -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        aa_ok_audits (id) {
            id -> Int8,
            post_id -> Int8,
        }
    }
    autumn_web::reexports::diesel::table! {
        aa_cycle_as (id) {
            id -> Int8,
            b_id -> Nullable<Int8>,
        }
    }
    autumn_web::reexports::diesel::table! {
        aa_cycle_bs (id) {
            id -> Int8,
            a_id -> Nullable<Int8>,
        }
    }
    autumn_web::reexports::diesel::table! {
        aa_ok_categories (id) {
            id -> Int8,
            parent_id -> Nullable<Int8>,
            name -> Text,
        }
    }
}

use autumn_web::prelude::*;
use schema::{aa_cycle_as, aa_cycle_bs, aa_ok_audits, aa_ok_categories, aa_ok_comments, aa_ok_posts};

#[autumn_web::model]
#[has_many(AaOkComment, fk = "post_id", dependent = destroy)]
#[has_many(AaOkAudit, fk = "post_id", dependent = restrict)]
pub struct AaOkPost {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::model]
pub struct AaOkComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub body: String,
}

#[autumn_web::model]
pub struct AaOkAudit {
    #[id]
    pub id: i64,
    pub post_id: i64,
}

#[autumn_web::model]
#[has_many(AaOkCategory, fk = "parent_id", dependent = destroy)]
pub struct AaOkCategory {
    #[id]
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
}

/// Two models that cascade into each other. The runtime breaks the loop. The
/// build must still compile.
#[autumn_web::model]
#[has_many(AaCycleB, fk = "a_id", dependent = destroy)]
pub struct AaCycleA {
    #[id]
    pub id: i64,
    pub b_id: Option<i64>,
}

#[autumn_web::model]
#[has_many(AaCycleA, fk = "b_id", dependent = destroy)]
pub struct AaCycleB {
    #[id]
    pub id: i64,
    pub a_id: Option<i64>,
}

#[autumn_web::repository(AaCycleA)]
pub trait AaCycleARepository {}

#[autumn_web::repository(AaCycleB)]
pub trait AaCycleBRepository {}

#[autumn_web::repository(AaOkPost)]
pub trait AaOkPostRepository {}

#[autumn_web::repository(AaOkComment)]
pub trait AaOkCommentRepository {}

#[autumn_web::repository(AaOkAudit)]
pub trait AaOkAuditRepository {}

#[autumn_web::repository(AaOkCategory)]
pub trait AaOkCategoryRepository {}

autumn_web::authority_grant! {
    /// Lists the post and the one model its delete cascades into.
    pub PostRemover {
        writes: [AaOkPost, AaOkComment],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

autumn_web::authority_grant! {
    /// A tree deletes itself; nothing else.
    pub CategoryRemover {
        writes: [AaOkCategory],
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[agent_operable(grant = PostRemover)]
async fn remove_post(posts: PgAaOkPostRepository, id: i64) -> AutumnResult<()> {
    posts.delete_by_id(id).await?;
    Ok(())
}

#[agent_operable(grant = CategoryRemover)]
async fn remove_category(categories: PgAaOkCategoryRepository, id: i64) -> AutumnResult<()> {
    categories.delete_by_id(id).await?;
    Ok(())
}

fn main() {
    let post = PgAaOkPostRepository::__AUTUMN_CASCADE_3;
    assert_eq!(post.models, ["AaOkComment"]);
    assert!(!post.more, "a shallow cascade is fully listed");
    assert!(PgAaOkCommentRepository::__AUTUMN_CASCADE_3.models.is_empty());
    assert!(PgAaOkCategoryRepository::__AUTUMN_CASCADE_3.models.is_empty());
    // A loop is never fully listed, so a governed delete through it is refused.
    assert!(PgAaCycleARepository::__AUTUMN_CASCADE_3.more);
}
