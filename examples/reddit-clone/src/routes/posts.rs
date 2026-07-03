//! Post routes — front page, submit, view, edit, delete.
//!
//! Demonstrates: CRUD with the Db extractor, `CsrfToken` in forms,
//! #[secured] for write operations, htmx for voting and deletion,
//! Maud templates with Tailwind CSS, and feature-flag fragment gating
//! via the `Flags` extractor.

use std::collections::HashMap;

use autumn_web::experiments::Experiments;
use autumn_web::extract::Path;
use autumn_web::extract::State;
use autumn_web::feature_flags::Flags;
use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;

use crate::jobs::{PostPublicationArgs, PostPublicationJob};
use crate::models::{
    Comment, CommentAssociations, NewTag, Post, PostAssociations, PostTagsMutations, Subreddit, Tag,
};
use crate::repositories::{PgPostRepository, PostRepository};
use crate::schema::{posts, subreddits, tags};
use crate::slugify::slugify;

fn posts_per_page() -> i64 {
    crate::config_svc()
        .get("posts_per_page")
        .ok()
        .and_then(|v| v.as_int())
        .unwrap_or(25)
}

use super::layout::{layout, time_ago, vote_controls};

// ── Front page — hot posts across all subreddits ───────────────

#[get("/")]
pub async fn front_page(
    session: Session,
    csrf: CsrfToken,
    mut db: Db,
    repo: PgPostRepository,
    flags: Flags,
    exps: Experiments,
    flash: Flash,
) -> AutumnResult<Markup> {
    let current_user = session.get("username").await;

    // A/B experiment: compact list (control) vs. card layout (treatment).
    // The Experiments extractor resolves the actor from the session automatically
    // (logged-in users → user_id; anonymous → stable per-session key).
    let compact_layout = exps.assign("feed_layout").unwrap_or_default() == "compact";

    // Hot posts across all subreddits. Instead of a hand-written two-way join,
    // load the page of posts, then `preload` their author + subreddit. This is
    // `1 + K` queries (here: posts, authors, subreddits = 3) regardless of how
    // many posts are on the page — no N+1.
    let hot_posts: Vec<Post> = posts::table
        .order(posts::hot_rank.desc())
        .limit(posts_per_page())
        .select(Post::as_select())
        .load(&mut *db)
        .await?;
    // Release the base-query connection before `preload` checks one out, so the
    // two never contend on a single-connection pool. The base rows were read
    // from the primary via `Db`, so pin the preload to the primary too
    // (`on_primary`) — otherwise, under replica lag, an author/subreddit just
    // written may be missing on the replica and the post would be skipped.
    drop(db);
    let hot_posts = repo
        .on_primary()
        .preload(hot_posts, Post::preload().author().subreddit())
        .await?;

    // Consume the flash only after all fallible work, so a mid-handler error
    // doesn't drop the one-shot message before it is shown.
    let flash_html = flash.render().await;
    Ok(layout(
        "Front Page",
        current_user.as_deref(),
        Some(csrf.token()),
        html! {
            (flash_html)
            // Fragment gating: banner visible only to users in the new_ui_preview rollout cohort.
            @if flags.enabled("new_ui_preview") {
                div class="mb-4 px-4 py-2 bg-indigo-50 border border-indigo-200 rounded-lg \
                           text-sm text-indigo-700 flex items-center gap-2" {
                    span class="font-semibold" { "New UI Preview" }
                    "You're in the early-access cohort. "
                    a href="#" class="underline hover:text-indigo-900" { "Send feedback" }
                }
            }

            // Sort tabs
            div class="flex items-center gap-4 mb-4 text-sm" {
                span class="px-3 py-1.5 bg-orange-100 text-orange-700 rounded-full font-medium" {
                    "Hot"
                }
                a href="/?sort=new" class="text-gray-500 hover:text-orange-600 px-3 py-1.5" {
                    "New"
                }
            }

            // Post list — layout variant determined by the feed_layout A/B
            // experiment. compact (control): dense rows; card (treatment):
            // bordered cards with vote controls. Author + subreddit come from
            // the preloaded record's typed accessors (`?`-free in templates:
            // treat a missing preload as "absent").
            @if compact_layout {
                ul id="posts-list" class="divide-y divide-gray-100 posts-feed-compact"
                    hx-ext="sse" sse-connect="/posts/stream" sse-swap="message" hx-swap="none" {
                    @for post in &hot_posts {
                        @let author = post.author().ok().flatten();
                        @let sub = post.subreddit().ok().flatten();
                        @if let Some(sub) = sub {
                            li id=(format!("post-{}", post.id)) class="posts-feed-item transition-all" {
                                div class="posts-feed-compact-version flex items-center gap-3 py-2 px-2 hover:bg-gray-50 transition-colors" {
                                    span class="text-sm font-semibold text-gray-500 w-8 text-right shrink-0" {
                                        (post.score)
                                    }
                                    div class="flex-1 min-w-0" {
                                        a href=(paths::show(&sub.slug, &post.slug))
                                           class="text-sm font-medium text-gray-900 hover:text-orange-600 \
                                                  line-clamp-1" {
                                            (post.title)
                                        }
                                        div class="text-xs text-gray-400" {
                                            a href=(super::subreddits::__autumn_path_show(&sub.slug))
                                               class="text-gray-500 hover:underline" {
                                                "r/" (sub.name)
                                            }
                                            @if let Some(author) = author {
                                                " \u{2022} "
                                                a href=(super::auth::__autumn_path_profile(&author.username))
                                                   class="text-gray-500 hover:underline" { "u/" (author.username) }
                                            }
                                            " \u{2022} " (time_ago(&post.created_at))
                                            " \u{2022} "
                                            a href=(paths::show(&sub.slug, &post.slug))
                                               class="text-gray-500 hover:text-orange-600" {
                                                (post.comment_count) " comments"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    @if hot_posts.is_empty() {
                        p class="text-gray-400 text-center py-8 text-sm" { "Nothing here yet!" }
                    }
                }
            } @else {
                ul id="posts-list" class="space-y-2"
                    hx-ext="sse" sse-connect="/posts/stream" sse-swap="message" hx-swap="none" {
                    @for post in &hot_posts {
                        @let author = post.author().ok().flatten();
                        @let sub = post.subreddit().ok().flatten();
                        @if let Some(sub) = sub {
                            li id=(format!("post-{}", post.id)) class="posts-feed-item transition-all" {
                                div class="posts-feed-card-version bg-white rounded-lg shadow-sm border border-gray-200 hover:border-orange-300 transition-colors" {
                                    div class="flex items-start gap-3 p-4" {
                                        (vote_controls(post.id, post.score))
                                        div class="flex-1 min-w-0" {
                                            a href=(paths::show(&sub.slug, &post.slug))
                                               class="text-lg font-medium text-gray-900 hover:text-orange-600 line-clamp-2" {
                                                (post.title)
                                            }
                                            div class="text-xs text-gray-400 mt-1" {
                                                a href=(super::subreddits::__autumn_path_show(&sub.slug))
                                                   class="font-medium text-gray-600 hover:underline" {
                                                    "r/" (sub.name)
                                                }
                                                @if let Some(author) = author {
                                                    " \u{2022} posted by "
                                                    a href=(super::auth::__autumn_path_profile(&author.username))
                                                       class="text-gray-500 hover:underline" {
                                                        "u/" (author.username)
                                                    }
                                                }
                                                " " (time_ago(&post.created_at))
                                                " \u{2022} "
                                                a href=(paths::show(&sub.slug, &post.slug))
                                                   class="text-gray-500 hover:text-orange-600" {
                                                    (post.comment_count) " comments"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    @if hot_posts.is_empty() {
                        div class="text-center py-16" {
                            p class="text-gray-400 text-lg mb-4" { "Nothing here yet!" }
                            p class="text-gray-400 text-sm" {
                                "Be the first to "
                                a href="/r" class="text-orange-600 hover:underline" {
                                    "join a community"
                                }
                                " and post something."
                            }
                        }
                    }
                }
            }
        },
    ))
}

// ── Submit form (global — pick subreddit) ──────────────────────

#[secured]
#[get("/submit")]
pub async fn submit_form(session: Session, csrf: CsrfToken, mut db: Db) -> AutumnResult<Markup> {
    let current_user = session.get("username").await;

    let subs: Vec<Subreddit> = subreddits::table
        .order(subreddits::name.asc())
        .select(Subreddit::as_select())
        .load(&mut *db)
        .await?;

    Ok(layout(
        "Submit Post",
        current_user.as_deref(),
        Some(csrf.token()),
        html! {
            div class="max-w-2xl mx-auto" {
                h1 class="text-2xl font-bold mb-6" { "Create a Post" }
                form action=(paths::submit()) method="post"
                     class="space-y-4 bg-white rounded-lg shadow p-6" {
                    input type="hidden" name="_csrf" value=(csrf.token());
                    div {
                        label for="subreddit_id" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Community"
                        }
                        select id="subreddit_id" name="subreddit_id" required
                               class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                      focus:outline-none focus:ring-2 focus:ring-orange-400" {
                            option value="" disabled selected { "Choose a community..." }
                            @for sub in &subs {
                                option value=(sub.id) { "r/" (sub.name) }
                            }
                        }
                    }
                    div {
                        label for="title" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Title"
                        }
                        input type="text" id="title" name="title" required
                              maxlength="300"
                              placeholder="An interesting title"
                              class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                     focus:outline-none focus:ring-2 focus:ring-orange-400";
                    }
                    div {
                        label for="url" class="block text-sm font-medium text-gray-700 mb-1" {
                            "URL " span class="text-gray-400" { "(optional)" }
                        }
                        input type="url" id="url" name="url"
                              placeholder="https://example.com"
                              class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                     focus:outline-none focus:ring-2 focus:ring-orange-400";
                    }
                    div {
                        label for="body" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Text " span class="text-gray-400" { "(optional for link posts)" }
                        }
                        textarea id="body" name="body" rows="8"
                                 placeholder="What's on your mind?"
                                 class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                        focus:outline-none focus:ring-2 focus:ring-orange-400" {}
                    }
                    button type="submit"
                           class="w-full bg-orange-500 text-white py-2 rounded font-medium \
                                  hover:bg-orange-600 transition-colors" {
                        "Post"
                    }
                }
            }
        },
    ))
}

/// Submit form for a specific subreddit
#[secured]
#[get("/r/{slug}/submit")]
pub async fn submit_to_sub_form(
    Path(slug): Path<String>,
    session: Session,
    csrf: CsrfToken,
    mut db: Db,
) -> AutumnResult<Markup> {
    let current_user = session.get("username").await;

    let sub: Subreddit = subreddits::table
        .filter(subreddits::slug.eq(&slug))
        .select(Subreddit::as_select())
        .first(&mut *db)
        .await
        .map_err(|_| AutumnError::not_found_msg(format!("r/{slug} not found")))?;

    Ok(layout(
        &format!("Submit to r/{}", sub.name),
        current_user.as_deref(),
        Some(csrf.token()),
        html! {
            div class="max-w-2xl mx-auto" {
                h1 class="text-2xl font-bold mb-6" {
                    "Post to "
                    span class="text-orange-600" { "r/" (sub.name) }
                }
                form action=(paths::submit()) method="post"
                     class="space-y-4 bg-white rounded-lg shadow p-6" {
                    input type="hidden" name="_csrf" value=(csrf.token());
                    input type="hidden" name="subreddit_id" value=(sub.id);
                    div {
                        label for="title" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Title"
                        }
                        input type="text" id="title" name="title" required
                              maxlength="300"
                              placeholder="An interesting title"
                              class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                     focus:outline-none focus:ring-2 focus:ring-orange-400";
                    }
                    div {
                        label for="url" class="block text-sm font-medium text-gray-700 mb-1" {
                            "URL " span class="text-gray-400" { "(optional)" }
                        }
                        input type="url" id="url" name="url"
                              placeholder="https://example.com"
                              class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                     focus:outline-none focus:ring-2 focus:ring-orange-400";
                    }
                    div {
                        label for="body" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Text"
                        }
                        textarea id="body" name="body" rows="8"
                                 placeholder="What's on your mind?"
                                 class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                        focus:outline-none focus:ring-2 focus:ring-orange-400" {}
                    }
                    button type="submit"
                           class="w-full bg-orange-500 text-white py-2 rounded font-medium \
                                  hover:bg-orange-600 transition-colors" {
                        "Post"
                    }
                }
            }
        },
    ))
}

#[derive(serde::Deserialize)]
pub struct SubmitPostForm {
    pub subreddit_id: i64,
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub body: String,
}

#[secured]
#[post("/submit")]
pub async fn submit(
    State(state): State<AppState>,
    session: Session,
    mut db: Db,
    _repo: PgPostRepository,
    flash: Flash,
    form: Form<SubmitPostForm>,
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

    let title = form.0.title.trim().to_string();
    if title.is_empty() || title.len() > 300 {
        return Err(AutumnError::unprocessable_msg(
            "Title must be 1-300 characters",
        ));
    }

    let base_slug = slugify(&title);
    if base_slug.is_empty() {
        return Err(AutumnError::unprocessable_msg(
            "Title must contain at least one letter or number",
        ));
    }
    let url = if form.0.url.trim().is_empty() {
        None
    } else {
        Some(form.0.url.trim().to_string())
    };

    // Look up the subreddit slug for redirect
    let sub: Subreddit = subreddits::table
        .find(form.0.subreddit_id)
        .select(Subreddit::as_select())
        .first(&mut *db)
        .await
        .map_err(|_| AutumnError::not_found_msg("Subreddit not found"))?;

    // Ensure unique slug within this subreddit by appending a suffix
    let slug = unique_slug(&base_slug, form.0.subreddit_id, &mut db).await?;

    let body = form.0.body.trim().to_string();
    let subreddit_id = form.0.subreddit_id;
    let subreddit_slug = sub.slug.clone();

    let new_post = crate::models::NewPost {
        title: title.clone(),
        slug: slug.clone(),
        body: body.clone(),
        url,
        author_id: user_id,
        subreddit_id,
    };

    let subreddit_slug_for_job = subreddit_slug.clone();
    let author_username_for_job = author_username.clone();
    let post: Post = db
        .tx(move |conn| {
            let new_post = new_post.clone();
            let subreddit_slug = subreddit_slug_for_job.clone();
            let author_username = author_username_for_job.clone();
            async move {
                let post: Post = diesel::insert_into(posts::table)
                    .values(&new_post)
                    .get_result(conn)
                    .await?;

                let post_id = post.id;
                diesel::insert_into(crate::schema::votes::table)
                    .values((
                        crate::schema::votes::user_id.eq(user_id),
                        crate::schema::votes::post_id.eq(post_id),
                        crate::schema::votes::value.eq(1_i16),
                    ))
                    .execute(conn)
                    .await?;

                diesel::update(posts::table.find(post_id))
                    .set(posts::score.eq(1_i64))
                    .execute(conn)
                    .await?;

                let post: Post = posts::table.find(post_id).first(conn).await?;

                // Enqueue the publication job inside the transaction
                let payload = serde_json::to_value(PostPublicationArgs::new(
                    post.id,
                    &post.title,
                    &post.slug,
                    &subreddit_slug,
                    &author_username,
                ))
                .unwrap();
                autumn_web::job::enqueue_on_conn(PostPublicationJob::NAME, &payload, conn).await?;

                Ok::<_, AutumnError>(post)
            }
            .scope_boxed()
        })
        .await?;

    let lookup = crate::repositories::PostRelationsLookup {
        author_name: author_username.clone(),
        sub_name: sub.name.clone(),
        sub_slug: sub.slug.clone(),
    };

    let sse_state = state.clone();
    let sse_post = post.clone();
    let sse_sub_slug = subreddit_slug.clone();
    crate::repositories::CURRENT_POST_RELATIONS
        .scope(lookup, async move {
            let _ = sse_state.broadcast().publish_oob(
                "posts",
                &sse_post.dom_id(),
                &autumn_web::htmx::OobSwap::OuterHTML,
                &sse_post.render_fragment(),
            );

            let _ = sse_state.broadcast().publish_oob(
                &format!("posts:r/{}", sse_sub_slug),
                &sse_post.dom_id(),
                &autumn_web::htmx::OobSwap::Target(
                    autumn_web::htmx::OobMethod::BeforeEnd,
                    "#posts-list".to_string(),
                ),
                &sse_post.render_fragment(),
            );
        })
        .await;

    flash.success("Post created.").await;
    Ok(Redirect::to(&super::subreddits::__autumn_path_show(
        &sub.slug,
    )))
}

// ── Short-form permalink for live-broadcast fragments ──────────

/// Redirects `/posts/{post_id}` to the canonical `/r/{sub_slug}/posts/{post_slug}`.
/// Used by live OOB fragments that only have a post id in scope.
#[get("/posts/{post_id}")]
pub async fn show_by_id(Path(post_id): Path<i64>, mut db: Db) -> AutumnResult<Redirect> {
    let (post_slug, subreddit_id): (String, i64) = posts::table
        .find(post_id)
        .select((posts::slug, posts::subreddit_id))
        .first(&mut *db)
        .await
        .map_err(|_| AutumnError::not_found_msg("Post not found"))?;
    let sub_slug: String = subreddits::table
        .find(subreddit_id)
        .select(subreddits::slug)
        .first(&mut *db)
        .await
        .map_err(|_| AutumnError::not_found_msg("Subreddit not found"))?;
    Ok(Redirect::to(&format!("/r/{sub_slug}/posts/{post_slug}")))
}

// ── View single post with comments ─────────────────────────────

#[allow(clippy::too_many_lines)] // Template-heavy function
#[get("/r/{sub_slug}/posts/{post_slug}")]
pub async fn show(
    Path((sub_slug, post_slug)): Path<(String, String)>,
    session: Session,
    csrf: CsrfToken,
    mut db: Db,
    repo: PgPostRepository,
    flags: Flags,
    flash: Flash,
) -> AutumnResult<Markup> {
    let current_user = session.get("username").await;
    let current_user_id = session.get("user_id").await;

    let sub: Subreddit = subreddits::table
        .filter(subreddits::slug.eq(&sub_slug))
        .select(Subreddit::as_select())
        .first(&mut *db)
        .await
        .map_err(|_| AutumnError::not_found_msg(format!("r/{sub_slug} not found")))?;

    let post: Post = posts::table
        .filter(posts::slug.eq(&post_slug))
        .filter(posts::subreddit_id.eq(sub.id))
        .select(Post::as_select())
        .first(&mut *db)
        .await
        .map_err(|_| AutumnError::not_found_msg("Post not found"))?;

    // Release the base-query connection before `preload` checks one out. The
    // base rows came from the primary via `Db`, so pin the preload to the
    // primary too (`on_primary`) to keep both reads on one consistent role.
    drop(db);

    // Eager-load the post's author, its comments (each with their author),
    // and its tags (#1324, many-to-many through `post_tags`) -- replacing
    // the per-row author lookup + hand-written comment/author join, and what
    // would otherwise be a hand-rolled `post_tags` join query. For a post
    // with N comments and M tags this is a fixed 2 extra queries
    // (post.author, comments) + 1 (comments.author) + 1 (post.tags) = at
    // most 4 here, never `3 + N + M`.
    let mut loaded = repo
        .on_primary()
        .preload(
            vec![post],
            Post::preload()
                .author()
                .comments_with(Comment::preload().author())
                .tags(),
        )
        .await?;
    let post = loaded.remove(0);
    let author = post.author()?;
    let post_tags = post.tags()?;

    // Show top-level comments (parent_id IS NULL), highest score first.
    let mut post_comments: Vec<&autumn_web::preload::Preloaded<Comment>> = post
        .comments()?
        .iter()
        .filter(|c| c.parent_id.is_none())
        .collect();
    post_comments.sort_by_key(|c| std::cmp::Reverse(c.score));

    let is_author = current_user_id
        .as_ref()
        .and_then(|id| id.parse::<i64>().ok())
        .is_some_and(|id| id == post.author_id);

    // Consume the flash only after all fallible work above.
    let flash_html = flash.render().await;
    Ok(layout(
        &post.title,
        current_user.as_deref(),
        Some(csrf.token()),
        html! {
            (flash_html)
            // Breadcrumbs
            div class="text-sm text-gray-500 mb-4" {
                a href=(super::subreddits::__autumn_path_show(&sub.slug)) class="hover:text-orange-600" {
                    "r/" (sub.name)
                }
                " \u{203A} Post"
            }

            // Post card
            div class="bg-white rounded-lg shadow-sm border border-gray-200 p-6 mb-6" {
                div class="flex items-start gap-4" {
                    (vote_controls(post.id, post.score))
                    div class="flex-1" {
                        h1 class="text-2xl font-bold text-gray-900 mb-2" { (post.title) }
                        div class="text-xs text-gray-400 mb-4" {
                            @if let Some(author) = author {
                                "posted by "
                                a href=(super::auth::__autumn_path_profile(&author.username))
                                   class="text-gray-500 hover:underline" {
                                    "u/" (author.username)
                                }
                            }
                            " " (time_ago(&post.created_at))
                        }
                        @if let Some(ref url) = post.url {
                            a href=(url) target="_blank" rel="noopener noreferrer"
                               class="text-blue-600 hover:underline text-sm mb-3 block" {
                                (url)
                                " \u{2197}"
                            }
                        }
                        @if !post.body.is_empty() {
                            div class="prose max-w-none text-gray-700" {
                                @for para in post.body.split("\n\n") {
                                    @if !para.trim().is_empty() {
                                        p { (para.trim()) }
                                    }
                                }
                            }
                        }
                        // Preloaded many-to-many tags (#1324): `post.tags()`
                        // reads the batched `post_tags` join loaded above, no
                        // per-tag query.
                        @if !post_tags.is_empty() {
                            div class="flex flex-wrap gap-2 mt-3" {
                                @for tag in &post_tags {
                                    span class="px-2 py-0.5 rounded-full bg-orange-50 text-orange-700 text-xs" {
                                        "#" (tag.slug)
                                    }
                                }
                            }
                        }
                        @if is_author {
                            div class="flex gap-3 mt-4 pt-4 border-t border-gray-100 text-sm" {
                                a href=(paths::edit_form(&sub.slug, &post.slug))
                                   class="text-gray-500 hover:text-orange-600" { "Edit" }
                                button
                                    hx-delete=(paths::delete_post(&sub.slug, &post.slug))
                                    hx-confirm="Delete this post? This cannot be undone."
                                    class="text-red-500 hover:text-red-700 cursor-pointer" {
                                    "Delete"
                                }
                            }
                            form action=(paths::manage_tags(&sub.slug, &post.slug)) method="post"
                                 class="flex items-center gap-2 mt-3 text-sm" {
                                input type="hidden" name="_csrf" value=(csrf.token());
                                input type="text" name="tags"
                                      value=(post_tags.iter().map(|t| t.slug.clone()).collect::<Vec<_>>().join(", "))
                                      placeholder="tags, comma separated"
                                      class="flex-1 border border-gray-300 rounded px-2 py-1 text-xs \
                                             focus:outline-none focus:ring-2 focus:ring-orange-400" {}
                                button type="submit"
                                       class="px-3 py-1 bg-gray-100 text-gray-700 rounded text-xs \
                                              hover:bg-gray-200" {
                                    "Save tags"
                                }
                            }
                        }
                    }
                }
            }

            // Handler gating: awards widget shown only when post_awards flag is enabled.
            // Toggle live: autumn flags enable post_awards
            @if flags.enabled("post_awards") {
                div class="bg-white rounded-lg shadow-sm border border-gray-200 p-4 mb-6" {
                    p class="text-sm font-semibold text-gray-700 mb-2" { "Awards" }
                    div class="flex gap-2 text-lg" {
                        span title="Gold" { "\u{1F947}" }
                        span title="Silver" { "\u{1F948}" }
                        span title="Bronze" { "\u{1F949}" }
                    }
                }
            }

            // Comment form
            @if current_user.is_some() {
                form action=(super::comments::__autumn_path_create(&sub.slug, &post.slug))
                     method="post"
                     class="bg-white rounded-lg shadow-sm border border-gray-200 p-4 mb-6" {
                    input type="hidden" name="_csrf" value=(csrf.token());
                    textarea name="body" rows="4" required
                             placeholder="What are your thoughts?"
                             class="w-full border border-gray-300 rounded px-3 py-2 text-sm mb-3 \
                                    focus:outline-none focus:ring-2 focus:ring-orange-400" {}
                    button type="submit"
                           class="px-4 py-2 bg-orange-500 text-white rounded text-sm \
                                  hover:bg-orange-600" {
                        "Comment"
                    }
                }
            } @else {
                div class="bg-white rounded-lg shadow-sm border border-gray-200 p-4 mb-6 \
                           text-center text-sm text-gray-500" {
                    a href="/login" class="text-orange-600 hover:underline" { "Log in" }
                    " to comment"
                }
            }

            // Comments
            div class="space-y-3" {
                h2 class="font-semibold text-gray-700 mb-2" {
                    (post.comment_count) " Comments"
                }
                @for comment in &post_comments {
                    @let comment_author = comment.author().ok().flatten();
                    div class="bg-white rounded-lg shadow-sm border border-gray-200 p-4" {
                        div class="flex items-center gap-2 text-xs text-gray-400 mb-2" {
                            @if let Some(comment_author) = comment_author {
                                a href=(super::auth::__autumn_path_profile(&comment_author.username))
                                   class="font-medium text-gray-600 hover:underline" {
                                    "u/" (comment_author.username)
                                }
                            }
                            "\u{2022} " (time_ago(&comment.created_at))
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
                    p class="text-gray-400 text-center py-8 text-sm" {
                        "No comments yet. Start the conversation!"
                    }
                }
            }
        },
    ))
}

// ── Edit post ──────────────────────────────────────────────────

/// Load a post by `(sub_slug, post_slug)` and authorize `action` against it,
/// or fail with 404/authorization error. Shared by every handler that
/// mutates (or renders a mutation form for) a single post.
async fn load_post_and_authorize(
    state: &AppState,
    session: &Session,
    db: &mut Db,
    sub_slug: &str,
    post_slug: &str,
    action: &str,
) -> AutumnResult<Post> {
    let post: Post = posts::table
        .inner_join(subreddits::table.on(posts::subreddit_id.eq(subreddits::id)))
        .filter(subreddits::slug.eq(sub_slug))
        .filter(posts::slug.eq(post_slug))
        .select(Post::as_select())
        .first(&mut *db)
        .await
        .map_err(|_| AutumnError::not_found_msg("Post not found"))?;

    autumn_web::authorization::authorize::<Post>(state, session, action, &post).await?;
    Ok(post)
}

#[secured]
#[get("/r/{sub_slug}/posts/{post_slug}/edit")]
pub async fn edit_form(
    Path((sub_slug, post_slug)): Path<(String, String)>,
    State(state): State<AppState>,
    session: Session,
    csrf: CsrfToken,
    mut db: Db,
) -> AutumnResult<Markup> {
    let current_user = session.get("username").await;

    let post =
        load_post_and_authorize(&state, &session, &mut db, &sub_slug, &post_slug, "update").await?;

    Ok(layout(
        &format!("Edit: {}", post.title),
        current_user.as_deref(),
        Some(csrf.token()),
        html! {
            div class="max-w-2xl mx-auto" {
                h1 class="text-2xl font-bold mb-6" { "Edit Post" }
                form action=(paths::update(&sub_slug, &post_slug)) method="post"
                     class="space-y-4 bg-white rounded-lg shadow p-6" {
                    input type="hidden" name="_csrf" value=(csrf.token());
                    div {
                        label for="title" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Title"
                        }
                        input type="text" id="title" name="title" required
                              value=(post.title)
                              class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                     focus:outline-none focus:ring-2 focus:ring-orange-400";
                    }
                    div {
                        label for="body" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Text"
                        }
                        textarea id="body" name="body" rows="8"
                                 class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                        focus:outline-none focus:ring-2 focus:ring-orange-400" {
                            (post.body)
                        }
                    }
                    div class="flex gap-3" {
                        button type="submit"
                               class="px-6 py-2 bg-orange-500 text-white rounded font-medium \
                                      hover:bg-orange-600 transition-colors" {
                            "Save"
                        }
                        a href=(paths::show(&sub_slug, &post_slug))
                           class="px-6 py-2 text-gray-500 hover:text-gray-700" { "Cancel" }
                    }
                }
            }
        },
    ))
}

#[derive(serde::Deserialize)]
pub struct EditPostForm {
    pub title: String,
    #[serde(default)]
    pub body: String,
}

#[secured]
#[post("/r/{sub_slug}/posts/{post_slug}")]
pub async fn update(
    Path((sub_slug, post_slug)): Path<(String, String)>,
    State(state): State<AppState>,
    session: Session,
    mut db: Db,
    repo: PgPostRepository,
    flash: Flash,
    form: Form<EditPostForm>,
) -> AutumnResult<Redirect> {
    let post =
        load_post_and_authorize(&state, &session, &mut db, &sub_slug, &post_slug, "update").await?;

    let title = form.0.title.trim().to_string();
    if title.is_empty() || title.len() > 300 {
        return Err(AutumnError::unprocessable_msg(
            "Title must be 1-300 characters",
        ));
    }

    let base_slug = slugify(&title);
    if base_slug.is_empty() {
        return Err(AutumnError::unprocessable_msg(
            "Title must contain at least one letter or number",
        ));
    }
    // Ensure unique slug within subreddit, excluding the current post
    let new_slug = unique_slug_excluding(&base_slug, post.subreddit_id, post.id, &mut db).await?;

    let changes = crate::models::UpdatePost {
        title: Patch::Set(title),
        slug: Patch::Set(new_slug.clone()),
        body: Patch::Set(form.0.body.trim().to_string()),
        ..Default::default()
    };
    let sub: Subreddit = subreddits::table
        .find(post.subreddit_id)
        .first(&mut *db)
        .await?;

    let author: crate::models::User = crate::schema::users::table
        .find(post.author_id)
        .first(&mut *db)
        .await?;

    let lookup = crate::repositories::PostRelationsLookup {
        author_name: author.username,
        sub_name: sub.name.clone(),
        sub_slug: sub.slug.clone(),
    };

    crate::repositories::CURRENT_POST_RELATIONS
        .scope(lookup, async move { repo.update(post.id, &changes).await })
        .await?;

    flash.success("Post updated.").await;
    Ok(Redirect::to(&paths::show(&sub_slug, &new_slug)))
}

// ── Manage tags (#1324 many-to-many demo) ───────────────────────

#[derive(serde::Deserialize)]
pub struct ManageTagsForm {
    /// Comma-separated tag names (newlines also split), e.g. `"rust, webdev"`.
    #[serde(default)]
    pub tags: String,
}

/// Resolve free-text tag names to ids, creating any tag that doesn't exist
/// yet. Batched to at most one lookup, one insert, and one lookup for any
/// insert that lost a create race to a concurrent request (find-then-insert;
/// a losing insert just means the slug already exists by the time it runs,
/// in which case the already-created row is looked up instead — the same
/// shape the DB layer as a whole already handles via other unique
/// constraints in this app) — 1-3 round trips total, not per tag name.
async fn resolve_or_create_tag_ids(raw: &str, db: &mut Db) -> AutumnResult<Vec<i64>> {
    let mut slug_order: Vec<String> = Vec::new();
    let mut name_by_slug: HashMap<String, String> = HashMap::new();
    for piece in raw.split([',', '\n']) {
        let name = piece.trim();
        if name.is_empty() {
            continue;
        }
        let slug = slugify(name);
        if slug.is_empty() {
            continue;
        }
        if !name_by_slug.contains_key(&slug) {
            slug_order.push(slug.clone());
        }
        name_by_slug.insert(slug, name.to_string());
    }
    if slug_order.is_empty() {
        return Ok(Vec::new());
    }

    let mut id_by_slug: HashMap<String, i64> = tags::table
        .filter(tags::slug.eq_any(slug_order.clone()))
        .select(Tag::as_select())
        .load(&mut **db)
        .await?
        .into_iter()
        .map(|tag| (tag.slug, tag.id))
        .collect();

    let missing: Vec<String> = slug_order
        .iter()
        .filter(|slug| !id_by_slug.contains_key(*slug))
        .cloned()
        .collect();
    if !missing.is_empty() {
        let new_tags: Vec<NewTag> = missing
            .iter()
            .map(|slug| NewTag {
                name: name_by_slug[slug].clone(),
                slug: slug.clone(),
            })
            .collect();
        let inserted: Vec<Tag> = diesel::insert_into(tags::table)
            .values(&new_tags)
            .on_conflict(tags::slug)
            .do_nothing()
            .get_results(&mut **db)
            .await?;
        id_by_slug.extend(inserted.into_iter().map(|tag| (tag.slug, tag.id)));

        // Any slug still missing lost a create race to a concurrent insert;
        // the row now exists, look it up.
        let still_missing: Vec<String> = missing
            .into_iter()
            .filter(|slug| !id_by_slug.contains_key(slug))
            .collect();
        if !still_missing.is_empty() {
            let races: Vec<Tag> = tags::table
                .filter(tags::slug.eq_any(still_missing))
                .select(Tag::as_select())
                .load(&mut **db)
                .await?;
            id_by_slug.extend(races.into_iter().map(|tag| (tag.slug, tag.id)));
        }
    }

    let mut ids = Vec::with_capacity(slug_order.len());
    for slug in &slug_order {
        let id = id_by_slug.get(slug).copied().ok_or_else(|| {
            AutumnError::not_found_msg(format!("Tag slug '{slug}' not found after resolution"))
        })?;
        ids.push(id);
    }
    Ok(ids)
}

/// Replace a post's tags with the free-text `tags` field, creating any new
/// tags. Demonstrates the generated `set_tags` (#[has_many(Tag, through =
/// post_tags)]) mutation helper end-to-end from an HTTP handler.
#[secured]
#[post("/r/{sub_slug}/posts/{post_slug}/tags")]
pub async fn manage_tags(
    Path((sub_slug, post_slug)): Path<(String, String)>,
    State(state): State<AppState>,
    session: Session,
    mut db: Db,
    repo: PgPostRepository,
    flash: Flash,
    form: Form<ManageTagsForm>,
) -> AutumnResult<Redirect> {
    let post =
        load_post_and_authorize(&state, &session, &mut db, &sub_slug, &post_slug, "update").await?;

    let tag_ids = resolve_or_create_tag_ids(&form.0.tags, &mut db).await?;
    drop(db);
    repo.set_tags(post.id, &tag_ids).await?;

    flash.success("Tags updated.").await;
    Ok(Redirect::to(&paths::show(&sub_slug, &post_slug)))
}

// ── Delete post (htmx) ────────────────────────────────────────

#[secured]
#[delete("/r/{sub_slug}/posts/{post_slug}")]
pub async fn delete_post(
    Path((sub_slug, post_slug)): Path<(String, String)>,
    State(state): State<AppState>,
    session: Session,
    mut db: Db,
    repo: PgPostRepository,
    flash: Flash,
) -> AutumnResult<autumn_web::reexports::axum::response::Response> {
    let post =
        load_post_and_authorize(&state, &session, &mut db, &sub_slug, &post_slug, "delete").await?;

    repo.delete_by_id(post.id).await?;

    let _ = state.broadcast().publish_oob(
        &format!("posts:r/{}", sub_slug),
        &post.dom_id(),
        &autumn_web::htmx::OobSwap::Delete,
        &autumn_web::html! {},
    );

    flash.success("Post deleted.").await;
    Ok(super::layout::hx_redirect_to(
        &super::subreddits::__autumn_path_show(&sub_slug),
    ))
}

// ── Helpers ────────────────────────────────────────────────────

/// Generate a unique slug within a subreddit by appending `-2`, `-3`, etc.
async fn unique_slug(
    base: &str,
    subreddit_id: i64,
    conn: &mut diesel_async::AsyncPgConnection,
) -> AutumnResult<String> {
    let mut slug = base.to_string();
    let mut suffix = 2u64;
    loop {
        let count: i64 = posts::table
            .filter(posts::subreddit_id.eq(subreddit_id))
            .filter(posts::slug.eq(&slug))
            .count()
            .get_result(conn)
            .await?;
        if count == 0 {
            return Ok(slug);
        }
        slug = format!("{base}-{suffix}");
        suffix += 1;
    }
}

/// Like `unique_slug`, but excludes a specific post ID (for updates).
async fn unique_slug_excluding(
    base: &str,
    subreddit_id: i64,
    exclude_id: i64,
    conn: &mut diesel_async::AsyncPgConnection,
) -> AutumnResult<String> {
    let mut slug = base.to_string();
    let mut suffix = 2u64;
    loop {
        let count: i64 = posts::table
            .filter(posts::subreddit_id.eq(subreddit_id))
            .filter(posts::slug.eq(&slug))
            .filter(posts::id.ne(exclude_id))
            .count()
            .get_result(conn)
            .await?;
        if count == 0 {
            return Ok(slug);
        }
        slug = format!("{base}-{suffix}");
        suffix += 1;
    }
}

autumn_web::paths![
    front_page,
    submit_form,
    submit_to_sub_form,
    submit,
    show,
    edit_form,
    update,
    manage_tags,
    delete_post
];

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn post_publication_enqueue_failure_is_returned_to_submit() {
        let error = PostPublicationJob::enqueue(PostPublicationArgs::new(
            99,
            "Ferris arrives",
            "ferris-arrives",
            "rust",
            "ferris",
        ))
        .await
        .expect_err("missing job runtime should fail post submission");

        assert!(
            error.to_string().contains("job runtime is not initialized"),
            "unexpected error: {error}"
        );
    }
}
