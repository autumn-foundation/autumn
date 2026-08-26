use autumn_web::AutumnResult;
use autumn_web::hooks::{MutationContext, MutationHooks, UpdateDraft};

use crate::models::{NewPost, Post, UpdatePost};
use autumn_web::slugify;

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
        ctx: &mut MutationContext,
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

        // Advance the modification timestamp on every update path -- the edit
        // form, the generated API routes, a bulk update. `posts.updated_at`
        // carries a `DEFAULT NOW()` that only fires on INSERT, so without this
        // line an edited post keeps reporting its creation date forever. That
        // matters beyond the column itself: `/sitemap.xml` publishes it as each
        // post's `<lastmod>` (see `crate::seo`), which is how a crawler decides
        // whether to fetch the page again. A stale `<lastmod>` tells it not to.
        //
        // `ctx.now` is the framework's injected mutation timestamp, so this
        // stays deterministic under `#[sim_test]`; `chrono::Utc::now()` would
        // not (see the determinism seam gate in `clippy.toml`).
        draft.after.updated_at = ctx.now.naive_utc();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use autumn_web::hooks::MutationOp;
    use chrono::TimeZone as _;

    use super::*;

    /// A post created a year ago and never touched since.
    fn stale_post() -> Post {
        let created = chrono::NaiveDate::from_ymd_opt(2025, 5, 1)
            .expect("valid date")
            .and_hms_opt(12, 0, 0)
            .expect("valid time");
        Post {
            id: 1,
            title: "Original title".to_owned(),
            slug: "original-title".to_owned(),
            body: "Body".to_owned(),
            url: None,
            author_id: 1,
            subreddit_id: 1,
            score: 0,
            hot_rank: 0.0,
            comment_count: 0,
            created_at: created,
            updated_at: created,
        }
    }

    /// A context whose `now` is fixed, so the assertion below is exact.
    fn context_at(year: i32, month: u32, day: u32) -> MutationContext {
        let mut ctx = MutationContext::new(MutationOp::Update);
        ctx.now = chrono::Utc
            .with_ymd_and_hms(year, month, day, 9, 30, 0)
            .single()
            .expect("valid timestamp");
        ctx
    }

    #[tokio::test]
    async fn before_update_advances_updated_at() {
        let mut ctx = context_at(2026, 6, 2);
        let mut draft = UpdateDraft::new(stale_post());
        draft.after.body = "Edited body".to_owned();

        PostHooks
            .before_update(&mut ctx, &mut draft)
            .await
            .expect("hook succeeds");

        assert_eq!(
            draft.after.updated_at,
            ctx.now.naive_utc(),
            "an edit must advance posts.updated_at -- /sitemap.xml publishes it as <lastmod>",
        );
        assert_ne!(
            draft.after.updated_at, draft.before.updated_at,
            "the timestamp must move off the creation date",
        );
        assert_eq!(
            draft.after.created_at, draft.before.created_at,
            "created_at must not move",
        );
    }

    #[tokio::test]
    async fn before_update_advances_updated_at_even_without_a_field_change() {
        // A no-op save still counts as a modification for `<lastmod>`: the
        // hook must not make the timestamp conditional on a diff it cannot
        // see (tags, votes and comments all change the rendered page).
        let mut ctx = context_at(2026, 7, 4);
        let mut draft = UpdateDraft::new(stale_post());

        PostHooks
            .before_update(&mut ctx, &mut draft)
            .await
            .expect("hook succeeds");

        assert_eq!(draft.after.updated_at, ctx.now.naive_utc());
    }

    #[tokio::test]
    async fn before_update_still_reslugs_on_a_title_change() {
        let mut ctx = context_at(2026, 6, 2);
        let mut draft = UpdateDraft::new(stale_post());
        draft.after.title = "A brand new title".to_owned();

        PostHooks
            .before_update(&mut ctx, &mut draft)
            .await
            .expect("hook succeeds");

        assert_eq!(draft.after.slug, "a-brand-new-title");
    }
}
