//! Compile-fail test: `#[confidential]` cannot be searchable (issue #1771).
//!
//! Full-text search indexes the stored column, and an index over ciphertext
//! matches nothing.

diesel::table! {
    searchable_notes (id) {
        id -> Integer,
        body -> Text,
    }
}

#[autumn_web::model(table = "searchable_notes")]
pub struct SearchableNote {
    pub id: i32,
    #[confidential]
    #[searchable]
    pub body: autumn_web::confidential::Sealed,
}

fn main() {}
