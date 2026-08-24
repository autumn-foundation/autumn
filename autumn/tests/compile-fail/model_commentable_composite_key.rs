//! #1367: `commentable_id` is ONE column, so a composite-keyed model has no
//! single value to store there. Reject rather than silently key on the first
//! component.
use autumn_web::model;

diesel::table! {
    posts (tenant, id) {
        tenant -> Text,
        id -> BigInt,
        comment_count -> BigInt,
    }
}

#[model]
#[commentable(by = User)]
pub struct Post {
    #[id]
    pub tenant: String,
    #[id]
    pub id: i64,
    pub comment_count: i64,
}

fn main() {}
