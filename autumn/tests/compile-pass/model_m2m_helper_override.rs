//! #1785 escape hatch: two many-to-many associations to the *same* target
//! type compile when each carries a distinct `helper = "..."` override.
//!
//! This is the self-referential followers/following-through-`Friendship`
//! pattern: a `User` follows many users and is followed by many users, both
//! modeled as m2m through the same `friendships` join table. Without the
//! `helper` override both associations would derive the target-type helper
//! `add_user`/`remove_user` and collide (a compile error); the distinct
//! `helper = follower` / `helper = following` overrides give
//! `add_follower`/`remove_follower` and `add_following`/`remove_following`.
use autumn_web::model;

diesel::table! {
    users (id) {
        id -> BigInt,
        name -> Text,
    }
}

#[model]
#[has_many(User, through = friendships, name = followers,
           fk = followed_id, target_fk = follower_id, helper = follower)]
#[has_many(User, through = friendships, name = following,
           fk = follower_id, target_fk = followed_id, helper = following)]
pub struct User {
    #[id]
    pub id: i64,
    pub name: String,
}

fn main() {}
