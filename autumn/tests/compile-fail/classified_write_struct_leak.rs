//! #1654 (review round 2): the generated write structs carry the classification
//! too. `NewCustomer` is how an application *receives* a classified value and
//! its fields are `pub`, so a bare `String` there was a complete bypass -- a
//! handler could move the plaintext straight into a serializable view and
//! release personal data with no boundary and no audit record.
//!
//! Excluding the column from the write struct's own `Serialize` never closed
//! this: `skip_serializing` governs serializing `NewCustomer`, not moving a
//! value out of it.

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

fn leak_from_create(input: NewCustomer) -> SupportView {
    SupportView { email: input.email }
}

fn leak_from_patch(input: UpdateCustomer) -> SupportView {
    match input.email {
        autumn_web::hooks::Patch::Set(email) => SupportView { email },
        _ => SupportView {
            email: String::new(),
        },
    }
}

fn main() {}
