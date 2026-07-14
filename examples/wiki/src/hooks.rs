use autumn_web::AutumnResult;
use autumn_web::hooks::{MutationContext, MutationHooks, UpdateDraft};

use crate::models::{NewPage, Page, UpdatePage};
use crate::slugify::slugify;

#[derive(Clone, Default)]
pub struct PageHooks;

impl MutationHooks for PageHooks {
    type Model = Page;
    type NewModel = NewPage;
    type UpdateModel = UpdatePage;

    async fn before_create(
        &self,
        _ctx: &mut MutationContext,
        new: &mut NewPage,
    ) -> AutumnResult<()> {
        // Auto-generate slug from title
        new.slug = slugify(&new.title);

        // Default status to "draft" if empty
        if new.status.trim().is_empty() {
            new.status = "draft".into();
        }

        // Only draft and published are valid initial statuses; archived can only
        // be reached via a transition from published.
        if !matches!(new.status.as_str(), "draft" | "published") {
            return Err(autumn_web::AutumnError::bad_request_msg(format!(
                "Invalid initial status `{}`; pages must start as `draft` or `published`",
                new.status
            )));
        }

        // Enforce the can_publish guard even on direct creates so a page
        // cannot be born already published with an empty title or body.
        if new.status == "published" && (new.title.trim().is_empty() || new.body.trim().is_empty())
        {
            return Err(autumn_web::AutumnError::bad_request_msg(
                "Cannot create a published page with an empty title or body",
            ));
        }

        Ok(())
    }

    async fn before_update(
        &self,
        _ctx: &mut MutationContext,
        draft: &mut UpdateDraft<Page>,
    ) -> AutumnResult<()> {
        // Re-slug if title changed
        if draft.after.title != draft.before.title {
            draft.after.slug = slugify(&draft.after.title);
        }

        // State-machine transitions are no longer enforced here: status changes
        // flow through the dedicated `POST /pages/{slug}/transitions/status`
        // route, which calls the macro-generated `Page::transition_status_to`.
        // The edit form only edits title/body, so `draft.after.status` always
        // equals `draft.before.status` on this path.

        // Maintain the published-page content invariant even when status is
        // unchanged; an edit that clears title/body on an already-published
        // page must be rejected the same way a direct published create would be.
        if draft.after.status == "published" && !draft.after.can_publish() {
            return Err(autumn_web::AutumnError::bad_request_msg(
                "A published page must have a non-empty title and body",
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_web::hooks::MutationOp;
    use chrono::Utc;

    #[tokio::test]
    async fn test_before_update_updates_slug_if_title_changes() {
        let hooks = PageHooks;
        let mut ctx = MutationContext::new(MutationOp::Update);
        let before = Page {
            id: 1,
            title: "Old Title".into(),
            slug: "old-title".into(),
            body: "Old Content".into(),
            status: "published".into(),
            lock_version: 0,
            created_at: Utc::now().naive_utc(),
            updated_at: Utc::now().naive_utc(),
        };

        let mut after = before.clone();
        after.title = "New Title".into();

        let mut draft = UpdateDraft { before, after };

        hooks.before_update(&mut ctx, &mut draft).await.unwrap();

        assert_eq!(draft.after.slug, "new-title");
    }

    #[tokio::test]
    async fn test_before_update_preserves_slug_if_title_unchanged() {
        let hooks = PageHooks;
        let mut ctx = MutationContext::new(MutationOp::Update);
        let before = Page {
            id: 1,
            title: "Old Title".into(),
            slug: "old-title".into(),
            body: "Old Content".into(),
            status: "published".into(),
            lock_version: 0,
            created_at: Utc::now().naive_utc(),
            updated_at: Utc::now().naive_utc(),
        };

        let mut after = before.clone();
        after.body = "New Content".into();

        let mut draft = UpdateDraft { before, after };

        hooks.before_update(&mut ctx, &mut draft).await.unwrap();

        assert_eq!(draft.after.slug, "old-title");
    }

    #[tokio::test]
    async fn after_update_can_declare_cache_invalidation() {
        use autumn_web::hooks::{MutationContext, MutationOp};

        let mut ctx = MutationContext::new(MutationOp::Update);
        let page = Page {
            id: 42,
            title: "Concurrent Edit".into(),
            slug: "concurrent-edit".into(),
            body: "Body".into(),
            status: "published".into(),
            lock_version: 1,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        };

        // Simulate what the app would do in after_update:
        // declare cache keys to invalidate after a successful write
        ctx.invalidate(format!("pages:{}", page.id));
        ctx.invalidate("pages:all");

        assert_eq!(ctx.invalidate_keys.len(), 2);
        assert!(ctx.invalidate_keys.contains(&format!("pages:{}", page.id)));
        assert!(ctx.invalidate_keys.contains(&"pages:all".to_string()));
    }

    #[test]
    fn concurrent_edit_version_mismatch_is_detectable() {
        // Simulate: replica A and replica B both read page at lock_version=3.
        // Replica A commits first (bumps to 4). Replica B then tries to commit
        // with expected_version=3, but stored is 4 — a conflict is detected.
        let stored_version: i64 = 4;
        let replica_b_expected: i64 = 3;

        // This is what the repository checks internally:
        let is_conflict = stored_version != replica_b_expected;
        assert!(is_conflict, "replica B should detect a conflict");

        let err = autumn_web::RepositoryError::Conflict {
            id: 99,
            expected_version: replica_b_expected,
            actual_version: Some(stored_version),
        };
        assert!(err.to_string().contains("99"));
        assert!(err.to_string().contains("3"));
    }

    fn page_with(status: &str, body: &str) -> Page {
        Page {
            id: 1,
            title: "My Page".into(),
            slug: "my-page".into(),
            body: body.into(),
            status: status.into(),
            lock_version: 0,
            created_at: Utc::now().naive_utc(),
            updated_at: Utc::now().naive_utc(),
        }
    }

    #[test]
    fn transition_status_to_allows_draft_to_published() {
        // draft -> published is a defined edge and the can_publish guard passes
        // on non-empty content. Enforcement now lives in the macro-generated
        // state machine, exercised by the transitions route.
        let page = page_with("draft", "Some content");
        assert!(page.can_transition_status_to("published"));
        assert_eq!(page.transition_status_to("published").unwrap(), "published");
    }

    #[test]
    fn transition_status_to_allows_published_to_archived() {
        // published -> archived is an unguarded defined edge, completing the
        // draft -> published -> archived chain.
        let page = page_with("published", "Some content");
        assert!(page.can_transition_status_to("archived"));
        assert_eq!(page.transition_status_to("archived").unwrap(), "archived");
    }

    #[test]
    fn transition_status_to_rejects_invalid_edge() {
        // published -> draft is not a defined edge, so the state machine rejects
        // it with a 400.
        let page = page_with("published", "Some content");
        assert!(!page.can_transition_status_to("draft"));
        assert!(page.transition_status_to("draft").is_err());
    }

    #[test]
    fn transition_status_to_guard_rejects_publishing_empty_body() {
        // The can_publish guard blocks draft -> published when body is empty.
        let page = page_with("draft", "");
        assert!(!page.can_transition_status_to("published"));
        assert!(
            page.transition_status_to("published").is_err(),
            "guard must reject publishing with empty body"
        );
    }

    #[test]
    fn transition_status_to_guard_rejects_whitespace_only_body() {
        // Whitespace-only body is treated as empty by the can_publish guard.
        let page = page_with("draft", "   ");
        assert!(
            page.transition_status_to("published").is_err(),
            "guard must reject publishing with whitespace-only body"
        );
    }

    #[tokio::test]
    async fn test_before_update_rejects_clearing_body_on_published_page() {
        // Status stays "published" but body is cleared — must be rejected.
        let hooks = PageHooks;
        let mut ctx = MutationContext::new(MutationOp::Update);
        let before = Page {
            id: 1,
            title: "My Page".into(),
            slug: "my-page".into(),
            body: "Original content".into(),
            status: "published".into(),
            lock_version: 0,
            created_at: Utc::now().naive_utc(),
            updated_at: Utc::now().naive_utc(),
        };
        let mut after = before.clone();
        after.body = String::new(); // clear body without changing status

        let mut draft = UpdateDraft { before, after };
        let result = hooks.before_update(&mut ctx, &mut draft).await;
        assert!(
            result.is_err(),
            "clearing body on a published page must fail"
        );
    }

    #[tokio::test]
    async fn test_before_update_allows_content_edits_on_published_page() {
        // Editing title/body on a published page is fine as long as they stay non-empty.
        let hooks = PageHooks;
        let mut ctx = MutationContext::new(MutationOp::Update);
        let before = Page {
            id: 1,
            title: "My Page".into(),
            slug: "my-page".into(),
            body: "Original content".into(),
            status: "published".into(),
            lock_version: 0,
            created_at: Utc::now().naive_utc(),
            updated_at: Utc::now().naive_utc(),
        };
        let mut after = before.clone();
        after.body = "Updated content".into();

        let mut draft = UpdateDraft { before, after };
        hooks.before_update(&mut ctx, &mut draft).await.unwrap();
    }

    #[tokio::test]
    async fn test_before_create_defaults_empty_status_to_draft() {
        let hooks = PageHooks;
        let mut ctx = MutationContext::new(MutationOp::Update);
        let mut new = NewPage {
            title: "My Page".into(),
            slug: String::new(),
            body: "Some content".into(),
            status: String::new(), // empty — should be defaulted to "draft"
        };
        hooks.before_create(&mut ctx, &mut new).await.unwrap();
        assert_eq!(new.status, "draft");
    }

    #[tokio::test]
    async fn test_before_create_rejects_invalid_initial_status() {
        let hooks = PageHooks;
        let mut ctx = MutationContext::new(MutationOp::Update);
        let mut new = NewPage {
            title: "My Page".into(),
            slug: String::new(),
            body: "Content".into(),
            status: "archived".into(),
        };
        let result = hooks.before_create(&mut ctx, &mut new).await;
        assert!(
            result.is_err(),
            "creating a page with status=archived must fail"
        );
    }

    #[tokio::test]
    async fn test_before_create_rejects_published_with_empty_body() {
        let hooks = PageHooks;
        let mut ctx = MutationContext::new(MutationOp::Update);
        let mut new = NewPage {
            title: "My Page".into(),
            slug: String::new(),
            body: String::new(),
            status: "published".into(),
        };
        let result = hooks.before_create(&mut ctx, &mut new).await;
        assert!(
            result.is_err(),
            "creating published page with empty body must fail"
        );
    }

    #[tokio::test]
    async fn test_before_create_allows_published_with_content() {
        let hooks = PageHooks;
        let mut ctx = MutationContext::new(MutationOp::Update);
        let mut new = NewPage {
            title: "My Page".into(),
            slug: String::new(),
            body: "Non-empty body".into(),
            status: "published".into(),
        };
        hooks.before_create(&mut ctx, &mut new).await.unwrap();
        assert_eq!(new.status, "published");
    }
}
