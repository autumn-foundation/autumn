//! #2373: the generated `{Model}Factory` carries the classification too.
//!
//! The factory's fields are `pub` and live for the whole builder chain, so a
//! bare `String` there was a complete bypass: a handler could move
//! `Customer::factory().email` straight into a serializable view and release
//! personal data with no boundary and no audit record. Classifying only at
//! `build()` time was too late.
//!
//! The factory is emitted in every build, not only test ones, so this was not
//! confined to test code.

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

#[derive(serde::Serialize)]
struct SupportView {
    email: String,
}

fn leak_from_factory() -> SupportView {
    SupportView {
        email: Customer::factory().email("ada@example.com").email,
    }
}

fn main() {}
