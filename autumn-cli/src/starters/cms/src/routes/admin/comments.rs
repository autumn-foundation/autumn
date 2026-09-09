//! The comment moderation queue.
//!
//! This screen is the reason the comment store is this app's rather than the
//! framework's polymorphic `#[commentable]` table: that one models no
//! moderation state and requires every author to hold an account, and
//! WordPress-core parity needs both a queue and guest commenters.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::capabilities::Capability;
use crate::content;
use crate::models::Comment;
use crate::plugins::{Action, do_action};
use crate::repositories::CommentRepository as _;
use crate::require_capability;

use super::super::site::{Csrf, Repos};
use super::layout;

/// The queue's tabs, in the order WordPress shows them.
const QUEUES: &[(&str, &str)] = &[
    ("pending", "Pending"),
    ("approved", "Approved"),
    ("spam", "Spam"),
    ("trash", "Trash"),
];

#[derive(Debug, Default, Deserialize)]
pub struct QueueFilter {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub page: Option<usize>,
}

#[get("/admin/comments")]
pub async fn list(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Query(filter): Query<QueueFilter>,
) -> AutumnResult<Response> {
    let user = require_capability!(repos, session, csrf, Capability::ModerateComments);
    let settings = repos.settings().await?;
    let status = filter
        .status
        .clone()
        .unwrap_or_else(|| "pending".to_owned());

    // Ordered, bounded and joined in SQL. The generated finder loaded every row
    // of the status and sorted in memory, and the post lookup below ran once
    // per comment — so the screen needed to clear a spam flood was the one that
    // stopped working first.
    let per_page = 50_i64;
    let page = i64::try_from(filter.page.unwrap_or(1).clamp(1, 100_000)).unwrap_or(1);
    let entries = {
        let mut conn = repos.conn().await?;
        let rows =
            content::moderation_queue_page(&mut conn, &status, (page - 1) * per_page, per_page)
                .await?;
        let posts = content::posts_for_comments(&mut conn, &rows).await?;
        let entries: Vec<(Comment, Option<crate::models::Post>)> = rows
            .iter()
            .map(|comment| (comment.clone(), posts.get(&comment.post_id).cloned()))
            .collect();
        entries
    };

    let mut counts = Vec::new();
    for (value, label) in QUEUES {
        counts.push((
            *value,
            *label,
            repos
                .comments
                .count_by_status((*value).to_owned())
                .await
                .unwrap_or(0),
        ));
    }

    let body = html! {
        nav aria-label="Comment queues" class="mb-4" {
            ul class="flex gap-4 text-sm" {
                @for (value, label, count) in &counts {
                    li {
                        a href=(format!("/admin/comments?status={value}"))
                          aria-current=[(status == *value).then_some("page")]
                          class=(if status == *value {
                              "font-semibold text-gray-900"
                          } else {
                              "text-gray-500 hover:text-gray-900"
                          }) {
                            (label) " (" (count) ")"
                        }
                    }
                }
            }
        }

        div class="bg-white rounded-lg shadow divide-y divide-gray-100" {
            @for (comment, post) in &entries {
                article class="p-4" {
                    div class="flex items-start justify-between gap-4" {
                        div class="min-w-0" {
                            p class="text-sm" {
                                span class="font-medium" { (comment.display_name()) }
                                @if comment.author_id.is_none() {
                                    span class="ml-2 px-1.5 py-0.5 bg-gray-100 text-gray-600 \
                                                rounded text-xs" { "guest" }
                                }
                                @if !comment.author_email.trim().is_empty() {
                                    span class="text-gray-400 text-xs ml-2" {
                                        (comment.author_email)
                                    }
                                }
                            }
                            p class="text-xs text-gray-400 mt-0.5" {
                                (settings.format_datetime(comment.created_at))
                                @if let Some(post) = post {
                                    " · on " (post.title)
                                }
                            }
                            p class="text-gray-700 mt-2 whitespace-pre-line" { (comment.body) }
                        }
                        div class="flex flex-col gap-1 shrink-0 text-xs" {
                            @for (action, label) in moderation_actions(&comment.status) {
                                form method="post"
                                     action=(format!("/admin/comments/{}/status?to={action}",
                                                      comment.id)) {
                                                          (csrf.input())
                                    button type="submit"
                                           class="px-3 py-1 border rounded bg-white \
                                                  hover:bg-gray-50 w-full" {
                                        (label)
                                    }
                                }
                            }
                            form method="post"
                                 action=(format!("/admin/comments/{}/delete", comment.id)) {
                                     (csrf.input())
                                button type="submit"
                                       class="px-3 py-1 border border-red-200 text-red-700 \
                                              rounded bg-white hover:bg-red-50 w-full" {
                                    "Delete"
                                }
                            }
                        }
                    }
                }
            }
            @if entries.is_empty() {
                p class="p-10 text-center text-gray-400" { "Nothing in this queue." }
            }
        }
    };

    Ok(layout(&user, &csrf, "/admin/comments", "Comments", body).into_response())
}

/// The moves available from a given status. A comment is never offered a
/// transition to the status it already has.
fn moderation_actions(status: &str) -> Vec<(&'static str, &'static str)> {
    match status {
        "pending" => vec![
            ("approved", "Approve"),
            ("spam", "Spam"),
            ("trash", "Trash"),
        ],
        "approved" => vec![
            ("pending", "Unapprove"),
            ("spam", "Spam"),
            ("trash", "Trash"),
        ],
        "spam" => vec![("approved", "Not spam"), ("trash", "Trash")],
        _ => vec![("approved", "Approve"), ("pending", "Restore")],
    }
}

#[derive(Deserialize)]
pub struct ModerateQuery {
    pub to: String,
}

#[post("/admin/comments/{id}/status")]
pub async fn moderate(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path(id): Path<i64>,
    Query(query): Query<ModerateQuery>,
) -> AutumnResult<Response> {
    let _user = require_capability!(repos, session, csrf, Capability::ModerateComments);

    // The approved-comment counter moves inside the same transaction as the
    // status change, so a reader never sees a count that disagrees with the
    // thread.
    let mut conn = repos.conn().await?;
    let (updated, changed) = content::moderate_comment(&mut conn, id, &query.to).await?;
    // Only on a real transition. A retried or double-clicked approval returns
    // the unchanged row, and firing the action again would have plugins send a
    // second notification for a request that moved nothing.
    if changed && updated.status == "approved" {
        do_action(Action::CommentApproved, updated.id);
    }

    Ok(Redirect::to(&format!("/admin/comments?status={}", query.to)).into_response())
}

#[post("/admin/comments/{id}/delete")]
pub async fn delete(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Path(id): Path<i64>,
) -> AutumnResult<Response> {
    let _user = require_capability!(repos, session, csrf, Capability::ModerateComments);

    // Delete and recount together. Decrementing for this row alone was wrong:
    // `comments.parent_id` cascades, so deleting an approved comment that has
    // approved replies removes the whole subtree while the counter loses only
    // one — leaving every deleted reply permanently in the post's displayed
    // count. `content::delete_comment` recomputes from ground truth in the same
    // transaction, so it is right whatever the cascade took.
    let mut conn = repos.conn().await?;
    content::delete_comment(&mut conn, id).await?;

    Ok(Redirect::to("/admin/comments?status=trash").into_response())
}
