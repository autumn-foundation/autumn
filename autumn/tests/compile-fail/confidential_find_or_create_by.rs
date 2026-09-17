//! Compile-fail test: `find_or_create_by` over a `#[confidential]` column
//! (issue #1771).
//!
//! A get-or-insert looks the row up first, so it is refused on the same terms
//! as `find_by`: the lookup can only compare ciphertext that never repeats.

diesel::table! {
    upsert_notes (id) {
        id -> Int8,
        body -> Text,
        body_bidx -> Text,
    }
}

#[autumn_web::model(table = "upsert_notes")]
pub struct UpsertNote {
    #[id]
    pub id: i64,
    #[confidential(blind_index)]
    pub body: autumn_web::confidential::Sealed,
    pub body_bidx: autumn_web::confidential::BlindIndex,
}

#[autumn_web::repository(UpsertNote, table = "upsert_notes")]
pub trait UpsertNoteRepository {
    async fn find_or_create_by_body(
        &self,
        body: autumn_web::confidential::Sealed,
    ) -> (UpsertNote, bool);
}

fn main() {}
