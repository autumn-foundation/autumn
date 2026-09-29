//! #1654: `#[classified]` and `#[encrypted]` both rewrite the column's Diesel
//! representation, so v1 rejects the combination rather than silently dropping
//! one of them.
diesel::table! {
    customers (id) {
        id -> Integer,
        email -> Text,
    }
}

#[autumn_web::model(table = "customers")]
pub struct Customer {
    pub id: i32,
    #[classified]
    #[encrypted]
    pub email: String,
}

fn main() {}
