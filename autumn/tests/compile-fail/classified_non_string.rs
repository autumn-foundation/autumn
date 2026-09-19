//! #1654: v1 classifies non-null `String` columns only.
diesel::table! {
    customers (id) {
        id -> Integer,
        age -> Integer,
    }
}

#[autumn_web::model(table = "customers")]
pub struct Customer {
    pub id: i32,
    #[classified]
    pub age: i32,
}

fn main() {}
