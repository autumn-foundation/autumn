//! Compile-fail test: `#[confidential(blind_index)]` needs its companion column
//! (issue #1771).
//!
//! The token is the only equality surface a sealed column has, so declaring the
//! opt-in without somewhere to store the token is refused rather than silently
//! producing a field nobody can look up.

diesel::table! {
    tokenless_notes (id) {
        id -> Integer,
        body -> Text,
    }
}

#[autumn_web::model(table = "tokenless_notes")]
pub struct TokenlessNote {
    pub id: i32,
    #[confidential(blind_index)]
    pub body: autumn_web::confidential::Sealed,
}

fn main() {}
