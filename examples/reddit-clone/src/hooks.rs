use autumn_web::AutumnResult;
use autumn_web::hooks::{MutationContext, MutationHooks, UpdateDraft};

use crate::models::{NewPost, NewSubreddit, Post, Subreddit, UpdatePost, UpdateSubreddit};
use autumn_web::{contains_letter_or_number, slugify};

/// The content rule the HTML forms enforce inline (`routes::posts::
/// validate_sluggable_title`, `routes::subreddits::validate_community_name`),
/// applied again here because the hooks are the only choke point the
/// **generated `/api/posts` and `/api/subreddits` routes** pass through.
///
/// A model-level `#[validate(custom(...))]` would not be enough: the `#[model]`
/// macro deliberately drops `custom` from the `UpdateModel` PATCH path, because
/// `Patch<T>` implements only validator's declarative per-field traits. It
/// would therefore cover an API *create* and leave an API *update* — renaming a
/// community to `"***"` — wide open. A hook sees the merged model on both.
///
/// Without this, #2424's fix would hold only for whichever entry point the
/// client happened to use. The forms validate first and never reach here, so a
/// human still gets the inline field error rather than a 422 page.
fn reject_content_free(subject: &str, value: &str) -> AutumnResult<()> {
    if contains_letter_or_number(value) {
        return Ok(());
    }
    Err(autumn_web::AutumnError::unprocessable_msg(format!(
        "{subject} must contain at least one letter or number"
    )))
}

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
        reject_content_free("Title", &new.title)?;

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
        if draft.after.title != draft.before.title {
            reject_content_free("Title", &draft.after.title)?;
        }

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
        // matters beyond the column itself: `/sitemap.xml` feeds it into each
        // post's `<lastmod>` (see `crate::seo`), which is how a crawler decides
        // whether to fetch the page again. A stale `<lastmod>` tells it not to.
        //
        // This covers edits only. A comment changes the page without touching
        // the `posts` row at all, so `crate::seo` derives the final
        // `<lastmod>` as the later of this column and the newest live comment
        // rather than expecting every subsystem to write a timestamp here.
        //
        // `ctx.now` is the framework's injected mutation timestamp, so this
        // stays deterministic under `#[sim_test]`; `chrono::Utc::now()` would
        // not (see the determinism seam gate in `clippy.toml`).
        draft.after.updated_at = ctx.now.naive_utc();

        Ok(())
    }
}

/// Mutation hooks for communities.
///
/// Validation only: the HTML `create` route derives the slug itself, and
/// re-slugging a renamed community would change its URLs, which is a bigger
/// decision than this hook should make on its own.
#[derive(Clone, Default)]
pub struct SubredditHooks;

impl MutationHooks for SubredditHooks {
    type Model = Subreddit;
    type NewModel = NewSubreddit;
    type UpdateModel = UpdateSubreddit;

    async fn before_create(
        &self,
        _ctx: &mut MutationContext,
        new: &mut NewSubreddit,
    ) -> AutumnResult<()> {
        reject_content_free("Community name", &new.name)
    }

    async fn before_update(
        &self,
        _ctx: &mut MutationContext,
        draft: &mut UpdateDraft<Subreddit>,
    ) -> AutumnResult<()> {
        if draft.after.name != draft.before.name {
            reject_content_free("Community name", &draft.after.name)?;
        }
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

    // ── Content-free titles through the generated API (#2424) ──────

    #[tokio::test]
    async fn before_create_rejects_a_title_with_no_letter_or_number() {
        // `POST /api/posts` runs the model's `#[validate]` attributes and the
        // hooks -- never `routes::posts::validate_sluggable_title` -- so this
        // is the only thing standing between the API and a post whose URL is
        // a bare hash.
        for title in ["***", "!!!???...:::", "🎉🔥💯"] {
            let mut ctx = MutationContext::new(MutationOp::Create);
            let mut new = NewPost {
                title: title.to_owned(),
                slug: String::new(),
                body: "Body".to_owned(),
                url: None,
                author_id: 1,
                subreddit_id: 1,
            };

            let error = PostHooks
                .before_create(&mut ctx, &mut new)
                .await
                .expect_err("a content-free title must not reach the database");

            assert!(
                error.to_string().contains("at least one letter or number"),
                "{title:?} must explain itself; got: {error}"
            );
        }
    }

    #[tokio::test]
    async fn before_create_still_slugs_a_real_title_in_any_script() {
        for title in ["Ferris arrives", "日本語"] {
            let mut ctx = MutationContext::new(MutationOp::Create);
            let mut new = NewPost {
                title: title.to_owned(),
                slug: String::new(),
                body: "Body".to_owned(),
                url: None,
                author_id: 1,
                subreddit_id: 1,
            };

            PostHooks
                .before_create(&mut ctx, &mut new)
                .await
                .unwrap_or_else(|error| panic!("{title:?} is a real title: {error}"));

            assert_eq!(new.slug, slugify(title));
        }
    }

    #[tokio::test]
    async fn before_update_rejects_retitling_to_nothing() {
        let mut ctx = context_at(2026, 6, 2);
        let mut draft = UpdateDraft::new(stale_post());
        draft.after.title = "***".to_owned();

        let error = PostHooks
            .before_update(&mut ctx, &mut draft)
            .await
            .expect_err("an API retitle must not empty a post's title either");

        assert!(
            error.to_string().contains("at least one letter or number"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn subreddit_hooks_reject_a_content_free_name_on_create_and_rename() {
        // `/api/subreddits` runs the model's `#[validate]` attributes -- which
        // check only length -- so a 3-character `"***"` was persisted through
        // the API even with `routes::subreddits::create` fixed (#2424, Codex).
        let mut ctx = MutationContext::new(MutationOp::Create);
        let mut new = NewSubreddit {
            name: "***".to_owned(),
            slug: "irrelevant".to_owned(),
            description: String::new(),
            creator_id: 1,
        };
        let error = SubredditHooks
            .before_create(&mut ctx, &mut new)
            .await
            .expect_err("a content-free community name must not reach the database");
        assert!(
            error.to_string().contains("at least one letter or number"),
            "got: {error}"
        );

        // The PATCH path is the half a model-level `#[validate(custom(...))]`
        // would have missed: the `#[model]` macro drops `custom` from the
        // `UpdateModel`, so only a hook catches a rename to nothing.
        let mut ctx = MutationContext::new(MutationOp::Update);
        let mut draft = UpdateDraft::new(Subreddit {
            id: 1,
            name: "rust".to_owned(),
            slug: "rust".to_owned(),
            description: String::new(),
            creator_id: 1,
            subscriber_count: 0,
            comment_count: 0,
            created_at: chrono::NaiveDateTime::default(),
        });
        draft.after.name = "🎉🔥💯".to_owned();

        let error = SubredditHooks
            .before_update(&mut ctx, &mut draft)
            .await
            .expect_err("an API rename to nothing must not stand either");
        assert!(
            error.to_string().contains("at least one letter or number"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn subreddit_hooks_accept_a_real_name_in_any_script() {
        for name in ["rust", "日本語"] {
            let mut ctx = MutationContext::new(MutationOp::Create);
            let mut new = NewSubreddit {
                name: name.to_owned(),
                slug: "irrelevant".to_owned(),
                description: String::new(),
                creator_id: 1,
            };

            SubredditHooks
                .before_create(&mut ctx, &mut new)
                .await
                .unwrap_or_else(|error| panic!("{name:?} is a real name: {error}"));
        }
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
        // A no-op save still counts as a modification: the hook cannot see
        // every reason a caller saved (a normalizer may have rewritten a
        // field, or the caller may be re-persisting a value it recomputed),
        // so it must not make the timestamp conditional on a column diff.
        //
        // This hook covers the `PgPostRepository::update` path only. Comments
        // never touch the `posts` row -- `crate::seo` derives their effect on
        // `<lastmod>` at read time instead.
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
