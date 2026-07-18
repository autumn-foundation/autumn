use autumn_web::model;

diesel::table! {
    accounts (id) {
        id -> BigInt,
        shard_id -> BigInt,
        name -> Text,
    }
}

#[model]
#[shard_key = "shard_id"]
pub struct Account {
    #[id]
    pub id: i64,
    pub shard_id: i64,
    pub name: String,
}

diesel::table! {
    widgets (id) {
        id -> BigInt,
        name -> Text,
    }
}

// #[shard_key = "id"] — the primary key itself is always valid
#[model]
#[shard_key = "id"]
pub struct Widget {
    #[id]
    pub id: i64,
    pub name: String,
}

fn main() {}
