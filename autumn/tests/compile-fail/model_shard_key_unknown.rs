use autumn_web::model;

diesel::table! {
    posts (id) {
        id -> BigInt,
        title -> Text,
    }
}

#[model]
#[shard_key = "nope"]
pub struct Post {
    #[id]
    pub id: i64,
    pub title: String,
}

fn main() {}
