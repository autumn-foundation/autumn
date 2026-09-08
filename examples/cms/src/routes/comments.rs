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

    // The type's flag as well as the row's column, so a row carrying a stale
    // `comment_status = "open"` on a type that disabled comments neither offers
    // the form nor claims the thread is open.
    let open = post.comment_status == "open" && content::type_supports_comments(&post.post_type);
    let may_comment = open && (viewer.is_some() || settings.allow_guest_comments);
    let moderated = settings.comment_moderation;
    let thread_ctx = ThreadCtx {
        post_id: post.id,
        csrf,
        may_comment,
        guest: viewer.is_none(),
        moderated,
    };

    Ok(html! {
        section class="mt-12 pt-8 border-t border-gray-200" aria-labelledby="comments-heading" {
            h2 #comments-heading class="text-xl font-semibold mb-6" {
                (autumn_web::format::pluralize(post.comment_count, "comment"))
            }

            @if views.is_empty() {
                p class="text-gray-500 text-sm mb-8" { "No comments yet." }
            } @else {
                (render_nodes(&views, 0, &thread_ctx))
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

/// What the thread renderer needs to draw a reply control under each comment.
struct ThreadCtx<'a> {
    post_id: i64,
    csrf: &'a Csrf,
    may_comment: bool,
    /// Whether the viewer is signed out, and so needs name/email fields.
    guest: bool,
    moderated: bool,
}

/// Render the thread as nested ordered lists.
///
/// An `<ol>` per level with `aria-level` makes the nesting available to a
/// screen reader rather than only to a sighted reader's eye for indentation.
///
/// Each comment carries its own reply control. Without one, `reply_to` had no
/// way of being set from a browser: the page rendered a single top-level form
/// that always posted a root comment, so the threading the schema, the depth
/// cap and the renderer all support was reachable only by a hand-written POST.
/// The control is a `<details>` holding a real form with a hidden `reply_to` —
/// no JavaScript, so it works the same everywhere and degrades to an expanded
/// form when scripting is off.
fn render_nodes(
    views: &[autumn_web::widgets::CommentView],
    depth: usize,
    ctx: &ThreadCtx<'_>,
) -> Markup {
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
                    // The server refuses a reply whose parent is already at the
                    // cap, so the control is not offered there — an error page
                    // is a poor way to learn a thread is full.
                    @if ctx.may_comment && depth < content::MAX_COMMENT_DEPTH {
                        details class="mt-2" {
                            summary class="text-xs text-indigo-700 cursor-pointer" {
                                "Reply to " (view.author)
                            }
                            (reply_form(view.id, ctx))
                        }
                    }
                    @if !view.replies.is_empty() {
                        (render_nodes(&view.replies, depth + 1, ctx))
                    }
                }
            }
        }
    }
}

/// The name/email pair a signed-out commenter has to supply.
///
/// Shared by the top-level form and every reply form, so the two cannot drift
/// in what they collect or what they require. `suffix` keeps the `id`
/// attributes unique on a page that renders many of these — a duplicated `id`
/// is what makes a `<label for=…>` point at the wrong field.
fn guest_fields(suffix: &str) -> Markup {
    html! {
        div class="grid grid-cols-1 sm:grid-cols-2 gap-3" {
            div {
                label for=(format!("author_name{suffix}"))
                      class="block text-sm font-medium mb-1" { "Name" }
                input id=(format!("author_name{suffix}")) type="text" name="author_name"
                      required maxlength="80" class="w-full border rounded px-3 py-2";
            }
            div {
                label for=(format!("author_email{suffix}"))
                      class="block text-sm font-medium mb-1" {
                    "Email "
                    span class="text-gray-400 font-normal" { "(not published)" }
                }
                input id=(format!("author_email{suffix}")) type="email" name="author_email"
                      maxlength="254" class="w-full border rounded px-3 py-2";
            }
        }
    }
}

/// The inline form under one comment. Posts to the same endpoint as the
/// top-level form, with `reply_to` naming the parent.
fn reply_form(parent_id: i64, ctx: &ThreadCtx<'_>) -> Markup {
    let suffix = format!("-reply-{parent_id}");
    html! {
        form action=(format!("/comments/{}", ctx.post_id)) method="post"
             class="space-y-3 mt-2 max-w-lg" {
            (ctx.csrf.input())
            input type="hidden" name="reply_to" value=(parent_id);
            @if ctx.moderated && ctx.guest {
                p class="text-xs text-gray-500" {
                    "Replies are reviewed before they appear."
                }
            }
            @if ctx.guest { (guest_fields(&suffix)) }
            div {
                label for=(format!("comment_body{suffix}"))
                      class="block text-sm font-medium mb-1" { "Reply" }
                textarea id=(format!("comment_body{suffix}")) name="body" rows="3" required
                         maxlength="10000" class="w-full border rounded px-3 py-2" {}
            }
            button type="submit"
                   class="px-3 py-1.5 text-sm bg-indigo-600 text-white rounded \
                          hover:bg-indigo-700" {
                "Post reply"
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
            @if viewer.is_none() { (guest_fields("")) }
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
///
/// Throttled per IP. With the shipped defaults — `allow_guest_comments = true`
/// and no global limiter — this is an unauthenticated route that writes a row
/// on every request, and the CSRF token guarding it is reusable. Moderation
/// only changes a row's status, so a request loop grows storage without bound
/// and buries the moderation queue; a per-address bound is the thing that stops
/// it, and it belongs on the route rather than in an operator's checklist.
#[post("/comments/{post_id}")]
#[throttle(limit = 10, per = "1m", key = "ip")]
pub async fn post_comment(
    repos: Repos,
    session: Session,
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
    // `supports_comments` is asked alongside `comment_status`: the flag is the
    // registered type's answer and the column is the row's. A `page` registers
    // `supports_comments: false`, so a row that somehow carries
    // `comment_status = "open"` — a crafted editor submission, an import, a
    // direct write — must still refuse. A signed-in submission is approved
    // immediately, so accepting one made `single_post` start rendering a thread
    // on a type that explicitly disabled comments.
    let unlocked = !post.is_password_protected()
        || session
            .get(&format!("post_unlock_{}", post.id))
            .await
            .is_some_and(|stored| stored == post.password);
    if !post.is_public()
        || !content::is_public_type(&post.post_type)
        || !content::type_supports_comments(&post.post_type)
        || !unlocked
        || post.comment_status != "open"
    {
        return Err(AutumnError::forbidden_msg(
            "Comments are closed on this post",
        ));
    }
    if viewer.is_none() && !settings.allow_guest_comments {
        return Err(AutumnError::unauthorized_msg(
            "You must be signed in to comment",
        ));
    }

    // A `reply_to` naming a comment on a *different* post would graft a subtree
    // onto someone else's thread. This half is a repository read, so it happens
    // before the connection below is taken.
    if let Some(parent_id) = form.reply_to {
        let parent = repos
            .comments
            .find_by_id(parent_id)
            .await?
            .ok_or_else(|| AutumnError::not_found_msg("No such comment"))?;
        if parent.post_id != post.id {
            return Err(AutumnError::unprocessable_msg(
                "That comment is not on this post",
            ));
        }
    }

    // One connection, checked out after every repository read is done and
    // released before the last one. Taking it as a `Db` extractor instead held
    // it from the very start of the handler, across all of the reads above —
    // and each repository call acquires a *second* connection from the same
    // pool. With the shipped `pool_size = 10`, ten concurrent submissions to
    // this unauthenticated route could each hold one slot while waiting for a
    // second that only another of them could release. The throttle raises the
    // bar; it does not remove the shape.
    let mut conn = repos.conn().await?;

    // Enforce the reply-depth cap on the write path, so the renderer never has
    // to defend itself against a chain deep enough to overflow the stack.
    if let Some(parent_id) = form.reply_to
        && content::reply_depth(&mut conn, parent_id).await? > content::MAX_COMMENT_DEPTH
    {
        return Err(AutumnError::unprocessable_msg(
            "This conversation is nested as deeply as it goes",
        ));
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
        &mut conn,
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

    // Released before the permalink read below, so the handler never holds two.
    drop(conn);

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
