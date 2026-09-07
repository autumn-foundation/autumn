//! Lifecycle hooks for the `Note` repository.
//!
//! Hooks run inside the repository's transaction on every write path — the
//! GraphQL mutations, the generated REST handlers, a seed script — so a rule
//! lives in exactly one place. Two rules live here:
//!
//! - **`before_create` runs the model's `#[validate]` rules.** The generated
//!   REST `create` handler validates a payload before calling `save`, but
//!   `repo.save` itself does not, so a resolver or a seed calling the
//!   repository directly would bypass `#[validate]` without this. Running it
//!   here, after `#[normalize(trim)]` has already canonicalised the input,
//!   makes the rule hold for every door.
//! - **`before_delete` refuses to delete a pinned note** — a rule that needs
//!   the record, which is what hooks are for.
//!
//! Having hooks also switches `update` onto the hooked path, which loads the
//! row, merges the patch, normalises the merged model, and persists the
//! normalised draft.

use autumn_web::hooks::{MutationContext, MutationHooks};
use autumn_web::{AutumnError, AutumnResult};
use validator::Validate;

use crate::models::{NewNote, Note, UpdateNote};

#[derive(Clone, Default)]
pub struct NoteHooks;

impl MutationHooks for NoteHooks {
    type Model = Note;
    type NewModel = NewNote;
    type UpdateModel = UpdateNote;

    /// Enforce the model's `#[validate]` rules on the (already normalised)
    /// insert, as a `422`. The per-field messages are folded into the error
    /// text, so a GraphQL client — which sees only `errors[].message` — learns
    /// which field failed and why. (A bare `?` on the validator's error would
    /// convert it through the generic path and surface as a `500`.)
    async fn before_create(
        &self,
        _ctx: &mut MutationContext,
        new: &mut NewNote,
    ) -> AutumnResult<()> {
        if let Err(errors) = new.validate() {
            let mut messages: Vec<String> = errors
                .field_errors()
                .iter()
                .flat_map(|(field, errs)| {
                    errs.iter().map(move |e| {
                        let reason = e.message.as_deref().unwrap_or(&e.code);
                        format!("{field}: {reason}")
                    })
                })
                .collect();
            messages.sort();
            return Err(AutumnError::unprocessable_msg(messages.join("; ")));
        }
        Ok(())
    }

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
