//! #1654: the field markers `#[model]` generates live in the *application's*
//! crate, so without a sealed guard one safe line would satisfy the orphan rule
//! and turn the withheld `Serialize` impl back on for that column -- silently,
//! with no boundary and no manifest row.
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
    pub email: String,
}

impl autumn_web::classify::ReleasedForSink for CustomerEmailClassified {}

fn main() {}
