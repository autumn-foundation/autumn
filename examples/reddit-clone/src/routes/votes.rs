//! Vote routes — upvote and downvote posts via `#[votable]` + htmx.
//!
//! Demonstrates: the declarative `#[votable(by = User, aggregate = sum)]`
//! association (#1362) replacing ~90 lines of hand-written toggle/flip/upsert
//! SQL and a raw `UPDATE posts SET score = (SELECT SUM(...))` recompute with a
//! single race-safe `posts.react(...)` call (the vote logic itself is now one
//! statement); htmx partial updates with a full-page redirect for plain form
//! POSTs; session auth.

use autumn_web::Redirect;
use autumn_web::extract::Path;
use autumn_web::htmx::HxRequest;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::{IntoResponse, Response};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

// The reaction trait `#[votable]` emits on `Post`; it is blanket-implemented
// for `PgPostRepository`, which is what brings `react` / `reaction_of` into
// scope here.
use crate::models::{Post, PostReactions as _, Subreddit, User};
use crate::repositories::PgPostRepository;
use crate::schema::{posts, subreddits, users};

use super::layout::vote_controls;

/// Upvote a post (+1). htmx requests get the updated vote-controls fragment;
/// a plain form POST (JavaScript off) is redirected back to the post.
#[post("/posts/{post_id}/upvote")]
pub async fn upvote(
    Path(post_id): Path<i64>,
    hx: HxRequest,
    session: Session,
    csrf: CsrfToken,
    posts_repo: PgPostRepository,
    State(state): State<AppState>,
) -> AutumnResult<Response> {
    cast_vote(post_id, 1, &hx, &session, &csrf, &posts_repo, &state).await
}

/// Downvote a post (-1). htmx requests get the updated vote-controls fragment;
/// a plain form POST (JavaScript off) is redirected back to the post.
#[post("/posts/{post_id}/downvote")]
pub async fn downvote(
    Path(post_id): Path<i64>,
    hx: HxRequest,
    session: Session,
    csrf: CsrfToken,
    posts_repo: PgPostRepository,
    State(state): State<AppState>,
) -> AutumnResult<Response> {
    cast_vote(post_id, -1, &hx, &session, &csrf, &posts_repo, &state).await
}

/// Cast a vote on a post: authenticate, `react`, re-render the control.
///
/// NOTE: no `Db` extractor anywhere in this path. `react()` checks out its
/// *own* pooled connection (it does not join a caller's transaction), so a
/// handler that holds a `Db` across the call needs *two* connections at once.
/// That works in dev and deadlocks under load: once concurrent requests reach
/// the pool size (10 by default), every one of them is holding one connection
/// and waiting for a second that no other request can release.
///
/// The re-rendered control carries the viewer's CSRF token, so the buttons in
/// the swapped-in fragment keep working with JavaScript off.
///
/// Response shape depends on the caller: htmx swaps the returned fragment in
/// place, but a plain `<form method="post">` navigation would render that bare
/// fragment as the whole page — so non-htmx requests get a `303 See Other`
/// back to the post instead, where the updated score and the viewer's own
/// vote are visible on a full page.
#[allow(clippy::too_many_arguments)]
async fn cast_vote(
    post_id: i64,
    value: i16,
    hx: &HxRequest,
    session: &Session,
    csrf: &CsrfToken,
    posts_repo: &PgPostRepository,
    state: &AppState,
) -> AutumnResult<Response> {
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

    // Best-effort from here on: the vote is committed. Failing the request
    // over a lost SSE refresh would invite a retry, and a retried toggle
    // *undoes* the vote — exactly the hazard `react()`'s docs warn about. The
    // viewer still sees the fresh score from the fragment/redirect below;
    // only other subscribers' live tiles go stale until the next update.
    //
    // Render the targeted fragment from the score the broadcast loaded under
    // `BROADCAST_ORDER`, not from `reaction.aggregate`: the broadcast's reload
    // happens after this vote's commit, so it is at least as new — while
    // `reaction.aggregate` is this commit's value and would swap an *older*
    // score back into this viewer's page if a concurrent vote committed and
    // broadcast in between. `reaction.aggregate` is only the fallback when the
    // fan-out failed (nothing newer was published to be overwritten).
    let score = match broadcast_post_update(post_id, state).await {
        Ok(score) => score,
        Err(error) => {
            tracing::warn!(post_id, error = %error, "vote committed but SSE fan-out failed");
            reaction.aggregate
        }
    };

    if hx.is_htmx {
        Ok(vote_controls(post_id, score, reaction.value, Some(csrf)).into_response())
    } else {
        // `/posts/{id}` canonicalizes to the post's full URL (`show_by_id`).
        Ok(Redirect::to(&format!("/posts/{post_id}")).into_response())
    }
}

/// Serializes the reload-and-publish window below. `react()`'s row lock is
/// released at commit, so two near-simultaneous votes could otherwise reload
/// in one order and publish in the other — the *older* score broadcast last,
/// every SSE subscriber showing a regressed aggregate until the next update.
/// Holding this across load→publish makes publish order equal load order, and
/// each load happens after its own vote's commit, so the published score never
/// goes backwards. One global lock is plenty at example scale; a hot
/// production app would shard it per post id.
static BROADCAST_ORDER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Publish the updated post fragment to the global and per-subreddit SSE
/// topics (presentation, not vote logic), returning the score it loaded —
/// read after the caller's commit and under [`BROADCAST_ORDER`], so it is the
/// newest published value and safe for the caller's own fragment to render.
///
/// Pool discipline: `react()` released its connection before returning, and
/// this helper's checkout is short-lived and dropped before the fan-out, so a
/// request never holds two connections at once — which is exactly what keeps
/// the pool from deadlocking under concurrent votes.
async fn broadcast_post_update(post_id: i64, state: &AppState) -> AutumnResult<i64> {
    let _publish_in_load_order = BROADCAST_ORDER.lock().await;
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
    let loaded_score = sse_post.score;
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

    Ok(loaded_score)
}

autumn_web::paths![upvote, downvote];
