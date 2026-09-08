//! The public site — WordPress's template hierarchy, as one front controller.
//!
//! WordPress resolves a request by matching rewrite rules into a `WP_Query` and
//! then picking a template file by filename convention. The two halves are
//! independent, which is why a permalink change can 404 content that is
//! plainly still there.
//!
//! Here one catch-all handler resolves the path with
//! [`crate::permalinks::resolve`] — the same function whose round-trip against
//! the permalink *generator* is unit-tested — and dispatches to a screen. There
//! is exactly one place that decides what a URL means.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use serde::Deserialize;

use crate::capabilities::Capability;
use crate::content_types;
use crate::models::{Post, Term, User};
use crate::permalinks::Resolved;
use crate::repositories::{
    AttachmentRepository as _, PostRepository as _, TermRepository as _, UserRepository as _,
};
use crate::settings::Settings;
use crate::theme;

use super::site::{Csrf, Repos, render};

/// Whether a post's registered type is reachable on the public front end.
///
/// `Post::is_public` answers for the row's *status*; this answers for its
/// *type*. Both have to hold. The distinction matters on the entry points that
/// reach a row without going through the resolver's type-aware branches —
/// `/?p=<id>`, `/archives/<id>` and search — where a type registered
/// `public: false` would otherwise render in full.
fn is_publicly_routable(post: &Post) -> bool {
    content_types::find_post_type(&post.post_type).is_some_and(|registered| registered.public)
}

/// Whether `viewer` may reach `post` at all: its registered type is public, and
/// its status is either published or one this viewer has a reason to see.
///
/// Password protection is deliberately *not* part of this. An unlockable post
/// is reachable — that is what lets it render a password form — so the password
/// gate is a separate question asked after this one.
///
/// One predicate, two callers. `single_post` had this inline, and `unlock` —
/// which takes a bare post id from an unauthenticated request — had none of it,
/// so posting any existing id returned that row's canonical permalink in the
/// `Location` header. Iterating ids confirmed the existence of drafts, private
/// and trashed rows and disclosed their slugs and page ancestry, with a wrong
/// password and no session at all.
fn viewer_may_read(post: &Post, viewer: Option<&User>) -> bool {
    if !is_publicly_routable(post) {
        return false;
    }
    if post.is_public() {
        return true;
    }
    match (viewer, post.status.as_str()) {
        // A private post is readable by its author and by anyone holding
        // `read_private_posts` — the WordPress rule.
        (Some(user), "private") => {
            user.id == post.author_id || user.role().can(Capability::ReadPrivatePosts)
        }
        // A draft, pending or scheduled post is previewable by someone who
        // could edit it. Everyone else gets a 404 rather than a 403: a 403
        // would confirm that unpublished content exists at that URL.
        (Some(user), _) => {
            crate::capabilities::can_edit_post(user.role(), user.id, post.author_id, &post.status)
        }
        (None, _) => false,
    }
}

/// Query parameters every listing screen understands.
#[derive(Debug, Default, Deserialize)]
pub struct ListQueryParams {
    /// 1-based page number.
    #[serde(default)]
    pub page: Option<usize>,
    /// The `plain` permalink structure's post id (`/?p=123`).
    #[serde(default)]
    pub p: Option<i64>,
    /// The search term (`?s=…`), WordPress's own parameter name.
    #[serde(default)]
    pub s: Option<String>,
}

impl ListQueryParams {
    /// The zero-based offset for the requested page.
    fn offset(&self, per_page: i64) -> usize {
        let page = self.page.unwrap_or(1).max(1);
        (page - 1) * usize::try_from(per_page.max(1)).unwrap_or(10)
    }

    fn page_number(&self) -> usize {
        self.page.unwrap_or(1).max(1)
    }
}

// ── The front page ──────────────────────────────────────────────────────────

#[get("/")]
pub async fn front_page(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Query(params): Query<ListQueryParams>,
) -> AutumnResult<Response> {
    // `/?p=123` is the `plain` permalink structure. It is honoured whatever the
    // configured structure is, so links minted before a settings change keep
    // working.
    if let Some(post_id) = params.p
        && let Some(post) = repos.posts.find_by_id(post_id).await.ok().flatten()
        && is_publicly_routable(&post)
    {
        return single_post(&repos, &session, &csrf, post).await;
    }

    let settings = repos.settings().await?;

    // A configured static front page replaces the blog index, as in
    // Settings → Reading.
    // `single_post` enforces status *and* registered-type visibility, so a
    // front page whose type was later registered `public: false` falls through
    // to the blog index rather than being served at `/`.
    if let Some(page_id) = settings.front_page_id
        && let Some(page) = repos.posts.find_by_id(page_id).await.ok().flatten()
        && page.is_public()
        && is_publicly_routable(&page)
    {
        return single_post(&repos, &session, &csrf, page).await;
    }

    blog_index(&repos, &session, &csrf, &settings, &params).await
}

#[get("/search")]
pub async fn search(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    Query(params): Query<ListQueryParams>,
) -> AutumnResult<Response> {
    let settings = repos.settings().await?;
    let query = params.s.clone().unwrap_or_default();
    let trimmed = query.trim();

    // `search_page` rather than `search`: the unpaginated form loads every
    // matching row, so one broad query on a large site costs memory and
    // database work proportional to the whole corpus — from an unauthenticated
    // endpoint. The page size is the site's own `posts_per_page`.
    let per_page = u32::try_from(settings.posts_per_page.max(1)).unwrap_or(10);
    let page_number = u32::try_from(params.page_number()).unwrap_or(1);
    // Visibility is a predicate of the SQL, not a filter applied to the page
    // that comes back. Filtering afterwards paginates the *unrestricted* result
    // set: a page can come back empty while public matches sit on later pages,
    // and `total_elements` would count — and so disclose the number of — draft
    // and non-public-type matches.
    let public_types: Vec<String> = content_types::all_post_types()
        .into_iter()
        .filter(|registered| registered.public)
        .map(|registered| registered.slug.to_owned())
        .collect();
    let (results, total) = if trimmed.is_empty() {
        (Vec::new(), 0usize)
    } else {
        let mut conn = repos.conn().await?;
        // Runs against the `search_vector` GIN index (see the model's
        // `#[searchable]` columns), so this is a real ranked full-text query
        // rather than a table scan of `LIKE '%…%'`.
        crate::content::search_published(
            &mut conn,
            trimmed,
            &public_types,
            i64::from(page_number.saturating_sub(1)) * i64::from(per_page),
            i64::from(per_page),
        )
        .await?
    };

    let mut cards = Vec::with_capacity(results.len());
    for post in &results {
        cards.push((post.clone(), repos.permalink(post, &settings).await?));
    }

    let theme = theme::active_theme(&settings);
    let body = html! {
        h1 class="text-2xl font-bold mb-2" {
            @if trimmed.is_empty() { "Search" } @else { "Search results for “" (trimmed) "”" }
        }
        p class="text-sm text-gray-500 mb-6" {
            (autumn_web::format::pluralize(
                i64::try_from(total).unwrap_or(i64::MAX), "result"))
        }
        form action="/search" method="get" role="search" class="flex gap-2 mb-8 max-w-md" {
            label for="search-field" class="sr-only" { "Search" }
            input #search-field type="search" name="s" value=(trimmed)
                  class="flex-1 border rounded px-3 py-2";
            button type="submit"
                   class="px-4 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700" {
                "Search"
            }
        }
        @for (post, url) in &cards {
            (theme.post_card(post, url, &settings))
        }
        @if cards.is_empty() && !trimmed.is_empty() {
            p class="text-gray-500" { "Nothing matched. Try a different phrase." }
        }
    };
    Ok(render(&repos, &session, &csrf, "Search", body)
        .await?
        .into_response())
}

// ── The catch-all ───────────────────────────────────────────────────────────

/// Absorb the browser's automatic favicon request.
///
/// The framework already answers `/favicon.ico` with `204 No Content` from its
/// fallback 404 handler, exactly so an unconfigured site does not log a console
/// error on every page load. A catch-all defeats that: `/{*path}` matches every
/// unmatched path, so the fallback never runs and the request lands in the
/// front controller, which resolves it as content, finds none, and renders a
/// themed 404.
///
/// A literal route wins over the wildcard and restores the framework's own
/// answer. Replace it with a real icon when the site has one — the point here
/// is that the catch-all must not silently swallow this.
#[get("/favicon.ico")]
pub async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// Everything that is not a reserved prefix.
///
/// Registered **last** so `/admin`, `/api`, `/feed`, `/login` and the static
/// assets keep their own handlers; axum prefers a literal path over a wildcard,
/// but the ordering is stated here because it is load-bearing rather than
/// incidental.
#[get("/{*path}")]
pub async fn dispatch(
    repos: Repos,
    session: Session,
    csrf: Csrf,
    State(state): State<AppState>,
    headers: autumn_web::reexports::http::HeaderMap,
    Path(path): Path<String>,
    Query(params): Query<ListQueryParams>,
) -> AutumnResult<Response> {
    let settings = repos.settings().await?;
    match crate::permalinks::resolve(&path) {
        Resolved::FrontPage => blog_index(&repos, &session, &csrf, &settings, &params).await,

        Resolved::SingleById { id } => {
            match repos
                .posts
                .find_by_id(id)
                .await
                .ok()
                .flatten()
                .filter(is_publicly_routable)
            {
                Some(post) => single_post(&repos, &session, &csrf, post).await,
                None => not_found(&repos, &session, &csrf).await,
            }
        }

        Resolved::Single { post_type, slug } => {
            match find_visible(&repos, &post_type, &slug).await? {
                Some(post) => single_post(&repos, &session, &csrf, post).await,
                // A bare segment is ambiguous: it may be a top-level page
                // rather than a post. Only a TOP-LEVEL page, though — a nested
                // page is addressed by its full path, so `/team` must not
                // resolve to `/about/team`. Without the `parent_id` check, two
                // pages named `team` under different parents would both answer
                // at `/team`, and which one you got would depend on row order.
                None => match find_visible(&repos, "page", &slug).await? {
                    Some(page) if page.parent_id.is_none() => {
                        single_post(&repos, &session, &csrf, page).await
                    }
                    _ => not_found(&repos, &session, &csrf).await,
                },
            }
        }

        Resolved::Page { path } => {
            // A nested path is either a page hierarchy or a dated post
            // permalink. Resolve the page ancestry first — it is the more
            // specific claim — then fall back to the last segment as a post.
            if let Some(page) = resolve_page_path(&repos, &path).await? {
                return single_post(&repos, &session, &csrf, page).await;
            }
            let Some(last) = path.last() else {
                return not_found(&repos, &session, &csrf).await;
            };
            match find_visible(&repos, "post", last).await? {
                Some(post) => single_post(&repos, &session, &csrf, post).await,
                None => not_found(&repos, &session, &csrf).await,
            }
        }

        // A term's feed. Served from here rather than from a route of its own:
        // `/{taxonomy_base}/{slug}/feed` and this handler's `/{*path}` are a
        // route-shape conflict the router rejects at build time.
        Resolved::TermFeed { taxonomy, slug } => {
            super::feed::term_feed(&repos, &state, &headers, &taxonomy, slug).await
        }

        Resolved::TermArchive { taxonomy, slug } => {
            let term = repos
                .terms
                .find_by_slug(slug)
                .await?
                .into_iter()
                .find(|t| t.taxonomy == taxonomy);
            match term {
                Some(term) => {
                    term_archive(&repos, &session, &csrf, &settings, &term, &params).await
                }
                None => not_found(&repos, &session, &csrf).await,
            }
        }

        Resolved::AuthorArchive { username } => {
            let author = repos
                .users
                .find_by_username(username)
                .await?
                .into_iter()
                .next();
            match author {
                Some(author) => {
                    author_archive(&repos, &session, &csrf, &settings, &author, &params).await
                }
                None => not_found(&repos, &session, &csrf).await,
            }
        }

        Resolved::PostTypeArchive { post_type } => {
            post_type_archive(&repos, &session, &csrf, &settings, &post_type, &params).await
        }

        Resolved::DateArchive { year, month, day } => {
            date_archive(
                &repos, &session, &csrf, &settings, year, month, day, &params,
            )
            .await
        }

        Resolved::NotFound => not_found(&repos, &session, &csrf).await,
    }
}

// ── Screens ─────────────────────────────────────────────────────────────────

async fn blog_index(
    repos: &Repos,
    session: &Session,
    csrf: &Csrf,
    settings: &Settings,
    params: &ListQueryParams,
) -> AutumnResult<Response> {
    let per_page = usize::try_from(settings.posts_per_page.max(1)).unwrap_or(10);
    let (page_posts, total) = repos
        .published_posts_page("post", params.offset(settings.posts_per_page), per_page)
        .await?;

    let body = listing(
        repos,
        settings,
        "",
        &page_posts,
        total,
        per_page,
        params,
        "/",
    )
    .await?;
    Ok(render(repos, session, csrf, "", body)
        .await?
        .into_response())
}

async fn term_archive(
    repos: &Repos,
    session: &Session,
    csrf: &Csrf,
    settings: &Settings,
    term: &Term,
    params: &ListQueryParams,
) -> AutumnResult<Response> {
    let per_page = usize::try_from(settings.posts_per_page.max(1)).unwrap_or(10);
    let (posts, total) = repos
        .posts_in_term(term.id, params.offset(settings.posts_per_page), per_page)
        .await?;

    let taxonomy = content_types::find_taxonomy(&term.taxonomy);
    let heading = taxonomy.map_or_else(
        || term.name.clone(),
        |t| format!("{}: {}", t.singular, term.name),
    );
    let base = theme::term_url(term);
    let body = html! {
        @if !term.description.trim().is_empty() {
            div class="mb-6 text-gray-600" { (theme::render_content(&term.description)) }
        }
        (listing(repos, settings, &heading, &posts, total, per_page, params, &base).await?)
    };
    Ok(render(repos, session, csrf, &heading, body)
        .await?
        .into_response())
}

async fn author_archive(
    repos: &Repos,
    session: &Session,
    csrf: &Csrf,
    settings: &Settings,
    author: &User,
    params: &ListQueryParams,
) -> AutumnResult<Response> {
    let per_page = usize::try_from(settings.posts_per_page.max(1)).unwrap_or(10);
    let (page_posts, total) = repos
        .posts_by_author(author.id, params.offset(settings.posts_per_page), per_page)
        .await?;

    let heading = format!("Posts by {}", author.public_name());
    let base = format!("/author/{}", author.username);
    let body = html! {
        @if !author.bio.trim().is_empty() {
            div class="mb-6 text-gray-600" { (theme::render_content(&author.bio)) }
        }
        (listing(repos, settings, &heading, &page_posts, total, per_page, params, &base).await?)
    };
    Ok(render(repos, session, csrf, &heading, body)
        .await?
        .into_response())
}

async fn post_type_archive(
    repos: &Repos,
    session: &Session,
    csrf: &Csrf,
    settings: &Settings,
    post_type: &str,
    params: &ListQueryParams,
) -> AutumnResult<Response> {
    let Some(registered) = content_types::find_post_type(post_type) else {
        return not_found(repos, session, csrf).await;
    };
    let per_page = usize::try_from(settings.posts_per_page.max(1)).unwrap_or(10);
    let (page_posts, total) = repos
        .published_posts_page(post_type, params.offset(settings.posts_per_page), per_page)
        .await?;

    let heading = registered.plural.to_owned();
    let base = format!("/{}", registered.archive_base);
    let body = listing(
        repos,
        settings,
        &heading,
        &page_posts,
        total,
        per_page,
        params,
        &base,
    )
    .await?;
    Ok(render(repos, session, csrf, &heading, body)
        .await?
        .into_response())
}

#[allow(clippy::too_many_arguments)]
async fn date_archive(
    repos: &Repos,
    session: &Session,
    csrf: &Csrf,
    settings: &Settings,
    year: i32,
    month: Option<u32>,
    day: Option<u32>,
    params: &ListQueryParams,
) -> AutumnResult<Response> {
    // Half-open bounds, so the filter is a pair of index-usable comparisons on
    // `published_at` rather than a per-row date decomposition.
    let Some((from, until)) = archive_bounds(year, month, day) else {
        return not_found(repos, session, csrf).await;
    };
    let per_page = usize::try_from(settings.posts_per_page.max(1)).unwrap_or(10);
    let (page_posts, total) = repos
        .posts_in_period(
            "post",
            from,
            until,
            params.offset(settings.posts_per_page),
            per_page,
        )
        .await?;

    let (heading, base) = match (month, day) {
        (Some(m), Some(d)) => (
            format!("{year}-{m:02}-{d:02}"),
            format!("/{year}/{m:02}/{d:02}"),
        ),
        (Some(m), None) => (format!("{year}-{m:02}"), format!("/{year}/{m:02}")),
        _ => (year.to_string(), format!("/{year}")),
    };
    let heading = format!("Archive: {heading}");
    let body = listing(
        repos,
        settings,
        &heading,
        &page_posts,
        total,
        per_page,
        params,
        &base,
    )
    .await?;
    Ok(render(repos, session, csrf, &heading, body)
        .await?
        .into_response())
}

/// One post or page, with its comment thread.
async fn single_post(
    repos: &Repos,
    session: &Session,
    csrf: &Csrf,
    post: Post,
) -> AutumnResult<Response> {
    let settings = repos.settings().await?;
    let viewer = repos.current_user(session).await?;

    // Type visibility is checked HERE rather than at each entry point. It was
    // a caller's job before, which is exactly why it kept being missed one
    // route at a time — `/?p=`, `/archives/<id>`, search, and the configured
    // front page each reach a row without passing through the resolver's
    // type-aware branches. A type registered `public: false` has no public
    // route by definition, so no caller of this function should be able to
    // render one, whatever path it arrived by.
    if !viewer_may_read(&post, viewer.as_ref()) {
        return not_found(repos, session, csrf).await;
    }

    let terms = repos.post_terms(post.id).await?;
    let author = repos.users.find_by_id(post.author_id).await.ok().flatten();
    let featured = match post.featured_media_id {
        Some(id) => repos.attachments.find_by_id(id).await.ok().flatten(),
        None => None,
    };

    // Password-protected content. The unlock is stored per-session so a reader
    // does not re-enter it on every page of the thread.
    let unlocked = if post.is_password_protected() {
        session
            .get(&format!("post_unlock_{}", post.id))
            .await
            .is_some_and(|stored| stored == post.password)
    } else {
        true
    };

    let comments_open = post.comment_status == "open"
        && content_types::find_post_type(&post.post_type).is_some_and(|t| t.supports_comments);
    let thread = if comments_open || post.comment_count > 0 {
        // Rendering an existing thread on a closed post is deliberate: closing
        // comments stops new ones, it does not retract the conversation.
        super::comments::render_thread(repos, session, csrf, &post).await?
    } else {
        html! {}
    };

    let title = theme::render_title(&post.title);
    let body = html! {
        article {
            header class="mb-6" {
                h1 class="text-3xl font-bold tracking-tight mb-2" { (title) }
                p class="text-sm text-gray-500 flex flex-wrap gap-2" {
                    @if let Some(published) = post.published_at {
                        time datetime=(published.and_utc().to_rfc3339()) {
                            (settings.format_date(published))
                        }
                    }
                    @if let Some(author) = &author {
                        span { "·" }
                        a href=(format!("/author/{}", author.username))
                          class="hover:underline" { (author.public_name()) }
                    }
                    @if !post.is_public() {
                        span class="px-1.5 py-0.5 bg-amber-100 text-amber-800 rounded text-xs \
                                    uppercase tracking-wide" {
                            (post.status) " preview"
                        }
                    }
                }
            }

            @if let Some(media) = &featured {
                @if media.is_image() {
                    img src=(format!("/media/{}", media.slug)) alt=(media.alt_text)
                        class="w-full rounded mb-6";
                }
            }

            @if unlocked {
                div class="prose max-w-none" { (theme::render_content(&post.body)) }
            } @else {
                (password_form(&post, csrf))
            }

            @if !terms.is_empty() && unlocked {
                footer class="mt-8 pt-4 border-t border-gray-100 flex flex-wrap gap-2" {
                    @for term in &terms {
                        a href=(theme::term_url(term))
                          class="px-2 py-0.5 bg-gray-100 rounded text-xs text-gray-700 \
                                 hover:bg-gray-200" {
                            (term.name)
                        }
                    }
                }
            }
        }
        @if unlocked { (thread) }
    };

    Ok(render(repos, session, csrf, &post.title, body)
        .await?
        .into_response())
}

fn password_form(post: &Post, csrf: &Csrf) -> Markup {
    html! {
        div class="border border-gray-200 rounded p-6 bg-gray-50" {
            h2 class="font-semibold mb-2" { "This content is password protected" }
            p class="text-sm text-gray-600 mb-4" {
                "Enter the password to read it."
            }
            form action=(format!("/unlock/{}", post.id)) method="post" class="flex gap-2 max-w-sm" {
                (csrf.input())
                label for="post-password" class="sr-only" { "Password" }
                input #post-password type="password" name="password" required
                      class="flex-1 border rounded px-3 py-2";
                button type="submit"
                       class="px-4 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700" {
                    "Unlock"
                }
            }
        }
    }
}

/// Accept a password-protected post's password and remember it for the session.
#[derive(Deserialize)]
pub struct UnlockForm {
    pub password: String,
}

// Unauthenticated, and every request is one password guess. The shipped
// configuration has no global limiter, so without a per-route bound a client
// that has picked up the (reusable) CSRF token can guess a content password as
// fast as it can open connections — and the redirect target starts serving the
// body on success, which is a free oracle telling it when to stop. The key is
// the client address rather than the post id so that spreading the attack
// across many protected posts does not buy a fresh budget for each.
#[post("/unlock/{id}")]
#[throttle(limit = 10, per = "1m", key = "ip")]
pub async fn unlock(
    repos: Repos,
    session: Session,
    Path(id): Path<i64>,
    Form(form): Form<UnlockForm>,
) -> AutumnResult<Redirect> {
    let post = repos
        .posts
        .find_by_id(id)
        .await?
        .ok_or_else(|| AutumnError::not_found_msg("No such post"))?;

    // The same reachability gate `single_post` applies, and for the same
    // reason: the response carries the row's canonical permalink, so answering
    // for a row this viewer could not have loaded discloses the slug and page
    // ancestry of hidden content to anyone willing to iterate ids.
    let viewer = repos.current_user(&session).await?;
    if !viewer_may_read(&post, viewer.as_ref()) {
        return Err(AutumnError::not_found_msg("No such post"));
    }

    let settings = repos.settings().await?;

    // A wrong password simply does not unlock; the redirect re-renders the form.
    // There is no error message on purpose — the form is the message, and a
    // "wrong password" response on a public URL is a free oracle for guessing.
    if post.is_password_protected() && form.password == post.password {
        session
            .insert(format!("post_unlock_{}", post.id), post.password.clone())
            .await;
    }
    Ok(Redirect::to(&repos.permalink(&post, &settings).await?))
}

// ── Shared pieces ───────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn listing(
    repos: &Repos,
    settings: &Settings,
    heading: &str,
    posts: &[Post],
    total: usize,
    per_page: usize,
    params: &ListQueryParams,
    base_path: &str,
) -> AutumnResult<Markup> {
    let theme = theme::active_theme(settings);
    let mut cards = Vec::with_capacity(posts.len());
    for post in posts {
        cards.push((post.clone(), repos.permalink(post, settings).await?));
    }

    let page = params.page_number();
    let last_page = total.div_ceil(per_page.max(1)).max(1);

    Ok(html! {
        @if !heading.is_empty() {
            h1 class="text-2xl font-bold mb-6" { (heading) }
        }
        @if cards.is_empty() {
            p class="text-gray-500 py-8" { "Nothing published here yet." }
        }
        @for (post, url) in &cards {
            (theme.post_card(post, url, settings))
        }
        @if last_page > 1 {
            nav aria-label="Pagination" class="flex items-center justify-between mt-8 text-sm" {
                @if page > 1 {
                    a href=(page_url(base_path, page - 1))
                      class="text-indigo-700 hover:underline" { "← Newer" }
                } @else {
                    span {}
                }
                span class="text-gray-500" { "Page " (page) " of " (last_page) }
                @if page < last_page {
                    a href=(page_url(base_path, page + 1))
                      class="text-indigo-700 hover:underline" { "Older →" }
                } @else {
                    span {}
                }
            }
        }
    })
}

/// The half-open `[from, until)` range a `/YYYY[/MM[/DD]]` archive covers.
///
/// `None` for a date that does not exist (the resolver already rejects an
/// out-of-range month or day, so this is the belt to that braces).
fn archive_bounds(
    year: i32,
    month: Option<u32>,
    day: Option<u32>,
) -> Option<(chrono::NaiveDateTime, chrono::NaiveDateTime)> {
    use chrono::{Days, Months, NaiveDate};

    let start = NaiveDate::from_ymd_opt(year, month.unwrap_or(1), day.unwrap_or(1))?;
    let end = match (month, day) {
        (Some(_), Some(_)) => start.checked_add_days(Days::new(1))?,
        (Some(_), None) => start.checked_add_months(Months::new(1))?,
        _ => NaiveDate::from_ymd_opt(year.checked_add(1)?, 1, 1)?,
    };
    Some((start.and_hms_opt(0, 0, 0)?, end.and_hms_opt(0, 0, 0)?))
}

/// A listing's page link. Page 1 drops the parameter so the canonical URL of a
/// first page has no query string.
fn page_url(base_path: &str, page: usize) -> String {
    if page <= 1 {
        base_path.to_owned()
    } else {
        format!("{base_path}?page={page}")
    }
}

/// Find a post by type and slug, excluding trashed content.
async fn find_visible(repos: &Repos, post_type: &str, slug: &str) -> AutumnResult<Option<Post>> {
    Ok(repos
        .posts
        .find_by_slug(slug.to_owned())
        .await?
        .into_iter()
        .find(|post| post.post_type == post_type && post.status != "trash"))
}

/// Walk a page path (`/about/team`) down the `parent_id` chain.
///
/// Every segment must match a page whose parent is the previous one, so
/// `/team` does not resolve just because a page called `team` exists somewhere
/// else in the tree.
async fn resolve_page_path(repos: &Repos, path: &[String]) -> AutumnResult<Option<Post>> {
    let mut parent: Option<i64> = None;
    let mut current: Option<Post> = None;
    for segment in path {
        let candidates = repos.posts.find_by_slug(segment.clone()).await?;
        let matched = candidates
            .into_iter()
            .find(|p| p.post_type == "page" && p.parent_id == parent && p.status != "trash");
        match matched {
            Some(page) => {
                parent = Some(page.id);
                current = Some(page);
            }
            None => return Ok(None),
        }
    }
    Ok(current)
}

/// The 404 screen, rendered inside the theme rather than as a bare framework
/// error page — a missing post is an ordinary outcome on a content site.
async fn not_found(repos: &Repos, session: &Session, csrf: &Csrf) -> AutumnResult<Response> {
    let body = html! {
        h1 class="text-2xl font-bold mb-3" { "Not found" }
        p class="text-gray-600 mb-6" {
            "That page does not exist. It may have been moved or unpublished."
        }
        form action="/search" method="get" role="search" class="flex gap-2 max-w-md" {
            label for="notfound-search" class="sr-only" { "Search" }
            input #notfound-search type="search" name="s" placeholder="Search the site…"
                  class="flex-1 border rounded px-3 py-2";
            button type="submit"
                   class="px-4 py-2 bg-indigo-600 text-white rounded hover:bg-indigo-700" {
                "Search"
            }
        }
    };
    let page = render(repos, session, csrf, "Not found", body).await?;
    Ok((StatusCode::NOT_FOUND, page).into_response())
}
