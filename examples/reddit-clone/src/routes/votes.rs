//! Vote routes — upvote and downvote posts via `#[votable]` + htmx.
//!
//! Demonstrates: the declarative `#[votable(by = User, aggregate = sum)]`
//! association (#1362) replacing ~130 lines of hand-written toggle/flip/upsert
//! SQL and a raw `UPDATE posts SET score = (SELECT SUM(...))` recompute with a
//! single race-safe `posts.react(...)` call; htmx partial updates; session
//! auth.

use autumn_web::extract::Path;
use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

// The reaction trait `#[votable]` emits on `Post`; it is blanket-implemented
// for `PgPostRepository`, which is what brings `react` / `reaction_of` into
// scope here.
use crate::models::{Post, PostReactions as _, Subreddit, User};
use crate::repositories::PgPostRepository;
use crate::schema::{posts, subreddits, users};

use super::layout::vote_controls;

/// Upvote a post (+1). Returns updated vote controls HTML via htmx.
#[post("/posts/{post_id}/upvote")]
pub async fn upvote(
    Path(post_id): Path<i64>,
    session: Session,
    posts_repo: PgPostRepository,
    State(state): State<AppState>,
) -> AutumnResult<Markup> {
    cast_vote(post_id, 1, &session, &posts_repo, &state).await
}

/// Downvote a post (-1). Returns updated vote controls HTML via htmx.
#[post("/posts/{post_id}/downvote")]
pub async fn downvote(
    Path(post_id): Path<i64>,
    session: Session,
    posts_repo: PgPostRepository,
    State(state): State<AppState>,
) -> AutumnResult<Markup> {
    cast_vote(post_id, -1, &session, &posts_repo, &state).await
}

/// Cast a vote on a post: authenticate, `react`, re-render the control.
///
/// NOTE: no `Db` extractor anywhere in this path. `react()` checks out its
/// *own* pooled connection (it does not join a caller's transaction), and this
/// example runs a single-connection pool — holding a `Db` across the call
/// would deadlock waiting for a second connection that can never free up.
async fn cast_vote(
    post_id: i64,
    value: i16,
    session: &Session,
    posts_repo: &PgPostRepository,
    state: &AppState,
) -> AutumnResult<Markup> {
    let user_id: i64 = session
        .get("user_id")
        .await
        .ok_or_else(|| AutumnError::unauthorized_msg("Login required to vote"))?
        .parse()
        .map_err(|_| AutumnError::bad_request_msg("Invalid session"))?;

    // Toggle / flip / insert this user's vote AND recompute `posts.score` from
    // ground truth, atomically and race-safely, in one call: the target row is
    // locked for the whole read-decide-write-recompute window. A missing or
    // soft-deleted post is `NotFound`. The returned `Reaction` carries the new
    // aggregate and the user's own value, so re-rendering needs no follow-up
    // query.
    let reaction = posts_repo.react(user_id, post_id, value).await?;

    broadcast_post_update(post_id, state).await?;

    Ok(vote_controls(post_id, reaction.aggregate, reaction.value))
}

/// Publish the updated post fragment to the global and per-subreddit SSE
/// topics (presentation, not vote logic).
///
/// Pool discipline: `react()` released its connection before returning, this
/// helper's checkout is short-lived and dropped before the fan-out, so no two
/// checkouts ever overlap on the example's `max_size = 1` pool.
async fn broadcast_post_update(post_id: i64, state: &AppState) -> AutumnResult<()> {
    let pool = state
        .pool()
        .ok_or_else(|| AutumnError::service_unavailable_msg("Database not configured"))?;
    let mut conn = pool.get().await.map_err(AutumnError::from)?;

    // Reload the post (its `score` was just updated inside `react()`'s
    // transaction), plus the relations the live fragment renders.
    let post: Post = posts::table.find(post_id).first(&mut conn).await?;
    let sub: Subreddit = subreddits::table
        .find(post.subreddit_id)
        .first(&mut conn)
        .await?;
    let author: User = users::table.find(post.author_id).first(&mut conn).await?;
    drop(conn);

    let lookup = crate::repositories::PostRelationsLookup {
        author_name: author.username,
        sub_name: sub.name,
        sub_slug: sub.slug.clone(),
    };

    let sse_state = state.clone();
    let sse_post = post;
    let sse_sub_slug = sub.slug;
    crate::repositories::CURRENT_POST_RELATIONS
        .scope(lookup, async move {
            let _ = sse_state.broadcast().publish_oob(
                "posts",
                &sse_post.dom_id(),
                &autumn_web::htmx::OobSwap::OuterHTML,
                &sse_post.render_fragment(),
            );

            let _ = sse_state.broadcast().publish_oob(
                &format!("posts:r/{sse_sub_slug}"),
                &sse_post.dom_id(),
                &autumn_web::htmx::OobSwap::OuterHTML,
                &sse_post.render_fragment(),
            );
        })
        .await;

    Ok(())
}

autumn_web::paths![upvote, downvote];
