//! The post ↔ term join, as a queryable model.
//!
//! This lives in its own module rather than in [`crate::models`] for a concrete
//! reason: `#[has_many(Term, through = post_terms)]` on `Post` expands into a
//! private module that does `use super::*` and then declares its own
//! `post_terms` table. A second `table!` for the same name in `models`'s scope
//! would land inside that glob and collide its trait impls. From here the two
//! never share a scope.
//!
//! The m2m association owns the **write** path (`set_terms`, which
//! [`crate::content::set_post_terms`] wraps to keep term counts correct); this
//! model owns the **read** path the archive screens need.

use crate::schema::post_terms;

/// One `(post, term)` filing.
#[autumn_web::model(table = "post_terms")]
pub struct PostTermLink {
    #[id]
    pub id: i64,
    #[indexed]
    pub post_id: i64,
    #[indexed]
    pub term_id: i64,
}

#[autumn_web::repository(PostTermLink, table = "post_terms")]
pub trait PostTermLinkRepository {
    /// Every term a post is filed under.
    fn find_by_post_id(post_id: i64) -> Vec<PostTermLink>;
    /// Every post filed under a term.
    fn find_by_term_id(term_id: i64) -> Vec<PostTermLink>;
}
