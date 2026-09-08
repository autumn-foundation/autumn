//! The public comment form and thread.
//!
//! The moderation half lives in [`crate::routes::admin::comments`]; this is the
//! reader-facing side.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::content;
use crate::models::{Comment, NewComment, Post};
use crate::plugins::{Action, do_action};
use crate::repositories::{CommentRepository as _, PostRepository as _, UserRepository as _};

use super::site::{Csrf, Repos};

/// What the reader submits.
#[derive(Deserialize)]
pub struct CommentForm {
    pub body: String,
    /// Present only for guests; a signed-in commenter's identity comes from
    /// the session and these are ignored.
    #[serde(default)]
    pub author_name: Option<String>,
    #[serde(default)]
    pub author_email: Option<String>,
    #[serde(default)]
    pub author_url: Option<String>,
    /// The comment being replied to, if any.
    #[serde(default)]
    pub reply_to: Option<i64>,
}

/// Load and render a post's approved comment thread plus its reply form.
pub async fn render_thread(
    repos: &Repos,
    session: &Session,
    csrf: &Csrf,
    post: &Post,
) -> AutumnResult<Markup> {
    let settings = repos.settings().await?;
    let viewer = repos.current_user(session).await?;

    // Status filter and bound both in SQL. The generated `find_by_post_id`
    // returns every row of every status, so filtering afterwards made a public
    // page view cost the whole moderation queue — which, with guest comments
    // on, anyone can grow.
    let mut conn = repos.conn().await?;
    let total = content::approved_comment_count(&mut conn, post.id).await?;
    let rows: Vec<Comment> =
        content::approved_comments_page(&mut conn, post.id, 0, content::MAX_THREAD_COMMENTS)
            .await?;
    drop(conn);
    let truncated = total > i64::try_from(rows.len()).unwrap_or(i64::MAX);

    // Resolve account-backed display names once. A registered commenter renders
    // under their *current* public name; a guest renders under the name they
    // gave at the time.
    let mut names: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    for id in rows.iter().filter_map(|c| c.author_id) {
        if let std::collections::hash_map::Entry::Vacant(slot) = names.entry(id)
            && let Some(user) = repos.users.find_by_id(id).await.ok().flatten()
        {
            slot.insert(user.public_name().to_owned());
        }
    }
    let name_of = |comment: &Comment| -> String {
        comment
            .author_id
            .and_then(|id| names.get(&id).cloned())
            .unwrap_or_else(|| comment.display_name().to_owned())
    };

    let mut sorted = rows;
    sorted.sort_by_key(|c| (c.created_at, c.id));
    let thread = content::assemble_thread(&sorted, None, 0, &name_of);
    let views = content::to_comment_views(&thread);

    let open = post.comment_status == "open";
    let may_comment = open && (viewer.is_some() || settings.allow_guest_comments);
    let moderated = settings.comment_moderation;

    Ok(html! {
        section class="mt-12 pt-8 border-t border-gray-200" aria-labelledby="comments-heading" {
            h2 #comments-heading class="text-xl font-semibold mb-6" {
                (autumn_web::format::pluralize(post.comment_count, "comment"))
            }

            @if views.is_empty() {
                p class="text-gray-500 text-sm mb-8" { "No comments yet." }
            } @else {
                (render_nodes(&views, 0))
            }

            @if truncated {
                p class="text-sm text-gray-500 mb-8" {
                    "Showing the first "
                    (autumn_web::format::pluralize(
                        i64::try_from(sorted.len()).unwrap_or(0), "comment"))
                    " of " (total) "."
                }
            }

            @if may_comment {
                (comment_form(post, viewer.as_ref(), moderated, csrf))
            } @else if !open {
                p class="text-sm text-gray-500" { "Comments are closed on this post." }
            } @else {
                p class="text-sm text-gray-500" {
                    a href="/login" class="text-indigo-700 hover:underline" { "Sign in" }
                    " to leave a comment."
                }
            }
        }
    })
}

/// Render the thread as nested ordered lists.
///
/// An `<ol>` per level with `aria-level` makes the nesting available to a
/// screen reader rather than only to a sighted reader's eye for indentation.
fn render_nodes(views: &[autumn_web::widgets::CommentView], depth: usize) -> Markup {
    html! {
        ol class=(if depth == 0 { "space-y-6 mb-8" } else { "space-y-4 mt-4 ml-6 border-l border-gray-100 pl-4" })
           role="list" {
            @for view in views {
                li #(format!("comment-{}", view.id)) aria-level=((depth + 1).to_string()) {
                    article {
                        p class="text-sm" {
                            span class="font-medium text-gray-900" { (view.author) }
                            @if let Some(datetime) = &view.datetime {
                                span class="text-gray-400" { " · " }
                                time datetime=(datetime) class="text-xs text-gray-400" {
                                    (view.timestamp)
                                }
                            }
                        }
                        @for paragraph in view.body.split("\n\n") {
                            @if !paragraph.trim().is_empty() {
                                p class="text-gray-700 mt-1" { (paragraph) }
                            }
                        }
                    }
                    @if !view.replies.is_empty() {
                        (render_nodes(&view.replies, depth + 1))
                    }
                }
            }
        }
    }
}

fn comment_form(
    post: &Post,
    viewer: Option<&crate::models::User>,
    moderated: bool,
    csrf: &Csrf,
) -> Markup {
    html! {
        form action=(format!("/comments/{}", post.id)) method="post"
             class="space-y-3 max-w-lg" {
            (csrf.input())
            h3 class="font-semibold" { "Leave a comment" }
            @if moderated {
                p class="text-xs text-gray-500" {
                    "Comments are reviewed before they appear."
                }
            }
            @if viewer.is_none() {
                div class="grid grid-cols-1 sm:grid-cols-2 gap-3" {
                    div {
                        label for="author_name" class="block text-sm font-medium mb-1" { "Name" }
                        input #author_name type="text" name="author_name" required maxlength="80"
                              class="w-full border rounded px-3 py-2";
                    }
                    div {
                        label for="author_email" class="block text-sm font-medium mb-1" {
                            "Email "
                            span class="text-gray-400 font-normal" { "(not published)" }
                        }
                        input #author_email type="email" name="author_email" maxlength="254"
                              class="w-full border rounded px-3 py-2";
                    }
                }
            }
            div {
                label for="comment_body" class="block text-sm font-medium mb-1" { "Comment" }
                textarea #comment_body name="body" rows="4" required maxlength="10000"
                         class="w-full border rounded px-3 py-2" {}
            }
            button type="submit"
                   class="px-4 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700" {
                "Post comment"
            }
        }
    }
}

/// Accept a comment.
#[post("/comments/{post_id}")]
pub async fn post_comment(
    repos: Repos,
    session: Session,
    mut db: autumn_web::Db,
    Path(post_id): Path<i64>,
    Form(form): Form<CommentForm>,
) -> AutumnResult<Response> {
    let settings = repos.settings().await?;
    let viewer = repos.current_user(&session).await?;

    let post = repos
        .posts
        .find_by_id(post_id)
        .await?
        .ok_or_else(|| AutumnError::not_found_msg("No such post"))?;

    // Never accept a comment on content the commenter cannot see, or on a post
    // whose author closed the thread. "Cannot see" is the same three questions
    // the render path asks — status, registered type, and the password gate —
    // not just the first. A signed-in caller is assigned `approved`
    // immediately, so accepting one here would inject visible discussion into
    // a thread the front end deliberately withholds.
    let type_is_public = crate::content_types::find_post_type(&post.post_type)
        .is_some_and(|registered| registered.public);
    let unlocked = !post.is_password_protected()
        || session
            .get(&format!("post_unlock_{}", post.id))
            .await
            .is_some_and(|stored| stored == post.password);
    if !post.is_public() || !type_is_public || !unlocked || post.comment_status != "open" {
        return Err(AutumnError::forbidden_msg(
            "Comments are closed on this post",
        ));
    }
    if viewer.is_none() && !settings.allow_guest_comments {
        return Err(AutumnError::unauthorized_msg(
            "You must be signed in to comment",
        ));
    }

    // Enforce the reply-depth cap on the write path, so the renderer never has
    // to defend itself against a chain deep enough to overflow the stack.
    if let Some(parent_id) = form.reply_to {
        let parent = repos
            .comments
            .find_by_id(parent_id)
            .await?
            .ok_or_else(|| AutumnError::not_found_msg("No such comment"))?;
        // A `reply_to` naming a comment on a *different* post would graft a
        // subtree onto someone else's thread.
        if parent.post_id != post.id {
            return Err(AutumnError::unprocessable_msg(
                "That comment is not on this post",
            ));
        }
        if content::reply_depth(&mut db, parent_id).await? > content::MAX_COMMENT_DEPTH {
            return Err(AutumnError::unprocessable_msg(
                "This conversation is nested as deeply as it goes",
            ));
        }
    }

    // A signed-in commenter's identity comes from the session, never from the
    // form — otherwise anyone could post under any name.
    let (author_id, author_name, author_email) = match &viewer {
        Some(user) => (
            Some(user.id),
            user.public_name().to_owned(),
            user.email.clone(),
        ),
        None => (
            None,
            form.author_name
                .clone()
                .unwrap_or_default()
                .trim()
                .to_owned(),
            form.author_email
                .clone()
                .unwrap_or_default()
                .trim()
                .to_owned(),
        ),
    };

    // A signed-in commenter skips the queue; a guest is held when moderation is
    // on. This is WordPress's default discussion setting.
    let status = if viewer.is_some() || !settings.comment_moderation {
        "approved"
    } else {
        "pending"
    };

    let created = content::create_comment(
        &mut db,
        NewComment {
            post_id: post.id,
            parent_id: form.reply_to,
            author_id,
            author_name,
            author_email,
            author_url: form.author_url.unwrap_or_default().trim().to_owned(),
            author_ip: String::new(),
            body: form.body.clone(),
            status: status.to_owned(),
        },
    )
    .await?;

    do_action(Action::CommentPosted, created.id);
    if status == "approved" {
        do_action(Action::CommentApproved, created.id);
    }

    let permalink = repos.permalink(&post, &settings).await?;
    let destination = if status == "approved" {
        format!("{permalink}#comment-{}", created.id)
    } else {
        format!("{permalink}?moderated=1")
    };
    Ok(Redirect::to(&destination).into_response())
}

/// The banner shown after a held comment is submitted.
///
/// Rendered from the `?moderated=1` marker the redirect above sets, so the
/// reader is told their comment was received rather than left staring at an
/// unchanged page wondering whether the button worked.
#[must_use]
pub fn unapproved_notice(moderated: bool) -> Markup {
    html! {
        @if moderated {
            p class="mb-6 px-4 py-3 rounded bg-amber-50 text-amber-900 text-sm" role="status" {
                "Thanks — your comment is awaiting moderation."
            }
        }
    }
}
