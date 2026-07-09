//! Record-level authorization policies for the reddit-clone app.
//!
//! Replaces the hand-rolled `if post.author_id != user_id` checks
//! that used to live inside every mutating route handler with a
//! single, typed [`PostPolicy`] declaration. Wired up on the app
//! builder via `.policy::<Post, _>(PostPolicy)` and consumed
//! declaratively from route handlers via `#[authorize(...)]`.

use autumn_web::authorization::{BoxFuture, Policy, PolicyContext};

use crate::models::Post;

/// Authorization rules for [`Post`].
///
/// - **Show** — public; everyone may read posts.
/// - **Create** — any authenticated user may post (route-level
///   `#[secured]` gates the unauthenticated path).
/// - **Update / Delete** — admins, or the post's own author.
#[derive(Default, Clone)]
pub struct PostPolicy;

impl Policy<Post> for PostPolicy {
    fn can_show<'a>(&'a self, _ctx: &'a PolicyContext, _post: &'a Post) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }

    fn can_create<'a>(&'a self, ctx: &'a PolicyContext) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.is_authenticated() })
    }

    fn can_update<'a>(&'a self, ctx: &'a PolicyContext, post: &'a Post) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.has_role("admin") || ctx.user_id_i64() == Some(post.author_id) })
    }

    fn can_delete<'a>(&'a self, ctx: &'a PolicyContext, post: &'a Post) -> BoxFuture<'a, bool> {
        Box::pin(async move { ctx.has_role("admin") || ctx.user_id_i64() == Some(post.author_id) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_web::session::Session;
    use std::collections::HashMap;

    fn make_post(author_id: i64) -> Post {
        Post {
            id: 1,
            title: "hello".into(),
            slug: "hello".into(),
            body: String::new(),
            url: None,
            author_id,
            subreddit_id: 1,
            score: 0,
            hot_rank: 0.0,
            comment_count: 0,
            created_at: chrono::NaiveDateTime::default(),
            updated_at: chrono::NaiveDateTime::default(),
        }
    }

    fn ctx(user_id: Option<&str>, role: Option<&str>) -> PolicyContext {
        let session = Session::new_for_test(String::new(), HashMap::new());
        PolicyContext {
            session,
            user_id: user_id.map(str::to_owned),
            roles: role.into_iter().map(str::to_owned).collect(),
            scopes: Vec::new(),
            pool: None,
            policy_registry: autumn_web::authorization::PolicyRegistry::default(),
        }
    }

    #[tokio::test]
    async fn anyone_can_show_a_post() {
        let policy = PostPolicy;
        let post = make_post(7);
        assert!(policy.can_show(&ctx(None, None), &post).await);
        assert!(policy.can_show(&ctx(Some("99"), None), &post).await);
    }

    #[tokio::test]
    async fn owner_can_update_and_delete_their_own_post() {
        let policy = PostPolicy;
        let post = make_post(42);
        let owner = ctx(Some("42"), None);
        assert!(policy.can_update(&owner, &post).await);
        assert!(policy.can_delete(&owner, &post).await);
    }

    #[tokio::test]
    async fn non_owner_cannot_update_or_delete_someone_elses_post() {
        let policy = PostPolicy;
        let post = make_post(42);
        let stranger = ctx(Some("99"), None);
        assert!(!policy.can_update(&stranger, &post).await);
        assert!(!policy.can_delete(&stranger, &post).await);
    }

    #[tokio::test]
    async fn admin_can_update_or_delete_any_post() {
        let policy = PostPolicy;
        let post = make_post(42);
        let admin = ctx(Some("99"), Some("admin"));
        assert!(policy.can_update(&admin, &post).await);
        assert!(policy.can_delete(&admin, &post).await);
    }

    #[tokio::test]
    async fn unauthenticated_user_cannot_create() {
        let policy = PostPolicy;
        assert!(!policy.can_create(&ctx(None, None)).await);
    }

    #[tokio::test]
    async fn authenticated_user_can_create() {
        let policy = PostPolicy;
        assert!(policy.can_create(&ctx(Some("1"), None)).await);
    }
}
