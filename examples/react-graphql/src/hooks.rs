//! Lifecycle hooks for the `Note` repository.
//!
//! Hooks run inside the repository's transaction on every write path — the
//! GraphQL mutations, the generated REST handlers, a seed script — so a rule
//! lives in exactly one place. Two rules live here:
//!
//! - **`before_delete` refuses to delete a pinned note** — a rule that needs
//!   the record, which is what hooks are for.
//!
//! There is deliberately no `before_create` here. This example used to run the
//! model's `#[validate]` rules in one, because `repo.save` did not — the gap
//! issue #2586 describes. The repository now runs them itself on every insert
//! path, after `#[normalize]` and *before* this hook would see the record, so
//! such a hook would be unreachable code. The per-field detail a GraphQL client
//! needs is read back out of the error with `AutumnError::details()` in
//! `notes::to_gql`.
//!
//! Having hooks also switches `update` onto the hooked path, which loads the
//! row, merges the patch, normalises the merged model, and persists the
//! normalised draft.

use autumn_web::hooks::{MutationContext, MutationHooks};
use autumn_web::{AutumnError, AutumnResult};

use crate::models::{NewNote, Note, UpdateNote};

#[derive(Clone, Default)]
pub struct NoteHooks;

impl MutationHooks for NoteHooks {
    type Model = Note;
    type NewModel = NewNote;
    type UpdateModel = UpdateNote;

    /// A pinned note is one the user chose to protect: refuse to delete it
    /// until it is unpinned. Returning `Err` aborts the `DELETE` and rolls the
    /// transaction back.
    async fn before_delete(&self, _ctx: &mut MutationContext, record: &Note) -> AutumnResult<()> {
        if record.pinned {
            return Err(AutumnError::unprocessable_msg(format!(
                "note {} is pinned; unpin it before deleting",
                record.id
            )));
        }
        Ok(())
    }
}
