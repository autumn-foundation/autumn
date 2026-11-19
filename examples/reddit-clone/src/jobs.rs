//! First-class Autumn background jobs for request-triggered side effects.
//!
//! Registration and post submission enqueue typed jobs instead of depending on
//! an external runner. The jobs runtime supplies retry/backoff and can
//! use Redis when the app is deployed with multiple processes.

use autumn_web::prelude::*;
use diesel::ExpressionMethods;
use diesel::QueryDsl;
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};

use crate::live_events::{
    post_created_event, publish_stored_live_event_best_effort, store_activity_event_for_state,
};
use crate::models::User;
use crate::schema::{posts, users};
use crate::tasks::calculate_hot_rank;

const STARTER_KARMA: i64 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserOnboardingArgs {
    pub user_id: i64,
    pub username: String,
}

impl UserOnboardingArgs {
    #[must_use]
    pub fn from_user(user: &User) -> Self {
        Self {
            user_id: user.id,
            username: user.username.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PostPublicationArgs {
    pub post_id: i64,
    pub title: String,
    pub post_slug: String,
    pub subreddit_slug: String,
    pub author_username: String,
}

impl PostPublicationArgs {
    #[must_use]
    pub fn new(
        post_id: i64,
        title: impl Into<String>,
        post_slug: impl Into<String>,
        subreddit_slug: impl Into<String>,
        author_username: impl Into<String>,
    ) -> Self {
        Self {
            post_id,
            title: title.into(),
            post_slug: post_slug.into(),
            subreddit_slug: subreddit_slug.into(),
            author_username: author_username.into(),
        }
    }
}

#[job(name = "user_onboarding", max_attempts = 5, backoff_ms = 500)]
pub async fn user_onboarding(state: AppState, args: UserOnboardingArgs) -> AutumnResult<()> {
    let pool = state
        .pool()
        .ok_or_else(|| AutumnError::service_unavailable_msg("No database pool"))?;
    let mut conn = pool.get().await.map_err(AutumnError::from)?;

    let affected = diesel::update(users::table.filter(users::id.eq(args.user_id)))
        .set(users::karma.eq(STARTER_KARMA))
        .execute(&mut conn)
        .await?;
    if affected != 1 {
        return Err(AutumnError::service_unavailable_msg(format!(
            "user {} was not visible to onboarding job",
            args.user_id
        )));
    }

    tracing::info!(
        user_id = args.user_id,
        username = %args.username,
        starter_karma = STARTER_KARMA,
        "completed user onboarding job"
    );
    Ok(())
}

#[job(name = "post_publication", max_attempts = 5, backoff_ms = 500)]
pub async fn post_publication(state: AppState, args: PostPublicationArgs) -> AutumnResult<()> {
    let pool = state
        .pool()
        .ok_or_else(|| AutumnError::service_unavailable_msg("No database pool"))?;
    let mut conn = pool.get().await.map_err(AutumnError::from)?;

    let (score, created_at): (i64, autumn_web::db::SqlDateTime) = posts::table
        .find(args.post_id)
        .select((posts::score, posts::created_at))
        .first(&mut conn)
        .await?;
    let hot_rank = calculate_hot_rank(
        score,
        created_at.into(),
        autumn_web::time::ambient_civil_utc(),
    );

    diesel::update(posts::table.find(args.post_id))
        .set(posts::hot_rank.eq(hot_rank))
        .execute(&mut conn)
        .await?;

    let event = post_created_event(
        args.post_id,
        &args.title,
        &args.post_slug,
        &args.subreddit_slug,
        &args.author_username,
    );
    let event_id =
        store_activity_event_for_state(&state, &mut conn, &args.subreddit_slug, &event).await?;
    publish_stored_live_event_best_effort(&state, event_id).await;

    tracing::info!(
        post_id = args.post_id,
        post_slug = %args.post_slug,
        subreddit_slug = %args.subreddit_slug,
        hot_rank,
        "completed post publication job"
    );
    Ok(())
}

#[must_use]
pub fn registered_jobs() -> Vec<autumn_web::job::JobInfo> {
    jobs![user_onboarding, post_publication]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_onboarding_args_copy_user_identity() {
        let user = User {
            id: 42,
            username: "ferris".to_string(),
            password_hash: "hashed".to_string(),
            karma: 0,
            role: "user".to_string(),
            created_at: autumn_web::time::civil::DateTime::constant(1970, 1, 1, 0, 0, 0, 0),
            avatar: None,
        };

        assert_eq!(
            UserOnboardingArgs::from_user(&user),
            UserOnboardingArgs {
                user_id: 42,
                username: "ferris".to_string(),
            }
        );
    }

    #[test]
    fn post_publication_args_capture_live_event_identity() {
        assert_eq!(
            PostPublicationArgs::new(99, "Ferris arrives", "ferris-arrives", "rust", "ferris"),
            PostPublicationArgs {
                post_id: 99,
                title: "Ferris arrives".to_string(),
                post_slug: "ferris-arrives".to_string(),
                subreddit_slug: "rust".to_string(),
                author_username: "ferris".to_string(),
            }
        );
    }
}
