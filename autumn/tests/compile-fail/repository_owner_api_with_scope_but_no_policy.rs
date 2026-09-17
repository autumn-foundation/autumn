//! Compile-fail test: `owner = <column>` + `scope = Type` next to
//! `api = "..."`, with no `policy`, is still rejected (Warden security
//! review, 2026-09-13 — a gap in the fix's first cut, caught in review on
//! PR #2770 before merge).
//!
//! `scope = Type` only ever filters the *list* endpoint's SQL query
//! (`scope_list_body`'s `scope_type.is_some()` arm). The single-record
//! handlers (`_api_get`/`_api_update`/`_api_delete`) have no `scope`-driven
//! equivalent — only `has_policy` gates them
//! (`policy_check_show`/`policy_check_update_pre`/`policy_check_delete_pre`).
//! So `owner = <column>` + `scope = Type` with no `policy` would still leave
//! `GET`/`PUT`/`DELETE <api>/{id}` fully open to any authenticated caller,
//! even though the list endpoint alone looks correctly scoped. `policy` is
//! required; `scope` is only ever an optional performance companion to it.

mod schema {
    autumn_web::reexports::diesel::table! {
        owner_api_scope_only_notes (id) {
            id -> Int8,
            title -> Text,
            author_id -> Int8,
        }
    }
}

use schema::owner_api_scope_only_notes;

#[autumn_web::model(table = "owner_api_scope_only_notes")]
pub struct OwnerApiScopeOnlyNote {
    #[id]
    pub id: i64,
    pub title: String,
    pub author_id: i64,
}

pub struct OwnerApiScopeOnlyNoteScope;

// The `list` method has a fail-closed default (returns no rows), so this
// fixture does not need to implement it — only the type needs to exist and
// implement `Scope<OwnerApiScopeOnlyNote>`.
impl autumn_web::authorization::Scope<OwnerApiScopeOnlyNote> for OwnerApiScopeOnlyNoteScope {}

#[autumn_web::repository(
    OwnerApiScopeOnlyNote,
    table = "owner_api_scope_only_notes",
    api = "/api/owner-scope-only-notes",
    owner = author_id,
    scope = OwnerApiScopeOnlyNoteScope
)]
pub trait OwnerApiScopeOnlyNoteRepository {}

fn main() {}
