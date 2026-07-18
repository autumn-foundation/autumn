//! Comment routes — create and display comments on posts.
//!
//! Demonstrates: #[secured] for write routes, Db extractor for
//! raw queries, `CsrfToken`, htmx integration for inline comment
//! loading.

use autumn_web::extract::Path;
use autumn_web::extract::State;
use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;

use crate::live_events::{
    comment_created_event, publish_stored_live_event_best_effort, store_activity_event_for_state,
};
use crate::models::Comment;
use crate::schema::{comments, posts, subreddits, users};

use autumn_web::Redirect;

#[derive(serde::Deserialize)]
pub struct CommentForm {
    pub body: String,
}

/// Create a new comment on a post.
#[secured]
#[post("/r/{sub_slug}/posts/{post_slug}/comments")]
pub async fn create(
    Path((sub_slug, post_slug)): Path<(String, String)>,
    State(state): State<AppState>,
    session: Session,
    mut db: Db,
    flash: Flash,
    form: Form<CommentForm>,
) -> AutumnResult<Redirect> {
    let user_id: i64 = session
        .get("user_id")
        .await
        .ok_or_else(|| AutumnError::unauthorized_msg("Login required"))?
        .parse()
        .map_err(|_| AutumnError::bad_request_msg("Invalid session"))?;
    let author_username = session
        .get("username")
        .await
        .unwrap_or_else(|| format!("user-{user_id}"));

    let body = form.0.body.trim().to_string();
    if body.is_empty() {
        return Err(AutumnError::unprocessable_msg("Comment cannot be empty"));
    }

    let sub_slug_for_event = sub_slug.clone();
    let post_slug_for_event = post_slug.clone();
    let body_for_insert = body.clone();
    let author_username_for_event = author_username.clone();
    let state_for_event = state.clone();
    let event_id = db
        .tx(|conn| {
            let sub_slug = sub_slug_for_event.clone();
            let post_slug = post_slug_for_event.clone();
            let body = body_for_insert.clone();
            let author_username = author_username_for_event.clone();
            let state = state_for_event.clone();
            async move {
                let post_id: i64 = posts::table
                    .inner_join(subreddits::table.on(posts::subreddit_id.eq(subreddits::id)))
                    .filter(subreddits::slug.eq(&sub_slug))
                    .filter(posts::slug.eq(&post_slug))
                    .select(posts::id)
                    .first(conn)
                    .await
                    .map_err(|_| AutumnError::not_found_msg("Post not found"))?;

                let comment_id: i64 = diesel::insert_into(comments::table)
                    .values((
                        comments::body.eq(&body),
                        comments::author_id.eq(user_id),
                        comments::post_id.eq(post_id),
                        comments::score.eq(1_i64),
                    ))
                    .returning(comments::id)
                    .get_result(conn)
                    .await?;

                diesel::update(posts::table.find(post_id))
                    .set(posts::comment_count.eq(posts::comment_count + 1))
                    .execute(conn)
                    .await?;

                let event = comment_created_event(
                    comment_id,
                    post_id,
                    &post_slug,
                    &sub_slug,
                    &author_username,
                    &body,
                );
                let event_id =
                    store_activity_event_for_state(&state, conn, &sub_slug, &event).await?;

                Ok::<_, AutumnError>(event_id)
            }
            .scope_boxed()
        })
        .await?;
    publish_stored_live_event_best_effort(&state, event_id).await;

    flash.success("Comment posted.").await;
    Ok(Redirect::to(&super::posts::__autumn_path_show(
        &sub_slug, &post_slug,
    )))
}

/// htmx endpoint: load comments for a post (for lazy loading).
#[get("/r/{sub_slug}/posts/{post_slug}/comments")]
pub async fn list_comments(
    Path((sub_slug, post_slug)): Path<(String, String)>,
    mut db: Db,
) -> AutumnResult<Markup> {
    let post_id: i64 = posts::table
        .inner_join(subreddits::table.on(posts::subreddit_id.eq(subreddits::id)))
        .filter(subreddits::slug.eq(&sub_slug))
        .filter(posts::slug.eq(&post_slug))
        .select(posts::id)
        .first(&mut *db)
        .await
        .map_err(|_| AutumnError::not_found_msg("Post not found"))?;

    let post_comments: Vec<(Comment, String)> = comments::table
        .filter(comments::post_id.eq(post_id))
        .filter(comments::parent_id.is_null())
        .inner_join(users::table.on(comments::author_id.eq(users::id)))
        .order(comments::score.desc())
        .select((Comment::as_select(), users::username))
        .load(&mut *db)
        .await?;

    Ok(html! {
        @for (comment, author) in &post_comments {
            div class="bg-white rounded-lg shadow-sm border border-gray-200 p-4" {
                div class="flex items-center gap-2 text-xs text-gray-400 mb-2" {
                    a href=(super::auth::__autumn_path_profile(author))
                       class="font-medium text-gray-600 hover:underline" {
                        "u/" (author)
                    }
                    "\u{2022} " (comment.score) " points"
                }
                div class="text-sm text-gray-700" {
                    @for para in comment.body.split("\n\n") {
                        @if !para.trim().is_empty() {
                            p class="mb-1" { (para.trim()) }
                        }
                    }
                }
            }
        }
        @if post_comments.is_empty() {
            p class="text-gray-400 text-center py-4 text-sm" { "No comments yet." }
        }
    })
}

autumn_web::paths![create, list_comments];
