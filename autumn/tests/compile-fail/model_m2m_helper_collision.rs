//! #1785: two many-to-many associations to the same target type with NO
//! `helper = "..."` override still collide on their target-derived
//! `add_`/`remove_` mutation helpers, and the error points at the
//! `helper = "..."` escape hatch as the fix.
use autumn_web::model;

diesel::table! {
    users (id) {
        id -> BigInt,
        name -> Text,
    }
}

#[model]
#[has_many(User, through = friendships, name = followers,
           fk = followed_id, target_fk = follower_id)]
#[has_many(User, through = friendships, name = following,
           fk = follower_id, target_fk = followed_id)]
pub struct User {
    #[id]
    pub id: i64,
    pub name: String,
}

fn main() {}
