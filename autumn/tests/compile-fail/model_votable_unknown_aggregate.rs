//! #1362: `sum` (signed up/down votes) and `count` (unary likes) are the only
//! aggregate modes — there is no `avg`, and arbitrary signed weights are
//! expressed with sum + `value_column`. An unknown mode must not fall back.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
        score -> BigInt,
    }
}

#[model]
#[votable(by = User, aggregate = avg)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
    pub score: i64,
}

fn main() {}
