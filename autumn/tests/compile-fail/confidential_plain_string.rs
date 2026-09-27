//! Compile-fail test: `#[confidential]` requires the `Sealed` type (#1771).
//!
//! A `String` field would suggest the server holds the plaintext. It never
//! does, so the column is declared as the envelope it actually stores.

diesel::table! {
    plain_notes (id) {
        id -> Integer,
        body -> Text,
    }
}

#[autumn_web::model(table = "plain_notes")]
pub struct PlainNote {
    pub id: i32,
    #[confidential]
    pub body: String,
}

fn main() {}
