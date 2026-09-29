//! Compile-pass: the escape hatch the `#[confidential]` build failure names.
//!
//! A derived finder over the sealed column is refused
//! (`tests/compile-fail/confidential_find_by.rs`). The diagnostic tells the
//! author to query the blind-index companion instead, so that spelling has to
//! compile — including the `BlindIndex` parameter type the client sends.

diesel::table! {
    passing_notes (id) {
        id -> Int8,
        owner_id -> Text,
        body -> Text,
        body_bidx -> Text,
    }
}

#[autumn_web::model(table = "passing_notes")]
pub struct PassingNote {
    #[id]
    pub id: i64,
    pub owner_id: String,
    #[confidential(blind_index)]
    pub body: autumn_web::confidential::Sealed,
    pub body_bidx: autumn_web::confidential::BlindIndex,
}

#[autumn_web::repository(PassingNote, table = "passing_notes")]
pub trait PassingNoteRepository {
    async fn find_by_body_bidx(
        &self,
        body_bidx: &autumn_web::confidential::BlindIndex,
    ) -> Vec<PassingNote>;
    async fn count_by_owner_id(&self, owner_id: &str) -> i64;
}

fn main() {}
