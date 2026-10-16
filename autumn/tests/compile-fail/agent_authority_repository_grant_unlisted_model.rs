//! `#[repository(.., mcp, grant = X)]` governs the generated tools. A grant
//! that does not list the repository's model fails the build (#2451).

mod schema {
    autumn_web::reexports::diesel::table! {
        aa_grant_notes (id) {
            id -> Int8,
            body -> Text,
        }
    }
}

use schema::aa_grant_notes;

#[autumn_web::model]
pub struct AaGrantNote {
    #[id]
    pub id: i64,
    pub body: String,
}

autumn_web::authority_grant! {
    /// Lists no model at all.
    pub NoWrites {
        tenant_scope: scoped,
        reversibility: compensable,
    }
}

#[autumn_web::repository(AaGrantNote, api = "/api/aa-grant-notes", mcp, grant = NoWrites)]
pub trait AaGrantNoteRepository {}

fn main() {}
