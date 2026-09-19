//! The `Note` model.
//!
//! One `#[model]` struct yields the query type (`Note`), the insert type
//! (`NewNote`, without `#[id]`/`#[default]` fields) and the partial-update
//! type (`UpdateNote`, every field a `Patch<T>`).
//!
//! The write pipeline the generated repository runs is **normalize →
//! validate → hooks → write**, so the two attributes below compose: a title
//! of `"   "` is trimmed to `""` first, and the length rule then rejects it.
//! Every door into the table gets that behaviour — the GraphQL mutations,
//! the generated REST handlers, the startup seed — because it lives on the
//! model, not in a resolver.

use crate::schema::notes;

#[autumn_web::model]
pub struct Note {
    #[id]
    pub id: i64,
    #[normalize(trim)]
    #[validate(length(min = 1, max = 120, message = "title must be 1–120 characters"))]
    pub title: String,
    #[normalize(trim)]
    pub body: String,
    #[indexed]
    pub pinned: bool,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}
