//! Compile-fail test: a server-side WHERE over a `#[confidential]` column
//! (issue #1771).
//!
//! The column is sealed under a key the server never holds, and sealing is
//! randomized, so the database can only ever compare ciphertext that never
//! repeats. The derived finder is refused at build time, and the diagnostic
//! names the blind-index companion column that does work.

diesel::table! {
    confidential_notes (id) {
        id -> Int8,
        owner_id -> Text,
        body -> Text,
        body_bidx -> Text,
    }
}

#[autumn_web::model(table = "confidential_notes")]
pub struct ConfidentialNote {
    #[id]
    pub id: i64,
    pub owner_id: String,
    #[confidential(blind_index)]
    pub body: autumn_web::confidential::Sealed,
    pub body_bidx: autumn_web::confidential::BlindIndex,
}

#[autumn_web::repository(ConfidentialNote, table = "confidential_notes")]
pub trait ConfidentialNoteRepository {
    async fn find_by_body(&self, body: String) -> Vec<ConfidentialNote>;
}

fn main() {}
