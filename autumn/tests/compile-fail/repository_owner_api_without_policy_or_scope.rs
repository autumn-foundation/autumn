//! Compile-fail test: `owner = <column>` next to `api = "..."` with no
//! `policy` is rejected (Warden security review, 2026-09-13). This fixture
//! carries neither `policy` nor `scope`; see
//! `repository_owner_api_with_scope_but_no_policy.rs` for why `scope = Type`
//! alone does not satisfy the gate either.
//!
//! `owner = <column>` only emits opt-in `list_scoped`/`search_page_scoped`
//! repository methods for a hand-written handler to call with an explicit
//! owner id. The auto-generated `api = "..."` CRUD handlers never call them —
//! only `policy = Type` actually gates the single-record handlers — so this
//! combination used to compile to a fully public REST API: every row
//! readable via `GET <api>`, and any single row readable/overwritable/
//! deletable by id via `GET`/`PUT`/`DELETE <api>/{id}`, regardless of
//! `owner_column`. The macro now refuses the configuration rather than
//! shipping a guarantee with a hole in it.

mod schema {
    autumn_web::reexports::diesel::table! {
        owner_api_bypass_notes (id) {
            id -> Int8,
            title -> Text,
            author_id -> Int8,
        }
    }
}

use schema::owner_api_bypass_notes;

#[autumn_web::model(table = "owner_api_bypass_notes")]
pub struct OwnerApiBypassNote {
    #[id]
    pub id: i64,
    pub title: String,
    pub author_id: i64,
}

#[autumn_web::repository(
    OwnerApiBypassNote,
    table = "owner_api_bypass_notes",
    api = "/api/owner-bypass-notes",
    owner = author_id
)]
pub trait OwnerApiBypassNoteRepository {}

fn main() {}
