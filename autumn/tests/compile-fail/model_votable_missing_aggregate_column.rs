//! #1362: the aggregate column `#[votable]` maintains must exist on the model.
//! The hidden edge module declares it regardless, so without this check a
//! missing field would only surface as a runtime `42703 column "score" does
//! not exist` on the first vote.
use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
    }
}

#[model]
#[votable(by = User, aggregate = sum)]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
}

fn main() {}
