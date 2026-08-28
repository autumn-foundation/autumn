//! Post routes — front page, submit, view, edit, delete.
//!
//! Demonstrates: CRUD with the Db extractor, `CsrfToken` in forms,
//! #[secured] for write operations, htmx for voting and deletion,
//! Maud templates with Tailwind CSS, and feature-flag fragment gating
//! via the `Flags` extractor.
//!
//! Three UI subsystems are showcased on the submit/show pair:
//!
//! * **Forms and validation** (`docs/guide/forms.md`) — the submit route is a
//!   `ChangesetForm` round-trip, so a rejected submission is re-rendered with
//!   the author's text still in the fields and one message per field.
//! * **Typed accessible primitives** (`docs/guide/accessibility.md`) — every
//!   control on that form is an `autumn_web::a11y` value whose unlabeled form
//!   does not compile.
//! * **Rich text** (`docs/guide/rich-text.md`) — post bodies are
//!   user-submitted Markdown, rendered through `render_user_content`'s
//!   sanitizing path at display time.
//!
//! All three work with JavaScript disabled: the form carries no `hx-*`
//! attributes and posts normally.

use std::collections::HashMap;

use autumn_web::experiments::Experiments;
use autumn_web::extract::Path;
use autumn_web::extract::State;
use autumn_web::feature_flags::Flags;
use autumn_web::form::ChangesetForm;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use autumn_web::reexports::http;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use scoped_futures::ScopedFutureExt;

use crate::jobs::{PostPublicationArgs, PostPublicationJob};
use crate::models::{
    NewTag, Post, PostAssociations, PostComments as _, PostReactions as _, PostTagsMutations,
    Subreddit, Tag,
};
use crate::repositories::{PgPostRepository, PgVoteRepository, PostRepository};
use crate::schema::{posts, subreddits, tags};
use autumn_web::slugify;
use autumn_web::widgets::{CommentThread, CommentView, comment_thread as comment_thread_widget};

fn posts_per_page() -> i64 {
    crate::config_svc()
        .get("posts_per_page")
        .ok()
        .and_then(|v| v.as_int())
        .unwrap_or(25)
}

use super::layout::{layout, layout_with_seo, time_ago, vote_controls};

// ── Front page — hot posts across all subreddits ───────────────

/// The front page.
///
/// Route-level SEO (#1182): the `seo(...)` argument declares the values that
/// never change, and the `SeoMeta` parameter delivers them to the handler.
/// The handler adds the one value the attribute cannot hold — the canonical
/// URL, which needs `[seo] base_url` from `autumn.toml` at run time.
///
/// See `docs/guide/seo.md`.
#[get(
    "/",
    seo(
        title = "Autumn Reddit \u{2022} Front page",
        description = "The hottest posts across every community on Autumn Reddit, a demo \
                       link-sharing site built with the Autumn web framework for Rust.",
        og_type = "website",
        // `summary` and not `summary_large_image`: this app ships no share
        // image. Set `og_image` first, then change the card type.
        twitter_card = "summary"
    )
)]
#[allow(clippy::too_many_arguments)]
pub async fn front_page(
    seo: SeoMeta,
    session: Session,
    csrf: CsrfToken,
    // Cookie consent (#1214). The extractor reads the request's `Cookie`
    // header directly — no layer, no state — and gates this app's one
    // non-essential script below. See docs/guide/cookie-consent.md.
    consent: autumn_web::consent::Consent,
    mut db: Db,
    State(state): State<AppState>,
    repo: PgPostRepository,
    votes_repo: PgVoteRepository,
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

    // Release the `Db` extractor's connection now, before any other pooled
    // checkout. `Db` acquires its connection eagerly at extraction and holds it
    // until it is dropped (not just for the duration of a `&mut *db` borrow), so
    // keeping it alive across the leaderboard aggregate below — or the `preload`
    // further down — would make this handler hold *two* connections at once.
    // That is invisible in dev and fatal under load: this app runs the default
    // pool (10 connections, no read replica), so once ten requests are in this
    // handler simultaneously, each holding `db` and waiting for a second
    // checkout, none can ever proceed. Dropping `db` here keeps the handler at
    // one connection at a time, which cannot deadlock at any concurrency.
    drop(db);

    // "Top posts by votes" leaderboard (#1364, AC3): a single typed
    // grouped-aggregate call — `SUM(value) GROUP BY post_id`, ordered by the
    // aggregate descending, top 5 — replacing what would otherwise be a
    // hand-written `SUM ... GROUP BY ... ORDER BY ... LIMIT` string. This is a
    // *read*, so it is replica-eligible (routes through the repository's read
    // route). The score-maintenance path in `routes::votes` stays an atomic
    // primary-side WRITE — see the note there.
    //
    // `votes.post_id` is nullable (comment votes carry a NULL `post_id`), and
    // the grouped-aggregate codegen guards the group column with `IS NOT NULL`,
    // so the NULL group is excluded — this leaderboard counts only
    // post-directed votes (comment votes are correctly omitted), no per-call
    // filter needed. Binding the result to an owned `Vec` releases the
    // repository's pooled connection before the title lookup checks one out.
    let top_by_votes: Vec<(i64, Option<i64>)> = votes_repo
        .sum_value_grouped_by_post_id()
        .order_by_aggregate_desc()
        .limit(5)
        .load()
        .await?;
    // Resolve the leaderboard entries' titles in one query (order preserved via
    // the `top_by_votes` iteration below).
    let top_ids: Vec<i64> = top_by_votes.iter().map(|(id, _)| *id).collect();
    // Skip the follow-up title lookup entirely when the leaderboard is empty —
    // otherwise we'd issue a pointless `WHERE id = ANY('{}')` query.
    let top_titles: HashMap<i64, String> = if top_ids.is_empty() {
        HashMap::new()
    } else {
        // Use a fresh, short-lived pool checkout — not the `Db` extractor, which
        // was dropped above — so this lookup never overlaps another live
        // connection held by this request. The `conn` guard is released at the
        // end of this block, before `preload` checks one out.
        let pool = state
            .pool()
            .ok_or_else(|| AutumnError::service_unavailable_msg("Database not configured"))?;
        let mut conn = pool.get().await.map_err(AutumnError::from)?;
        posts::table
            .filter(posts::id.eq_any(&top_ids))
            .select((posts::id, posts::title))
            .load::<(i64, String)>(&mut conn)
            .await?
            .into_iter()
            .collect()
    };

    // The base rows were read from the primary via `Db`, so pin the preload to
    // the primary too (`on_primary`) — otherwise, under replica lag, an
    // author/subreddit just written may be missing on the replica and the post
    // would be skipped. `db` was already released above, so this checkout never
    // overlaps it — the handler still holds at most one connection.
    let hot_posts = repo
        .on_primary()
        .preload(hot_posts, Post::preload().author().subreddit())
        .await?;

    // Consume the flash only after all fallible work, so a mid-handler error
    // doesn't drop the one-shot message before it is shown.
    let flash_html = flash.render().await;
    Ok(layout_with_seo(
        crate::seo::with_canonical(seo, "/"),
        current_user.as_deref(),
        Some(csrf.token()),
        html! {
            (flash_html)
            // Non-essential scripts live behind the consent gate, not behind
            // the banner: showing a prompt and loading the tracker anyway is
            // the failure mode the feature exists to prevent.
            (super::layout::analytics_snippet(&consent))
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

            // Top posts by votes — rendered from the typed grouped-aggregate
            // leaderboard read (#1364, AC3). Empty until posts have votes.
            @if !top_by_votes.is_empty() {
                div class="mb-4 px-4 py-3 bg-white rounded-lg shadow-sm border border-gray-200" {
                    div class="text-xs font-semibold text-gray-500 uppercase tracking-wide mb-2" {
                        "Top posts by votes"
                    }
                    ol class="space-y-1 text-sm" {
                        @for (post_id, sum) in &top_by_votes {
                            @if let Some(title) = top_titles.get(post_id) {
                                li class="flex items-center gap-2" {
                                    span class="font-semibold text-gray-500 w-8 text-right shrink-0" {
                                        (sum.unwrap_or(0))
                                    }
                                    a href=(format!("/posts/{}", post_id))
                                       class="text-gray-900 hover:text-orange-600 line-clamp-1" {
                                        (title)
                                    }
                                }
                            }
                        }
                    }
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
                                        // Feed: `None` current rather than one
                                        // `reaction_of` per row (an N+1). A
                                        // batch accessor is the follow-up. The
                                        // CSRF token *is* threaded, so the
                                        // buttons work with JavaScript off.
                                        (vote_controls(post.id, post.score, None, Some(&csrf)))
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

/// The submit form.
///
/// `robots = "noindex, nofollow"` declares the intent, but note what actually
/// reaches a crawler here: the page is behind `#[secured]`, so an anonymous
/// request gets the login redirect, not this HTML. The directive is therefore
/// belt-and-braces for signed-in states rather than the thing keeping the URL
/// out of an index. `routes::auth::profile` is the route where the same
/// directive genuinely does the work, on a page a crawler can fetch.
///
/// What the app must NOT do is also add `/submit` to `[seo.robots]
/// additional_rules`. A `Disallow` line stops the fetch, so no crawler could
/// ever read this tag — the two are alternatives for one URL, not layers.
/// See the comment in `autumn.toml` and `docs/guide/seo.md`.
///
/// The directive has a second effect on `#[static_get]` routes: the framework
/// drops such a route from `/sitemap.xml`, so the application never advertises
/// a URL and asks crawlers to skip it at the same time. This route is a
/// `#[get]` route, so no derived entry exists to drop.
#[secured]
#[get(
    "/submit",
    seo(
        title = "Submit a post \u{2022} Autumn Reddit",
        robots = "noindex, nofollow"
    )
)]
pub async fn submit_form(
    seo: SeoMeta,
    session: Session,
    csrf: CsrfToken,
    mut db: Db,
) -> AutumnResult<Markup> {
    let current_user = session.get("username").await;
    let subs = all_subreddits(&mut db).await?;

    // A blank changeset: the same type the POST handler re-renders on failure,
    // so `new` and `invalid` are one code path with one set of markup. The GET
    // route has no submitted body to capture a token from, so it passes
    // `csrf.token()` explicitly — see docs/guide/forms.md.
    let blank = ChangesetForm::blank(SubmitPostForm::default(), csrf.token());

    Ok(layout_with_seo(
        seo,
        current_user.as_deref(),
        Some(csrf.token()),
        submit_form_markup(&blank, &subs, None),
    ))
}

/// Submit form for a specific subreddit.
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

    let blank = ChangesetForm::blank(
        SubmitPostForm {
            subreddit_id: sub.id.to_string(),
            ..SubmitPostForm::default()
        },
        csrf.token(),
    );

    Ok(layout(
        &format!("Submit to r/{}", sub.name),
        current_user.as_deref(),
        Some(csrf.token()),
        submit_form_markup(&blank, &[], Some(&sub)),
    ))
}

/// Load every community, for the submit form's community picker.
async fn all_subreddits(db: &mut Db) -> AutumnResult<Vec<Subreddit>> {
    Ok(subreddits::table
        .order(subreddits::name.asc())
        .select(Subreddit::as_select())
        .load(&mut **db)
        .await?)
}

/// The submit form's markup — rendered by the two GET routes and re-rendered
/// verbatim by the POST route when validation fails.
///
/// Two framework features carry this function, and both are worth reading for:
///
/// **Typed accessible primitives (#1706).** Every control is an
/// `autumn_web::a11y` value rather than raw markup. `TextField`, `TextArea` and
/// `Select` are typestate: they do **not** implement `maud::Render` until a
/// label is attached, so the inaccessible version of this form is not merely
/// discouraged — it does not compile. The error wiring is typed too:
/// `aria_invalid` plus `described_by` point the field at its own message
/// element, so a screen-reader user hears the error when focus lands on the
/// input instead of having to hunt for red text. See
/// `docs/guide/accessibility.md`.
///
/// **The changeset round-trip (#1135).** `form.field_value(..)` and
/// `form.errors_for(..)` read the submitted values and the per-field errors out
/// of the same `Changeset`, so a rejected submission comes back with the user's
/// text still in the boxes. `form_tag` emits the hidden `_csrf` input, which is
/// what lets this form work with **no JavaScript at all**: nothing here is an
/// `hx-*` attribute, so a browser with scripting disabled performs an ordinary
/// POST and gets the same 422 page a fetch would. See `docs/guide/forms.md`.
///
/// Pass `fixed_sub` when the community is already chosen (the
/// `/r/{slug}/submit` entry point); the picker becomes a hidden input and
/// `subs` is ignored.
fn submit_form_markup(
    form: &ChangesetForm<SubmitPostForm>,
    subs: &[Subreddit],
    fixed_sub: Option<&Subreddit>,
) -> Markup {
    let heading = fixed_sub.map_or_else(
        || html! { "Create a Post" },
        |sub| {
            html! {
                "Post to " span class="text-orange-600" { "r/" (sub.name) }
            }
        },
    );

    let input_class = "w-full border border-gray-300 rounded px-3 py-2 text-sm \
                       focus:outline-none focus:ring-2 focus:ring-orange-400";
    let label_class = "block text-sm font-medium text-gray-700 mb-1";

    html! {
        div class="max-w-2xl mx-auto" {
            h1 class="text-2xl font-bold mb-6" { (heading) }
            (form.form_tag(&paths::submit(), "post", html! {
                @if let Some(sub) = fixed_sub {
                    input type="hidden" name="subreddit_id" value=(sub.id);
                } @else {
                    div {
                        (field_errors("subreddit_id", form))
                        (autumn_web::a11y::Select::new("subreddit_id")
                            .label("Community")
                            .label_class(label_class)
                            .class(input_class)
                            .required()
                            // The empty, disabled placeholder is load-bearing,
                            // not decoration. Without it the browser
                            // auto-selects the first real option, `required`
                            // is satisfied without the author choosing
                            // anything, and the post lands silently in
                            // whichever community sorts first. An empty value
                            // is also what makes `required` fire client-side,
                            // and `validate_subreddit_choice` rejects it
                            // server-side for a client that ignores both.
                            .options(
                                std::iter::once(
                                    autumn_web::a11y::SelectOption::new(
                                        "",
                                        "Choose a community\u{2026}",
                                    )
                                    .disabled(),
                                )
                                .chain(subs.iter().map(|sub| {
                                    autumn_web::a11y::SelectOption::new(
                                        sub.id.to_string(),
                                        format!("r/{}", sub.name),
                                    )
                                })),
                            )
                            .selected_value(form.field_value("subreddit_id").unwrap_or_default())
                            .aria_invalid(!form.errors_for("subreddit_id").is_empty())
                            .described_by("subreddit_id-error"))
                    }
                }

                div {
                    (field_errors("title", form))
                    (autumn_web::a11y::TextField::new("title")
                        .label("Title")
                        .label_class(label_class)
                        .class(input_class)
                        .value(form.field_value("title").unwrap_or_default())
                        .required()
                        .maxlength(300)
                        .aria_invalid(!form.errors_for("title").is_empty())
                        .described_by("title-error"))
                }

                div {
                    (field_errors("url", form))
                    (autumn_web::a11y::TextField::new("url")
                        .input_type("url")
                        .label("Link URL (optional)")
                        .label_class(label_class)
                        .class(input_class)
                        .value(form.field_value("url").unwrap_or_default())
                        .aria_invalid(!form.errors_for("url").is_empty())
                        .described_by("url-error"))
                }

                div {
                    (field_errors("body", form))
                    (autumn_web::a11y::TextArea::new("body")
                        .label("Text (optional for link posts)")
                        .label_class(label_class)
                        .class(input_class)
                        .rows(8)
                        .value(form.field_value("body").unwrap_or_default())
                        .aria_invalid(!form.errors_for("body").is_empty())
                        .described_by("body-hint"))
                    p id="body-hint" class="text-xs text-gray-400 mt-1" {
                        "Markdown is supported: **bold**, `code`, > quotes, lists and links. "
                        "Raw HTML and images are removed when the post is displayed."
                    }
                }

                (autumn_web::a11y::Button::new("Post")
                    .submit()
                    .class("w-full bg-orange-500 text-white py-2 rounded font-medium \
                            hover:bg-orange-600 transition-colors"))
            }))
        }
    }
}

/// Render one field's validation messages, in the element its control points
/// at with `aria-describedby`.
///
/// `role="alert"` makes the message announced when it appears after an htmx
/// swap; the stable `{field}-error` id is what makes it announced on focus for
/// a full-page 422 too.
fn field_errors<T>(field: &str, form: &ChangesetForm<T>) -> Markup {
    let errors = form.errors_for(field);
    html! {
        div id=(format!("{field}-error")) {
            @for message in errors {
                p class="text-red-600 text-xs mb-1" role="alert" { (message) }
            }
        }
    }
}

/// The submit form's shape.
///
/// `subreddit_id` is a `String`, not an `i64`, on purpose: this struct is a
/// *form*, and a form field's job is to round-trip whatever the browser sent so
/// the page can be re-rendered with it. Typing it as `i64` would make an empty
/// or garbled select a hard 400 — the user's title and body discarded with it —
/// instead of the inline "Choose a community" this renders. The conversion to
/// `i64` happens in `into_new`, after validation.
#[derive(serde::Deserialize, serde::Serialize, validator::Validate, Clone, Default)]
pub struct SubmitPostForm {
    #[serde(default)]
    #[validate(custom(function = "validate_subreddit_choice"))]
    pub subreddit_id: String,
    #[validate(
        length(min = 1, max = 300, message = "Title must be 1-300 characters"),
        custom(function = "validate_sluggable_title")
    )]
    pub title: String,
    #[serde(default)]
    #[validate(custom(function = "validate_optional_url"))]
    pub url: String,
    #[serde(default)]
    pub body: String,
}

impl SubmitPostForm {
    /// The chosen community, once validation has proved the field parses.
    fn subreddit_id(&self) -> i64 {
        self.subreddit_id.trim().parse().unwrap_or_default()
    }

    /// The link URL, or `None` for a text post.
    fn url(&self) -> Option<String> {
        let trimmed = self.url.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    }
}

fn validate_subreddit_choice(value: &str) -> Result<(), validator::ValidationError> {
    match value.trim().parse::<i64>() {
        Ok(id) if id > 0 => Ok(()),
        _ => Err(validator::ValidationError::new("subreddit_id")
            .with_message("Choose a community".into())),
    }
}

/// A title has to survive `slugify` — "***" is 3 characters long and still
/// produces an empty slug, which would give the post an unreachable URL.
fn validate_sluggable_title(value: &str) -> Result<(), validator::ValidationError> {
    if slugify(value).is_empty() {
        return Err(validator::ValidationError::new("title")
            .with_message("Title must contain at least one letter or number".into()));
    }
    Ok(())
}

/// The URL field is optional, so "empty" is valid; anything else must be an
/// absolute http/https URL. Rejecting other schemes here is defence in depth
/// for the rendered link, which already carries `rel="noopener noreferrer"`.
fn validate_optional_url(value: &str) -> Result<(), validator::ValidationError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    match url::Url::parse(trimmed) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => Ok(()),
        _ => Err(validator::ValidationError::new("url")
            .with_message("Enter a full http:// or https:// URL, or leave this empty".into())),
    }
}

/// Create a post.
///
/// The changeset round-trip in full (`docs/guide/forms.md`): `into_valid()`
/// either hands back the validated form or hands back the **form itself**,
/// still carrying the user's title, body and URL alongside the per-field
/// errors. The failure arm re-renders `submit_form_markup` with a 422, so a
/// rejected submission does not cost the author their draft — which is exactly
/// what the old `AutumnError::unprocessable_msg("Title must be 1-300
/// characters")` did.
///
/// It also works with **no JavaScript**: nothing in that markup is an `hx-*`
/// attribute, so a scripting-disabled browser posts the form normally and
/// renders the 422 body as the page.
#[secured]
#[post("/submit")]
pub async fn submit(
    State(state): State<AppState>,
    session: Session,
    csrf: CsrfToken,
    mut db: Db,
    _repo: PgPostRepository,
    flash: Flash,
    // The body extractor is last, because it consumes the request body — every
    // extractor after it would have nothing left to read. See
    // docs/guide/extractors.md.
    form: ChangesetForm<SubmitPostForm>,
) -> AutumnResult<Response> {
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

    let valid = match form.into_valid() {
        Ok(valid) => valid,
        Err(rejected) => {
            let subs = all_subreddits(&mut db).await?;
            return Ok((
                http::StatusCode::UNPROCESSABLE_ENTITY,
                layout(
                    "Create a Post",
                    Some(author_username.as_str()),
                    Some(csrf.token()),
                    submit_form_markup(&rejected, &subs, None),
                ),
            )
                .into_response());
        }
    };

    let title = valid.title.trim().to_string();
    let base_slug = slugify(&title);
    let url = valid.url();
    let subreddit_id = valid.subreddit_id();

    // Look up the subreddit slug for redirect. A community that vanished
    // between rendering the form and submitting it is a 404, not a field
    // error — nothing the author can fix by editing the form.
    let sub: Subreddit = subreddits::table
        .find(subreddit_id)
        .select(Subreddit::as_select())
        .first(&mut *db)
        .await
        .map_err(|_| AutumnError::not_found_msg("Subreddit not found"))?;

    // Ensure unique slug within this subreddit by appending a suffix
    let slug = unique_slug(&base_slug, subreddit_id, &mut db).await?;

    // NOT trimmed, unlike the title. The body is Markdown source, and leading
    // whitespace is syntax: four spaces make a CommonMark code block, and
    // trailing spaces make a hard line break. Trimming it silently reformats
    // what the author wrote — and this app stores the source and renders at
    // display time precisely so the author's original survives editing.
    let body = valid.body.clone();
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

            // `AfterBegin`, not `BeforeEnd`. The live row is a new-content
            // affordance shown at the top, NOT the position a reload would
            // give it — an OOB swap targets a fixed place in the DOM and
            // cannot compare ranks, so no swap method can insert into a ranked
            // listing correctly.
            //
            // The listing orders by `(hot_rank DESC, id DESC)`, and a new post
            // has the default `hot_rank` of 0.0 with the highest id. It is
            // therefore genuinely first only among the 0.0-ranked posts: any
            // post `calculate_hot_rank` has already scored positive (see
            // `tasks.rs` — `score / (age_hours + 2)^1.5`, positive for any
            // positive score) sorts ABOVE it, and a reload will move the new
            // row down past them. Appending it to the bottom would be wrong in
            // the other direction, and wrong in every community rather than
            // just the ones with upvoted posts.
            //
            // The same reason this stream is wired on page 1 only: a live
            // insert cannot maintain an exact page slice either. The list
            // transiently shows `size + 1` rows until the next load, and the
            // row pushed past the boundary also appears on page 2. That is
            // offset pagination's inherent instability under concurrent
            // inserts (see docs/guide/pagination.md), which a live feed makes
            // visible rather than causes — a feed that must stay exact in both
            // order and slice wants cursor pagination over a fixed key, not a
            // paginated ranked slice.
            let _ = sse_state.broadcast().publish_oob(
                &format!("posts:r/{}", sse_sub_slug),
                &sse_post.dom_id(),
                &autumn_web::htmx::OobSwap::Target(
                    autumn_web::htmx::OobMethod::AfterBegin,
                    "#posts-list".to_string(),
                ),
                &sse_post.render_fragment(),
            );
        })
        .await;

    flash.success("Post created.").await;
    Ok(Redirect::to(&super::subreddits::__autumn_path_show(&sub.slug)).into_response())
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

/// A post's detail page.
///
/// This is the SEO case the attribute alone cannot serve. `og_type` and the
/// Twitter card type are the same for every post, so they sit on the
/// attribute. The title, the description, and the canonical URL come from the
/// row, so the handler refines the builder after it reads the post.
///
/// The canonical URL matters here more than on any other page. `/posts/{id}`
/// redirects to this route, and htmx and share links add query strings, so one
/// post has many addresses. The canonical tag names the one true address.
///
/// See `docs/guide/seo.md`.
#[allow(clippy::too_many_lines)] // Template-heavy function
#[get(
    "/r/{sub_slug}/posts/{post_slug}",
    // `og_type = "article"` is the right Open Graph type for a post. The card
    // stays `summary` until the app has a share image to point `og_image` at.
    seo(og_type = "article", twitter_card = "summary")
)]
// Every argument is a distinct extractor -- path, session, CSRF token, CSRF
// field name, connection, repository, flags, flash. An axum handler's arguments
// ARE its request-state declaration; bundling them into a struct would only
// move the same list one level down.
#[allow(clippy::too_many_arguments)]
pub async fn show(
    Path((sub_slug, post_slug)): Path<(String, String)>,
    seo: SeoMeta,
    session: Session,
    csrf: CsrfToken,
    // The widget's hidden input has to carry the field name `CsrfLayer` will
    // look for. It scans a URL-encoded body for the CONFIGURED name only, so an
    // app that set `security.csrf.form_field` would otherwise render a thread
    // whose very first submit is a 403.
    csrf_field: CsrfFormField,
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

    // Eager-load the post's author and its tags (#1324, many-to-many through
    // `post_tags`) -- replacing the per-row author lookup and what would
    // otherwise be a hand-rolled `post_tags` join query. For a post with M
    // tags this is a fixed 2 extra queries, never `1 + M`.
    let mut loaded = repo
        .on_primary()
        .preload(vec![post], Post::preload().author().tags())
        .await?;
    let post = loaded.remove(0);
    let author = post.author()?;
    let post_tags = post.tags()?;

    // Threaded comments (#1367). One call, whatever the nesting depth: the
    // whole live thread comes back nested, in stable order, with author names
    // resolved -- no hand-written join, no N+1 walk, no sort to write. What
    // used to be a `has_many(Comment)` preload plus a 30-line render plus a
    // 188-line `routes/comments.rs` is this and the widget below.
    let comment_thread = repo.comment_thread(post.id).await?;

    // The widget posts to the one framework-mounted comment route, keyed on
    // this model's `commentable_type`. `return_to` is the no-JS round trip:
    // with htmx the reply swaps the thread in place, without it the browser
    // comes straight back to this page.
    //
    // The dom id and action come from the framework, not from here: the router
    // re-renders this same region after every reply, and an id of our own
    // devising would be replaced by the router's on the first htmx swap, so
    // every later swap would miss.
    let post_path = __autumn_path_show(&sub.slug, &post.slug);
    let comments_config = autumn_web::commentable::CommentsConfig::default();
    let mut comment_config = CommentThread::from_spec(
        autumn_web::commentable::thread_dom_id(Post::COMMENTABLE_TYPE, post.id),
        autumn_web::commentable::thread_action(&comments_config, Post::COMMENTABLE_TYPE, post.id),
        Post::commentable_spec(),
    )
    .label("Post comments")
    .empty_text("No comments yet. Start the conversation!")
    .return_to(&post_path);
    if current_user.is_some() {
        comment_config = comment_config
            .csrf_token(csrf.token())
            .csrf_field(csrf_field.0.clone());
    } else {
        comment_config = comment_config
            .read_only()
            .sign_in_prompt("Log in to comment.");
    }

    let viewer_id = current_user_id
        .as_ref()
        .and_then(|id| id.parse::<i64>().ok());
    let is_author = viewer_id.is_some_and(|id| id == post.author_id);

    // The viewer's own vote, so the detail page's control renders pressed
    // (#1362). One indexed lookup on the edge table, and only here — feeds pass
    // `None` to avoid an N+1. `db` was dropped above and `preload` has already
    // returned, so this is this request's only live checkout.
    //
    // `on_primary()`: the no-JS vote flow lands here via a 303 redirect — a
    // fresh GET with no read-your-writes pin — so on a lagging replica the
    // viewer's *own just-cast* vote could render unpressed. Their own vote is
    // the one read on this page where staleness is user-visible and wrong;
    // it is a single point lookup, and every heavy read above stays
    // replica-routed.
    let current_vote = match viewer_id {
        Some(uid) => repo.on_primary().reaction_of(uid, post.id).await?,
        None => None,
    };

    // Refine the attribute defaults with this post's own values. `og_type`
    // and the Twitter card type stay as the attribute declared them.
    // `og:title` and `og:description` fall back to `title` and `description`,
    // so this app sets each value one time.
    let seo = crate::seo::with_canonical(
        seo.title(format!(
            "{} \u{2022} r/{} \u{2022} Autumn Reddit",
            post.title, sub.name
        ))
        .description(
            crate::seo::summarize(&post.body, 155)
                .or_else(|| post.url.clone())
                .unwrap_or_else(|| format!("A post in r/{}.", sub.name)),
        ),
        &post_path,
    );

    // Consume the flash only after all fallible work above.
    let flash_html = flash.render().await;
    Ok(layout_with_seo(
        seo,
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
                    (vote_controls(post.id, post.score, current_vote, Some(&csrf)))
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
                        // Rich text (#1255). `post.body` is whatever a
                        // stranger typed into a <textarea>, so it is rendered
                        // through `render_user_content` — Markdown with
                        // raw-HTML passthrough disabled, a curated tag
                        // allowlist, an http/https/mailto/tel scheme
                        // allowlist, `rel="noopener noreferrer nofollow"`
                        // forced on every surviving link, and images dropped
                        // to their alt text so a post cannot beacon a
                        // reader's IP to a third-party host.
                        //
                        // The sanitizing happens at RENDER time, not at write
                        // time: the database keeps the author's original
                        // source so an edit shows them what they typed, and
                        // tightening the allowlist later protects every post
                        // already stored rather than only new ones. See
                        // docs/guide/rich-text.md.
                        @if !post.body.is_empty() {
                            div class="prose max-w-none text-gray-700" {
                                (autumn_web::markdown::render_user_content(&post.body))
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
                                      aria-label="Post tags"
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

            // Comments (#1367). `comment_thread` renders the nested list AND an
            // inline reply form on every node, posting to the framework's
            // generic comment route -- with htmx it swaps the thread in place,
            // and without any JavaScript at all it is an ordinary form POST
            // that comes back here via `return_to`.
            // Deliberately no count here. An htmx reply swaps ONLY the widget's
            // own region (`hx-target` / `outerHTML`), so anything rendered
            // outside it -- a heading like this one -- would still show the
            // pre-reply number until a full page load. A stale count next to a
            // freshly posted comment is worse than no count; the listings show
            // `comment_count`, and those are always full loads.
            h2 class="font-semibold text-gray-700 mb-2" { "Comments" }
            (comment_thread_widget(&comment_config, &CommentView::from_thread(&comment_thread)))
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

    let existing = ChangesetForm::blank(
        EditPostForm {
            title: post.title.clone(),
            // The stored value is the author's ORIGINAL Markdown source, not
            // the sanitized HTML the show page renders — which is why the
            // sanitizing happens at render time. Editing a post shows the
            // author what they typed.
            body: post.body.clone(),
        },
        csrf.token(),
    );

    Ok(layout(
        &format!("Edit: {}", post.title),
        current_user.as_deref(),
        Some(csrf.token()),
        edit_form_markup(&existing, &sub_slug, &post_slug),
    ))
}

/// The edit form's markup — shared by the GET route and by `update`'s 422 path,
/// the same way `submit_form_markup` is. See `docs/guide/forms.md`.
fn edit_form_markup(form: &ChangesetForm<EditPostForm>, sub_slug: &str, post_slug: &str) -> Markup {
    let input_class = "w-full border border-gray-300 rounded px-3 py-2 text-sm \
                       focus:outline-none focus:ring-2 focus:ring-orange-400";
    let label_class = "block text-sm font-medium text-gray-700 mb-1";

    html! {
        div class="max-w-2xl mx-auto" {
            h1 class="text-2xl font-bold mb-6" { "Edit Post" }
            (form.form_tag(&paths::update(sub_slug, post_slug), "post", html! {
                div {
                    (field_errors("title", form))
                    (autumn_web::a11y::TextField::new("title")
                        .label("Title")
                        .label_class(label_class)
                        .class(input_class)
                        .value(form.field_value("title").unwrap_or_default())
                        .required()
                        .maxlength(300)
                        .aria_invalid(!form.errors_for("title").is_empty())
                        .described_by("title-error"))
                }
                div {
                    (field_errors("body", form))
                    (autumn_web::a11y::TextArea::new("body")
                        .label("Text")
                        .label_class(label_class)
                        .class(input_class)
                        .rows(8)
                        .value(form.field_value("body").unwrap_or_default())
                        .aria_invalid(!form.errors_for("body").is_empty())
                        .described_by("body-hint"))
                    p id="body-hint" class="text-xs text-gray-400 mt-1" {
                        "Markdown is supported: **bold**, `code`, > quotes, lists and links. "
                        "Raw HTML and images are removed when the post is displayed."
                    }
                }
                div class="flex gap-3" {
                    (autumn_web::a11y::Button::new("Save")
                        .submit()
                        .class("px-6 py-2 bg-orange-500 text-white rounded font-medium \
                                hover:bg-orange-600 transition-colors"))
                    (autumn_web::a11y::Link::new(
                        paths::show(sub_slug, post_slug),
                        "Cancel",
                    ).class("px-6 py-2 text-gray-500 hover:text-gray-700"))
                }
            }))
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize, validator::Validate, Clone, Default)]
pub struct EditPostForm {
    #[validate(
        length(min = 1, max = 300, message = "Title must be 1-300 characters"),
        custom(function = "validate_sluggable_title")
    )]
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
    csrf: CsrfToken,
    mut db: Db,
    repo: PgPostRepository,
    flash: Flash,
    form: ChangesetForm<EditPostForm>,
) -> AutumnResult<Response> {
    let post =
        load_post_and_authorize(&state, &session, &mut db, &sub_slug, &post_slug, "update").await?;
    let current_user = session.get("username").await;

    let valid = match form.into_valid() {
        Ok(valid) => valid,
        Err(rejected) => {
            return Ok((
                http::StatusCode::UNPROCESSABLE_ENTITY,
                layout(
                    "Edit Post",
                    current_user.as_deref(),
                    Some(csrf.token()),
                    edit_form_markup(&rejected, &sub_slug, &post_slug),
                ),
            )
                .into_response());
        }
    };

    let title = valid.title.trim().to_string();
    let base_slug = slugify(&title);
    // Ensure unique slug within subreddit, excluding the current post
    let new_slug = unique_slug_excluding(&base_slug, post.subreddit_id, post.id, &mut db).await?;

    let changes = crate::models::UpdatePost {
        title: Patch::Set(title),
        slug: Patch::Set(new_slug.clone()),
        // Not trimmed, for the same reason as the create path: this is
        // Markdown source, where leading and trailing whitespace carry meaning.
        body: Patch::Set(valid.body.clone()),
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
    Ok(Redirect::to(&paths::show(&sub_slug, &new_slug)).into_response())
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

    // Audit this moderation action. The acting principal is auto-attributed
    // from the request scope via `Current::actor()` — the auth layer published
    // it when `#[secured]` resolved the session — so we never re-extract the
    // session just to answer "who deleted this post?". Best-effort: an audit
    // sink hiccup must not fail the delete the user already performed.
    let actor = autumn_web::current::Current::actor().unwrap_or_else(|| "unknown".to_string());
    let _ = autumn_web::audit::write_from_state(
        &state,
        AuditEvent::new(
            actor,
            "post.delete",
            post.id.to_string(),
            None,
            AuditStatus::Success,
        ),
    )
    .await;

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

    // ── Typed a11y form primitives (#1706) ─────────────────────────

    fn blank_submit_form() -> ChangesetForm<SubmitPostForm> {
        ChangesetForm::without_csrf(SubmitPostForm::default())
    }

    fn rejected_submit_form() -> ChangesetForm<SubmitPostForm> {
        // A submission that fails every rule: no community, a title that
        // slugifies to nothing, and a URL that is not http(s).
        let submitted = SubmitPostForm {
            subreddit_id: String::new(),
            title: "***".to_owned(),
            url: "javascript:alert(1)".to_owned(),
            body: "kept".to_owned(),
        };
        ChangesetForm::from_changeset(submitted.into_changeset())
    }

    #[test]
    fn every_submit_control_carries_a_real_label() {
        let rendered = submit_form_markup(&blank_submit_form(), &[], None).into_string();

        for field in ["subreddit_id", "title", "url", "body"] {
            assert!(
                rendered.contains(&format!(r#"for="{field}""#)),
                "field `{field}` must have an associated <label>; rendered: {rendered}"
            );
        }
    }

    #[test]
    fn the_community_picker_does_not_preselect_a_real_community() {
        // Without an empty placeholder the browser auto-selects the first
        // option, `required` is satisfied without the author choosing, and the
        // post lands silently in whichever community sorts first.
        let subs = vec![Subreddit {
            id: 7,
            name: "aardvarks".to_owned(),
            slug: "aardvarks".to_owned(),
            description: String::new(),
            creator_id: 1,
            subscriber_count: 0,
            comment_count: 0,
            created_at: chrono::NaiveDateTime::default(),
        }];
        let rendered = submit_form_markup(&blank_submit_form(), &subs, None).into_string();

        assert!(
            rendered.contains(r#"<option value="" disabled"#)
                || rendered.contains(r#"<option value="" selected disabled"#),
            "the picker needs an empty disabled placeholder; rendered: {rendered}"
        );
        assert!(
            !rendered.contains(r#"<option value="7" selected"#),
            "a real community must not be preselected on a blank form; rendered: {rendered}"
        );
    }

    #[test]
    fn the_submit_form_works_with_javascript_disabled() {
        let rendered = submit_form_markup(&blank_submit_form(), &[], None).into_string();

        assert!(
            !rendered.contains("hx-"),
            "the submit form must not depend on htmx; rendered: {rendered}"
        );
        assert!(
            rendered.contains(r#"method="post""#),
            "an ordinary form POST is the no-JavaScript path; rendered: {rendered}"
        );
    }

    #[test]
    fn the_submit_form_carries_its_own_csrf_token() {
        // The load-bearing half of the no-JavaScript path: with scripting on,
        // the framework's `autumn-htmx-csrf.js` shim can send the token as a
        // header; with scripting off, this hidden input is the only thing that
        // gets a plain form POST past `CsrfLayer`. `form_tag` emits it.
        let with_token = ChangesetForm::blank(SubmitPostForm::default(), "tok-123");
        let rendered = submit_form_markup(&with_token, &[], None).into_string();

        assert!(
            rendered.contains(r#"name="_csrf""#) && rendered.contains("tok-123"),
            "the form must carry a hidden CSRF input; rendered: {rendered}"
        );
    }

    #[test]
    fn a_rejected_submission_keeps_the_authors_input_and_wires_its_errors() {
        let rendered = submit_form_markup(&rejected_submit_form(), &[], None).into_string();

        // The draft survives the 422 — the whole point of the changeset.
        assert!(
            rendered.contains("kept"),
            "the body the author typed must come back; rendered: {rendered}"
        );
        // Each invalid control is marked, and points at its own message.
        for field in ["subreddit_id", "title", "url"] {
            assert!(
                rendered.contains(&format!(r#"aria-describedby="{field}-error""#)),
                "field `{field}` must reference its message element; rendered: {rendered}"
            );
            assert!(
                rendered.contains(&format!(r#"id="{field}-error""#)),
                "field `{field}` must render the element it references; rendered: {rendered}"
            );
        }
        assert!(
            rendered.contains(r#"aria-invalid="true""#),
            "an invalid control must say so; rendered: {rendered}"
        );
        assert!(
            rendered.contains(r#"role="alert""#),
            "messages must be announced; rendered: {rendered}"
        );
    }

    #[test]
    fn a_valid_submission_passes_every_rule() {
        let form = SubmitPostForm {
            subreddit_id: "7".to_owned(),
            title: "Ferris arrives".to_owned(),
            url: "https://example.com/ferris".to_owned(),
            body: "Hello".to_owned(),
        };
        let valid = ChangesetForm::without_csrf(form)
            .into_valid()
            .unwrap_or_else(|_| panic!("this submission is valid"));

        assert_eq!(valid.subreddit_id(), 7);
        assert_eq!(valid.url().as_deref(), Some("https://example.com/ferris"));
    }

    #[test]
    fn an_empty_url_field_is_a_text_post_not_an_error() {
        let form = SubmitPostForm {
            subreddit_id: "1".to_owned(),
            title: "A text post".to_owned(),
            url: "   ".to_owned(),
            body: "Body".to_owned(),
        };
        let valid = ChangesetForm::without_csrf(form)
            .into_valid()
            .unwrap_or_else(|_| panic!("an optional URL may be blank"));

        assert!(valid.url().is_none());
    }

    // ── Rich text (#1255) ──────────────────────────────────────────

    /// Leading whitespace in a Markdown body is syntax, not padding — which is
    /// why neither write path trims the body the way both trim the title. Four
    /// spaces make a CommonMark code block; trimming the source turns the
    /// author's code sample into a paragraph, silently and permanently, since
    /// this app stores the source rather than the rendered HTML.
    #[test]
    fn leading_indentation_is_markdown_syntax_and_must_survive_storage() {
        let authored = "    let x = 1;";

        let kept = autumn_web::markdown::render_user_content(authored).into_string();
        assert!(
            kept.contains("<code>"),
            "four-space indentation must still render as a code block: {kept}"
        );

        // What a `.trim()` on the stored body would have produced instead.
        let trimmed = autumn_web::markdown::render_user_content(authored.trim()).into_string();
        assert!(
            !trimmed.contains("<code>"),
            "this is the regression being guarded against — if trimming stops \
             changing the output, this test no longer proves anything: {trimmed}"
        );
    }

    #[test]
    fn a_post_body_is_rendered_as_sanitized_markdown() {
        let rendered =
            autumn_web::markdown::render_user_content("**bold** and [a link](https://example.com)")
                .into_string();

        assert!(
            rendered.contains("<strong>bold</strong>"),
            "rendered: {rendered}"
        );
        // Every surviving link is defanged for a user-content context.
        assert!(
            rendered.contains(r#"rel="noopener noreferrer nofollow""#),
            "rendered: {rendered}"
        );
    }

    #[test]
    fn a_post_body_cannot_smuggle_script_or_beacon_an_image() {
        let hostile = "<script>alert(1)</script>\n\n![tracker](https://evil.example/pixel.gif)\n\n[x](javascript:alert(1))";
        let rendered = autumn_web::markdown::render_user_content(hostile).into_string();

        assert!(!rendered.contains("<script"), "rendered: {rendered}");
        assert!(!rendered.contains("<img"), "rendered: {rendered}");
        assert!(!rendered.contains("javascript:"), "rendered: {rendered}");
    }
}
