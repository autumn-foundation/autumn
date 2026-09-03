//! Subreddit routes — list communities, create, and show.
//!
//! Demonstrates: #[secured] macro for requiring authentication,
//! repository-generated CRUD, `CsrfToken` for forms, Maud templates.

use autumn_web::extract::Path;
use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::{NewSubreddit, Subreddit, SubredditComments as _};
use crate::repositories::{PgSubredditRepository, SubredditRepository};
use crate::schema::users;
use autumn_web::widgets::{CommentThread, CommentView, comment_thread};
use autumn_web::{contains_letter_or_number, slugify};

use super::layout::{layout, layout_with_seo, time_ago};

// ── List all communities ───────────────────────────────────────

/// The community index.
///
/// Every meta value is fixed text, so the `seo(...)` argument carries all of
/// them and the handler only adds the canonical URL. See `docs/guide/seo.md`.
#[get(
    "/r",
    seo(
        title = "Communities \u{2022} Autumn Reddit",
        description = "Every community on Autumn Reddit. Each one collects posts on one topic.",
        og_type = "website"
    )
)]
pub async fn list(
    seo: SeoMeta,
    session: Session,
    repo: PgSubredditRepository,
) -> AutumnResult<Markup> {
    let current_user = session.get("username").await;
    let all = repo.find_all().await?;

    Ok(layout_with_seo(
        crate::seo::with_canonical(seo, "/r"),
        current_user.as_deref(),
        None,
        html! {
            div class="flex justify-between items-center mb-6" {
                h1 class="text-2xl font-bold" { "Communities" }
                @if current_user.is_some() {
                    a href=(paths::create_form())
                      class="px-4 py-2 bg-orange-500 text-white rounded hover:bg-orange-600 text-sm" {
                        "+ Create Community"
                    }
                }
            }
            div class="space-y-3" {
                @for sub in &all {
                    a href=(paths::show(&sub.slug))
                       class="block bg-white rounded-lg shadow-sm border border-gray-200 \
                              hover:border-orange-300 hover:shadow transition-all p-4" {
                        div class="flex items-center justify-between" {
                            div {
                                h2 class="font-semibold text-orange-600" { "r/" (sub.name) }
                                @if !sub.description.is_empty() {
                                    p class="text-sm text-gray-500 mt-1" { (sub.description) }
                                }
                            }
                            div class="text-right text-xs text-gray-400" {
                                div { (sub.subscriber_count) " members" }
                                div { "Created " (time_ago(&sub.created_at)) }
                            }
                        }
                    }
                }
                @if all.is_empty() {
                    p class="text-gray-400 text-center py-12" {
                        "No communities yet. Be the first to create one!"
                    }
                }
            }
        },
    ))
}

// ── Create community form (requires auth) ──────────────────────

#[secured]
#[get("/r/create")]
pub async fn create_form(session: Session, csrf: CsrfToken) -> AutumnResult<Markup> {
    let current_user = session.get("username").await;
    Ok(layout(
        "Create Community",
        current_user.as_deref(),
        Some(csrf.token()),
        html! {
            div class="max-w-lg mx-auto" {
                h1 class="text-2xl font-bold mb-6" { "Create a Community" }
                form action=(paths::create()) method="post"
                     class="space-y-4 bg-white rounded-lg shadow p-6" {
                    input type="hidden" name="_csrf" value=(csrf.token());
                    div {
                        label for="name" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Community Name"
                        }
                        div class="flex items-center" {
                            span class="text-gray-400 mr-1" { "r/" }
                            input type="text" id="name" name="name" required
                                  minlength="2" maxlength="32"
                                  placeholder="rustlang"
                                  pattern="[a-zA-Z0-9_]+"
                                  class="flex-1 border border-gray-300 rounded px-3 py-2 text-sm \
                                         focus:outline-none focus:ring-2 focus:ring-orange-400";
                        }
                        p class="text-xs text-gray-400 mt-1" {
                            "Letters, numbers, and underscores only"
                        }
                    }
                    div {
                        label for="description" class="block text-sm font-medium text-gray-700 mb-1" {
                            "Description"
                        }
                        textarea id="description" name="description" rows="3"
                                 placeholder="What is this community about?"
                                 class="w-full border border-gray-300 rounded px-3 py-2 text-sm \
                                        focus:outline-none focus:ring-2 focus:ring-orange-400" {}
                    }
                    button type="submit"
                           class="w-full bg-orange-500 text-white py-2 rounded font-medium \
                                  hover:bg-orange-600 transition-colors" {
                        "Create Community"
                    }
                }
            }
        },
    ))
}

#[derive(serde::Deserialize)]
pub struct CreateSubredditForm {
    pub name: String,
    pub description: String,
}

/// The community-name rules, factored out of the handler so they can be
/// exercised without a database.
///
/// The content rule asks `contains_letter_or_number`, not `slugify(name)
/// .is_empty()`: `slugify` never returns an empty string, so the check this
/// replaces was unreachable and a community called `"***"` was created with a
/// hash slug (the same bug as the post title, issue #2424).
fn validate_community_name(name: &str) -> Result<(), AutumnError> {
    // Characters, not bytes: `str::len()` counts UTF-8 bytes, so it told an
    // 11-character Japanese name it was over 32 "characters" and let a
    // 1-character one past a rule that exists to require 2. The post title's
    // `length(min = 1, max = 300)` already counts characters, so this also
    // makes the app's two length rules mean the same thing.
    let length = name.chars().count();
    if !(2..=32).contains(&length) {
        return Err(AutumnError::unprocessable_msg(
            "Community name must be 2-32 characters",
        ));
    }
    if !contains_letter_or_number(name) {
        return Err(AutumnError::unprocessable_msg(
            "Community name must contain at least one letter or number",
        ));
    }
    Ok(())
}

#[secured]
#[post("/r/create")]
pub async fn create(
    session: Session,
    repo: PgSubredditRepository,
    form: Form<CreateSubredditForm>,
) -> AutumnResult<Redirect> {
    let user_id: i64 = session
        .get("user_id")
        .await
        .ok_or_else(|| AutumnError::unauthorized_msg("Login required"))?
        .parse()
        .map_err(|_| AutumnError::bad_request_msg("Invalid session"))?;

    let name = form.0.name.trim().to_string();
    validate_community_name(&name)?;
    let slug = slugify(&name);

    let new_sub = NewSubreddit {
        name: name.clone(),
        slug: slug.clone(),
        description: form.0.description.trim().to_string(),
        creator_id: user_id,
    };

    // Race-safe get-or-insert on the unique `slug` column (#1382): replaces the
    // old raw `insert_into(...).map_err("already taken")`. If two requests race
    // to create the same community, `ON CONFLICT DO NOTHING` lets exactly one
    // win the insert while the loser reads the winner's row back — neither sees
    // a unique-violation, and both land on the same community.
    let (subreddit, created) = repo.find_or_create_by_slug(slug.clone(), &new_sub).await?;
    if !created {
        // The slug is already owned by an existing community; preserve the prior
        // UX of rejecting the duplicate rather than redirecting into someone
        // else's community as if the create had succeeded.
        return Err(AutumnError::unprocessable_msg(
            "Community name already taken",
        ));
    }

    Ok(Redirect::to(&paths::show(&subreddit.slug)))
}

// ── Show subreddit with posts ──────────────────────────────────

/// A community's page.
///
/// The name and the description come from the row, so the handler refines the
/// attribute defaults after it reads the community.
///
/// This is also the app's pagination showcase (`docs/guide/pagination.md`).
/// Offset pagination is the right flavour here: a community listing is a
/// browse-style UI where "page 3" is a meaningful, linkable place, unlike the
/// front page's live feed. `PageRequest` clamps `?page=` and `?size=` rather
/// than rejecting them, so a hand-edited or stale URL renders the list instead
/// of a 400.
#[get("/r/{slug}", seo(og_type = "website"))]
// Every argument is a distinct extractor: path, SEO defaults, session, CSRF
// token, CSRF field name, page request, repository, connection, flash.
#[allow(clippy::too_many_arguments)]
pub async fn show(
    Path(slug): Path<String>,
    seo: SeoMeta,
    session: Session,
    csrf: CsrfToken,
    // Same as the post detail page: the widget's hidden input must be named
    // whatever `security.csrf.form_field` configured, or the first submit 403s.
    csrf_field: CsrfFormField,
    page_req: PageRequest,
    repo: PgSubredditRepository,
    mut db: Db,
    flash: Flash,
) -> AutumnResult<Markup> {
    let current_user = session.get("username").await;

    let subs = repo.find_by_slug(slug.clone()).await?;
    let sub = subs
        .into_iter()
        .next()
        .ok_or_else(|| AutumnError::not_found_msg(format!("r/{slug} not found")))?;

    // Offset pagination, in the two queries it always takes: the filtered
    // COUNT, then the page slice. The COUNT carries the SAME `subreddit_id`
    // filter as the slice — a total computed over the whole table would
    // render a pager with pages that do not exist.
    let total: i64 = crate::schema::posts::table
        .filter(crate::schema::posts::subreddit_id.eq(sub.id))
        .count()
        .get_result(&mut *db)
        .await?;

    // `page_req.limit()` / `page_req.offset()` come pre-clamped: `?size=0` and
    // `?size=99999` both land inside `1..=MAX_PAGE_SIZE`, so this route cannot
    // be turned into an unbounded read by a query parameter.
    let rows: Vec<(i64, String, String, i64, i64, String, chrono::NaiveDateTime)> =
        crate::schema::posts::table
            .filter(crate::schema::posts::subreddit_id.eq(sub.id))
            .inner_join(users::table.on(crate::schema::posts::author_id.eq(users::id)))
            // The `id` tie-breaker is what makes the ORDER BY a TOTAL order,
            // and it is load-bearing the moment LIMIT/OFFSET splits this query.
            // `hot_rank` defaults to 0.0, so a fresh community is mostly ties,
            // and PostgreSQL does not promise a stable order among equal keys:
            // without a unique final column, two page requests can return the
            // same post twice or skip one entirely, with nothing having
            // changed. See docs/guide/pagination.md.
            .order((
                crate::schema::posts::hot_rank.desc(),
                crate::schema::posts::id.desc(),
            ))
            .limit(page_req.limit())
            .offset(page_req.offset())
            .select((
                crate::schema::posts::id,
                crate::schema::posts::title,
                crate::schema::posts::slug,
                crate::schema::posts::score,
                crate::schema::posts::comment_count,
                users::username,
                crate::schema::posts::created_at,
            ))
            .load(&mut *db)
            .await?;
    let page = Page::new(rows, total, &page_req);
    let posts = page.content.as_slice();

    // Release this request's pooled connection before the comment read. The
    // repository helper takes its OWN connection from the pool -- it does not
    // join an enclosing `Db` checkout -- so holding both across the await lets
    // `pool_size` concurrent requests each pin one connection while waiting for
    // a second that can never arrive. Every route that pairs a `Db` query with
    // a repository helper has to drop first; the post-detail route does the
    // same a few lines above its own `comment_thread` call.
    drop(db);

    // AC5 of #1367, in full: `Subreddit` is the SECOND commentable model, and
    // this is *all* it took -- the `#[commentable]` attribute on the model, its
    // `comment_count` column, and these few lines of rendering. No comments
    // table of its own, no route, no threading query: the framework router
    // `main.rs` already mounts for `Post` serves this too, keyed on
    // `Subreddit::COMMENTABLE_TYPE`.
    let thread = repo.comment_thread(sub.id).await?;
    let comments_config = autumn_web::commentable::CommentsConfig::default();
    let mut comment_config = CommentThread::from_spec(
        autumn_web::commentable::thread_dom_id(Subreddit::COMMENTABLE_TYPE, sub.id),
        autumn_web::commentable::thread_action(
            &comments_config,
            Subreddit::COMMENTABLE_TYPE,
            sub.id,
        ),
        Subreddit::commentable_spec(),
    )
    .label("Community discussion")
    .empty_text("No community discussion yet.")
    .return_to(__autumn_path_show(&sub.slug));
    if current_user.is_some() {
        comment_config = comment_config
            .csrf_token(csrf.token())
            .csrf_field(csrf_field.0.clone());
    } else {
        comment_config = comment_config
            .read_only()
            .sign_in_prompt("Log in to join the discussion.");
    }

    let first_page = page.page <= 1;

    // A paginated listing gets a SELF-referential canonical: page 2 is not a
    // duplicate of page 1, and pointing every page at page 1 asks a crawler to
    // drop the deeper pages from the index. See docs/guide/seo.md.
    //
    // `size` belongs in it whenever it is not the default, for the same reason
    // `page` does: the canonical must name a URL that renders THIS slice.
    // `/r/x?page=2` under the default size of 20 is posts 21-40, which is not
    // what `?page=2&size=5` just showed the visitor — pointing at it would
    // declare materially different content canonical.
    let canonical_path = {
        let base = __autumn_path_show(&sub.slug);
        let default_size = autumn_web::pagination::DEFAULT_PAGE_SIZE;
        match (first_page, page.size == default_size) {
            (true, true) => base,
            (true, false) => format!("{base}?size={}", page.size),
            (false, true) => format!("{base}?page={}", page.page),
            (false, false) => format!("{base}?page={}&size={}", page.page, page.size),
        }
    };

    let seo = crate::seo::with_canonical(
        seo.title(if first_page {
            format!("r/{} \u{2022} Autumn Reddit", sub.name)
        } else {
            format!(
                "r/{} \u{2022} page {} \u{2022} Autumn Reddit",
                sub.name, page.page
            )
        })
        .description(
            crate::seo::summarize(&sub.description, 155)
                .unwrap_or_else(|| format!("The r/{} community on Autumn Reddit.", sub.name)),
        ),
        &canonical_path,
    );

    // Consume the flash only after all fallible work above.
    let flash_html = flash.render().await;
    Ok(layout_with_seo(
        seo,
        current_user.as_deref(),
        Some(csrf.token()),
        html! {
            (flash_html)
            // Subreddit header
            div class="bg-white rounded-lg shadow-sm border border-gray-200 p-6 mb-6" {
                div class="flex justify-between items-start" {
                    div {
                        h1 class="text-2xl font-bold text-orange-600" { "r/" (sub.name) }
                        @if !sub.description.is_empty() {
                            p class="text-gray-600 mt-2" { (sub.description) }
                        }
                        p class="text-xs text-gray-400 mt-2" {
                            (sub.subscriber_count) " members \u{2022} created "
                            (time_ago(&sub.created_at))
                        }
                    }
                    @if current_user.is_some() {
                        a href=(super::posts::__autumn_path_submit_to_sub_form(&sub.slug))
                          class="px-4 py-2 bg-orange-500 text-white rounded text-sm \
                                 hover:bg-orange-600" {
                            "New Post"
                        }
                    }
                }
            }

            // Post list. The live SSE feed appends new posts to the TOP of
            // the list, which is only correct on page 1 — on page 2 a
            // just-published post belongs at the head of page 1, not here, and
            // appending it would show the reader a row that is not part of the
            // slice they asked for. So the feed is wired on the first page
            // only; deeper pages are a plain, stable listing.
            ul id="posts-list" class="space-y-2"
                hx-ext=[first_page.then_some("sse")]
                sse-connect=[first_page.then(|| format!("/r/{}/posts/stream", sub.slug))]
                sse-swap=[first_page.then_some("message")]
                hx-swap=[first_page.then_some("none")] {
                @for (post_id, title, post_slug, score, comment_count, author, created_at) in posts {
                    li id=(format!("post-{}", post_id)) class="posts-feed-item transition-all" {
                        div class="posts-feed-card-version bg-white rounded-lg shadow-sm border border-gray-200 hover:border-orange-300 transition-colors" {
                            div class="flex items-start gap-3 p-4" {
                                // Vote controls
                                // Feed: `None` current (see the front page) —
                                // no per-row `reaction_of` lookup. The CSRF
                                // token is threaded so the no-JS form POSTs.
                                (super::layout::vote_controls(*post_id, *score, None, Some(&csrf)))

                                // Post info
                                div class="flex-1 min-w-0" {
                                    a href=(super::posts::__autumn_path_show(&sub.slug, post_slug))
                                       class="text-lg font-medium text-gray-900 hover:text-orange-600" {
                                        (title)
                                    }
                                    div class="text-xs text-gray-400 mt-1" {
                                        "posted by "
                                        a href=(super::auth::__autumn_path_profile(author))
                                           class="text-gray-500 hover:underline" { "u/" (author) }
                                        " " (time_ago(created_at))
                                        " \u{2022} "
                                        a href=(super::posts::__autumn_path_show(&sub.slug, post_slug))
                                           class="text-gray-500 hover:text-orange-600" {
                                            (comment_count) " comments"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                @if posts.is_empty() {
                    p class="text-gray-400 text-center py-12" {
                        @if page.page > 1 {
                            "No posts on this page."
                        } @else {
                            "No posts yet. Be the first!"
                        }
                    }
                }
            }

            // One line renders the whole pager: a <nav aria-label="Pagination">
            // with a windowed page-number sequence, `aria-current="page"` on
            // the active page, and non-focusable `aria-disabled` prev/next at
            // the ends. `PagerOptions` has no `hx_target` here on purpose, so
            // every link is a plain <a href> and pagination keeps working with
            // JavaScript disabled. See docs/guide/pagination.md.
            // `include_size` carries the effective page size onto every link.
            // Without it a visitor who arrived on `?size=5` gets links that say
            // only `?page=2`, silently reverting to the default size — so page 2
            // would start at offset 20 and skip posts 6-20.
            (pagination_nav(
                &page,
                &PagerOptions::new(&__autumn_path_show(&sub.slug)).include_size(),
            ))

            // Community discussion -- the second `#[commentable]` model (#1367).
            div class="bg-white rounded-lg shadow-sm border border-gray-200 p-4 mt-6" {
                // No count, for the same reason as the post detail page: an
                // htmx reply swaps only the widget's own region, so a number
                // rendered outside it would go stale the moment someone
                // replies.
                h2 class="font-semibold text-gray-700 mb-2" { "Community discussion" }
                (comment_thread(&comment_config, &CommentView::from_thread(&thread)))
            }
        },
    ))
}

autumn_web::paths![list, create_form, create, show];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_community_name_with_no_letter_or_number_is_rejected() {
        // The same dead-guard bug as the post title (#2424): `slugify` grew a
        // non-empty fallback, so `slugify(&name).is_empty()` could never fire.
        for name in ["***", "!!!???...:::", "🎉🔥💯", "----"] {
            assert!(
                !slugify(name).is_empty(),
                "slugify({name:?}) is non-empty -- that is why the old guard was dead"
            );
            let Err(error) = validate_community_name(name) else {
                panic!("{name:?} must be rejected")
            };
            assert!(
                error.to_string().contains("at least one letter or number"),
                "{name:?} must explain itself; got: {error}"
            );
        }
    }

    /// These are the *server's* rules. The shipped form is deliberately
    /// narrower — `create_form`'s input carries `pattern="[a-zA-Z0-9_]+"`, so
    /// a browser will not send `"web dev"` or `"日本語"` in the first place.
    /// Widening that pattern is a separate UI change; what matters here is
    /// that the server does not reject real text out of hand.
    #[test]
    fn a_community_name_with_a_letter_or_number_in_any_script_is_accepted() {
        for name in ["rust", "web dev", "42", "日本語", "Привет"] {
            assert!(
                validate_community_name(name).is_ok(),
                "{name:?} must be accepted"
            );
        }
    }

    #[test]
    fn the_length_rule_still_applies_and_counts_characters() {
        for name in ["r", "", "日"] {
            let Err(error) = validate_community_name(name) else {
                panic!("{name:?} is too short")
            };
            assert!(
                error.to_string().contains("2-32 characters"),
                "got: {error}"
            );
        }
        let long = "r".repeat(33);
        let Err(error) = validate_community_name(&long) else {
            panic!("a 33-character name is too long")
        };
        assert!(
            error.to_string().contains("2-32 characters"),
            "got: {error}"
        );

        // 11 characters, 33 bytes: a byte-counting rule called this too long.
        assert!(
            validate_community_name(&"日".repeat(11)).is_ok(),
            "an 11-character name is within a 32-character limit"
        );
    }
}
