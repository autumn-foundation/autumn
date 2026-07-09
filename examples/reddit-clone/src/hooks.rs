use autumn_web::AutumnResult;
use autumn_web::hooks::{MutationContext, MutationHooks, UpdateDraft};

use crate::models::{NewPost, Post, UpdatePost};
use crate::slugify::slugify;

/// Mutation hooks for posts — auto-generate slug from title on
/// create and re-slug on title change during update.
#[derive(Clone, Default)]
pub struct PostHooks;

impl MutationHooks for PostHooks {
    type Model = Post;
    type NewModel = NewPost;
    type UpdateModel = UpdatePost;

    async fn before_create(
        &self,
        _ctx: &mut MutationContext,
        new: &mut NewPost,
    ) -> AutumnResult<()> {
        // Auto-generate slug from title if not already populated
        if new.slug.is_empty() {
            new.slug = slugify(&new.title);
            tracing::debug!(slug = %new.slug, "Generated post slug from title");
        }
        Ok(())
    }

    async fn before_update(
        &self,
        _ctx: &mut MutationContext,
        draft: &mut UpdateDraft<Post>,
    ) -> AutumnResult<()> {
        // Re-slug if title changed and slug was not manually set in the changes
        if draft.after.title != draft.before.title && draft.after.slug == draft.before.slug {
            draft.after.slug = slugify(&draft.after.title);
            tracing::debug!(
                old_slug = %draft.before.slug,
                new_slug = %draft.after.slug,
                "Re-slugged post after title change"
            );
        }
        Ok(())
    }
}
