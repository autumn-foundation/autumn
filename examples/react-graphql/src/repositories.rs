//! The `Note` repository.
//!
//! `#[repository]` generates `PgNoteRepository` with `find_by_id`, `find_all`,
//! `save`, `update`, `delete_by_id`, `count`, `exists_by_id`, the bulk
//! variants, and one derived finder per method declared below. `hooks =`
//! wires [`NoteHooks`] into every write, and `api =` also generates JSON REST
//! handlers (`note_api_list`, `note_api_get`, …) over the same repository —
//! `src/lib.rs` mounts the two read handlers, so the exact rows the GraphQL
//! resolvers return are also visible at `GET /api/notes`.

use crate::hooks::NoteHooks;
use crate::models::{NewNote, Note, NoteDraftExt, UpdateNote};
use crate::schema::notes;

#[autumn_web::repository(Note, hooks = NoteHooks, api = "/api/notes")]
pub trait NoteRepository {
    /// `SELECT … WHERE pinned = $1`, backed by `idx_notes_pinned`.
    fn find_by_pinned(pinned: bool) -> Vec<Note>;
}
