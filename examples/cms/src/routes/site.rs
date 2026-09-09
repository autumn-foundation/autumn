//! The shared request context: repositories, settings, the signed-in user, and
//! the theme chrome every front-end page renders inside.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::extract::FromRequestParts;
use autumn_web::reexports::http::request::Parts;
use autumn_web::security::{CsrfFormField, CsrfToken};

use crate::models::{MenuItem, Post, Term, User};
use crate::repositories::{
    MenuItemRepository as _, MenuRepository as _, PgAttachmentRepository, PgCommentRepository,
    PgMenuItemRepository, PgMenuRepository, PgPostMetaRepository, PgPostRepository,
    PgSiteOptionRepository, PgTermRepository, PgUserRepository, PgWidgetRepository,
    PostRepository as _, TermRepository as _, UserRepository as _,
};
use crate::settings::{SITE_SCOPE, Settings, cached_settings};
use crate::taxonomy::{PgPostTermLinkRepository, PostTermLinkRepository as _};
use crate::theme::{self, Chrome, NavNode, SidebarData};

/// Every repository a page might need, in one extractor.
///
/// A repository holds the connection **pool**, not a connection — it acquires
/// one per call and returns it — so bundling nine of them costs nothing at
/// request time. This is the reason the front end resolves the current user and
/// the settings through repositories rather than through a `Db` extractor:
/// `Db` pins a connection for the whole request, and a page that also touched a
/// repository would hold two at once, halving effective concurrency and
/// deadlocking at pool saturation. Handlers take `Db` only for the
/// [`crate::content`] operations that genuinely need one transaction.
pub struct Repos {
    pub users: PgUserRepository,
    pub posts: PgPostRepository,
    pub post_meta: PgPostMetaRepository,
    pub terms: PgTermRepository,
    pub comments: PgCommentRepository,
    pub attachments: PgAttachmentRepository,
    pub options: PgSiteOptionRepository,
    pub menus: PgMenuRepository,
    pub menu_items: PgMenuItemRepository,
    pub widgets: PgWidgetRepository,
    pub post_term_links: PgPostTermLinkRepository,
    /// The pool the repositories were built from.
    ///
    /// Held so the listing screens can run a real set-based query — ordering,
    /// filtering, counting and pagination in SQL — which the repository
    /// codegen has no finder for. A connection is acquired for the duration of
    /// that one query and returned, so this does not pin one for the request
    /// the way a `Db` extractor would.
    pool: Option<autumn_web::db::Pool<autumn_web::reexports::diesel_async::AsyncPgConnection>>,
}

impl FromRequestParts<AppState> for Repos {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // The earliest point the running configuration is in hand — `bootstrap`
        // happens before the app is built. Every path that allocates a slug
        // passes through here, and after the first request this is a `OnceLock`
        // hit. See `content::observe_probe_paths`.
        crate::content::observe_probe_paths(&state.config());

        Ok(Self {
            users: PgUserRepository::from_request_parts(parts, state).await?,
            posts: PgPostRepository::from_request_parts(parts, state).await?,
            post_meta: PgPostMetaRepository::from_request_parts(parts, state).await?,
            terms: PgTermRepository::from_request_parts(parts, state).await?,
            comments: PgCommentRepository::from_request_parts(parts, state).await?,
            attachments: PgAttachmentRepository::from_request_parts(parts, state).await?,
            options: PgSiteOptionRepository::from_request_parts(parts, state).await?,
            menus: PgMenuRepository::from_request_parts(parts, state).await?,
            menu_items: PgMenuItemRepository::from_request_parts(parts, state).await?,
            widgets: PgWidgetRepository::from_request_parts(parts, state).await?,
            post_term_links: PgPostTermLinkRepository::from_request_parts(parts, state).await?,
            pool: state.pool().cloned(),
        })
    }
}

/// The most terms a sidebar widget renders.
///
/// A widget is a glance at the taxonomy, not a listing of it, and it renders on
/// every public page — so the bound belongs in the query rather than in the
/// template.
const WIDGET_TERM_LIMIT: i64 = 50;

/// The most widgets one sidebar renders.
///
/// A sidebar is chrome on every public page; past a couple of dozen entries it
/// has stopped being a sidebar and started being the page. The Appearance
/// screen reads the same bound, so what an administrator manages is what
/// visitors see.
pub const MAX_SIDEBAR_WIDGETS: i64 = 30;

impl Repos {
    /// Borrow a pooled connection for one query.
    pub async fn conn(
        &self,
    ) -> AutumnResult<
        autumn_web::reexports::diesel_async::pooled_connection::deadpool::Object<
            autumn_web::reexports::diesel_async::AsyncPgConnection,
        >,
    > {
        self.pool
            .as_ref()
            .ok_or_else(|| AutumnError::service_unavailable_msg("No database pool configured"))?
            .get()
            .await
            .map_err(|error| AutumnError::service_unavailable_msg(error.to_string()))
    }

    /// The resolved site settings (memoized for 60 seconds).
    pub async fn settings(&self) -> AutumnResult<Settings> {
        cached_settings(SITE_SCOPE, &self.options).await
    }

    /// The signed-in user, if any.
    ///
    /// A session naming a user that no longer exists resolves to `None` rather
    /// than an error: deleting an account must log its holder out, not 500
    /// every page they load.
    pub async fn current_user(&self, session: &Session) -> AutumnResult<Option<User>> {
        let Some(raw) = session.get("user_id").await else {
            return Ok(None);
        };
        let Ok(user_id) = raw.parse::<i64>() else {
            return Ok(None);
        };
        // Propagated, not swallowed. `.ok()` turned a pool or database failure
        // into "no such account", so a protected handler reported an
        // authentication failure and a public page rendered a signed-in
        // visitor as anonymous — both hiding a service fault behind a
        // plausible-looking answer. `Ok(None)` still means the account is gone.
        self.users.find_by_id(user_id).await
    }

    /// The signed-in user, or a 401.
    pub async fn require_user(&self, session: &Session) -> AutumnResult<User> {
        self.current_user(session)
            .await?
            .ok_or_else(|| AutumnError::unauthorized_msg("You must be signed in"))
    }

    /// The permalink for a post, resolving a page's ancestry when it has one.
    pub async fn permalink(&self, post: &Post, settings: &Settings) -> AutumnResult<String> {
        let ancestry = if post.post_type == "page" {
            self.page_ancestry(post).await?
        } else {
            Vec::new()
        };
        Ok(settings
            .permalink_structure
            .permalink(post, &ancestry, settings.zone()))
    }

    /// The slugs of a page's ancestors, outermost first.
    ///
    /// The walk is bounded: a `parent_id` cycle is only reachable by a direct
    /// write, but a page render must not hang on one.
    pub async fn page_ancestry(&self, post: &Post) -> AutumnResult<Vec<String>> {
        // Shares the constant with the write-path check in
        // `content::MAX_PAGE_DEPTH`, so the editor cannot store a hierarchy
        // deeper than the permalink builder will render — which would emit a
        // URL starting mid-tree that resolves to a 404.
        const MAX_DEPTH: usize = crate::content::MAX_PAGE_DEPTH;
        let mut slugs = Vec::new();
        let mut cursor = post.parent_id;
        let mut seen = vec![post.id];
        while let Some(parent_id) = cursor {
            if slugs.len() >= MAX_DEPTH || seen.contains(&parent_id) {
                break;
            }
            seen.push(parent_id);
            // A failed lookup must not read as "no parent": that truncates the
            // path, so `/about/team` is published as `/team` — a URL that 404s
            // or, worse, names different content, and that listings, feeds and
            // caches then carry.
            match self.posts.find_by_id(parent_id).await? {
                Some(parent) => {
                    slugs.push(parent.slug.clone());
                    cursor = parent.parent_id;
                }
                None => break,
            }
        }
        slugs.reverse();
        Ok(slugs)
    }

    /// Build the site chrome: the primary nav, the sidebar and the viewer.
    pub async fn chrome(
        &self,
        session: &Session,
        settings: &Settings,
        csrf: &Csrf,
    ) -> AutumnResult<Chrome> {
        Ok(Chrome {
            nav: self.nav_for("primary", settings).await?,
            footer_nav: self.nav_for("footer", settings).await?,
            sidebar: Some(self.sidebar(settings).await?),
            current_user: self.current_user(session).await?,
            settings: settings.clone(),
            csrf: csrf.input(),
        })
    }

    /// The menu assigned to the `primary` theme location, resolved to URLs.
    /// The menu assigned to a theme location, resolved to URLs.
    ///
    /// Takes the location rather than hard-coding `primary`: the Appearance
    /// screen offers `footer` too, and for as long as this only ever asked for
    /// one of them, assigning the other saved successfully and rendered
    /// nowhere.
    async fn nav_for(&self, location: &str, settings: &Settings) -> AutumnResult<Vec<NavNode>> {
        let Some(menu) = self
            .menus
            .find_by_location(location.to_owned())
            .await?
            .into_iter()
            .next()
        else {
            return Ok(Vec::new());
        };
        let items = self.menu_items.find_by_menu_id(menu.id).await?;

        // Every target loaded in two set queries — plus one per level of page
        // hierarchy — rather than one query per item and one more per ancestor.
        // This runs on *every* public page render and the item count is
        // whatever an editor has added, so the per-item shape made a large but
        // perfectly valid menu a few hundred round trips on the front page.
        let post_ids: Vec<i64> = items.iter().filter_map(|item| item.post_id).collect();
        let term_ids: Vec<i64> = items.iter().filter_map(|item| item.term_id).collect();
        let (posts, terms) = self
            .with_conn(async move |conn| {
                let posts = crate::content::posts_with_ancestors(conn, &post_ids).await?;
                let terms = crate::content::terms_by_ids(conn, &term_ids).await?;
                Ok((posts, terms))
            })
            .await?;

        // An item whose target is no longer public is dropped, not rendered
        // with a dead link. A menu names a post by id, and that post can be
        // drafted, scheduled, made private or trashed afterwards — the menu has
        // no idea, so every page of the site carried a link to a 404 until
        // somebody noticed and edited the menu.
        //
        // Dropping a parent drops its children with it: `build_nav` assembles
        // by `parent_id`, so leaving them behind floats a sub-item to the top
        // level, which is a stranger outcome than the link disappearing. The
        // renderer draws two levels, so one orphan pass covers it.
        let renderable: std::collections::HashSet<i64> = items
            .iter()
            .filter(|item| menu_item_is_visible(item, &posts, &terms))
            .map(|item| item.id)
            .collect();
        let items: Vec<MenuItem> = items
            .into_iter()
            .filter(|item| {
                renderable.contains(&item.id)
                    && item
                        .parent_id
                        .is_none_or(|parent| renderable.contains(&parent))
            })
            .collect();

        // Building the tree is a pure function over those maps and needs no
        // database access per node.
        let resolved: std::collections::HashMap<i64, String> = items
            .iter()
            .map(|item| (item.id, menu_item_url(item, &posts, &terms, settings)))
            .collect();
        Ok(theme::build_nav(&items, &|item: &MenuItem| {
            resolved
                .get(&item.id)
                .cloned()
                .unwrap_or_else(|| "/".to_owned())
        }))
    }

    /// Render the primary sidebar.
    pub async fn sidebar(&self, settings: &Settings) -> AutumnResult<Markup> {
        // Ordered and bounded in SQL. This renders on every public page, and
        // widgets are added through an ordinary form with no cap — so the cost
        // of the whole site was a function of how many somebody had placed.
        let widgets = self
            .with_conn(async |conn| {
                crate::content::sidebar_widgets(conn, "primary", MAX_SIDEBAR_WIDGETS).await
            })
            .await?;
        if widgets.is_empty() {
            return Ok(html! {});
        }

        // Only load what the placed widgets actually need. A sidebar with one
        // text widget must not cost a posts query and two term queries.
        let kinds: Vec<crate::theme::WidgetKind> = widgets
            .iter()
            .filter_map(|w| crate::theme::WidgetKind::parse(&w.kind))
            .collect();
        let needs = |kind: crate::theme::WidgetKind| kinds.contains(&kind);

        let recent_posts = if needs(crate::theme::WidgetKind::RecentPosts) {
            let posts = self.published_posts("post", 20).await?;
            let mut out = Vec::with_capacity(posts.len());
            for post in &posts {
                out.push((post.title.clone(), self.permalink(post, settings).await?));
            }
            out
        } else {
            Vec::new()
        };
        // Bounded in SQL, and populated terms only. The sidebar renders on
        // every public page, so an unbounded finder here made otherwise
        // paginated pages cost the whole category table — and listed empty
        // terms whose archives have nothing to show. `populated_terms` applies
        // both the `post_count > 0` filter and the limit in the query.
        let categories = if needs(crate::theme::WidgetKind::Categories) {
            let mut conn = self.conn().await?;
            crate::content::populated_terms(&mut conn, "category", WIDGET_TERM_LIMIT).await?
        } else {
            Vec::new()
        };
        let tags = if needs(crate::theme::WidgetKind::TagCloud) {
            let mut conn = self.conn().await?;
            crate::content::populated_terms(&mut conn, "post_tag", WIDGET_TERM_LIMIT).await?
        } else {
            Vec::new()
        };

        Ok(theme::render_sidebar(&SidebarData {
            widgets,
            recent_posts,
            categories,
            tags,
        }))
    }

    /// Published posts of one type, newest first.
    ///
    /// `publish` only — `private`, `future`, `draft`, `pending` and `trash` are
    /// all excluded, so this is the one query every public listing is built
    /// from and no screen has to remember the filter.
    pub async fn published_posts(&self, post_type: &str, limit: i64) -> AutumnResult<Vec<Post>> {
        let mut conn = self.conn().await?;
        crate::content::recent_published_posts(&mut conn, post_type, limit).await
    }

    /// One page of published content of a type, plus the total, ordered and
    /// paginated by the database.
    pub async fn published_posts_page(
        &self,
        post_type: &str,
        offset: usize,
        limit: usize,
    ) -> AutumnResult<(Vec<Post>, usize)> {
        let mut conn = self.conn().await?;
        let (rows, total) = crate::content::published_posts_page(
            &mut conn,
            post_type,
            i64::try_from(offset).unwrap_or(0),
            i64::try_from(limit).unwrap_or(10),
        )
        .await?;
        Ok((rows, usize::try_from(total).unwrap_or(0)))
    }

    /// Run one operation on a connection held only for the duration of the call.
    ///
    /// The rule this enforces: **a handler never holds a pool connection across
    /// a repository call.** The repositories are pool-backed and acquire their
    /// own connection per call, so a handler that holds one (the `Db` extractor
    /// holds it from before the body runs until the response is returned) and
    /// then reaches for a repository needs *two* slots at once. With the shipped
    /// `pool_size = 10`, ten concurrent requests in that shape can each hold one
    /// slot while waiting for a second that only another of them could release,
    /// and none of them can make progress.
    ///
    /// Writing the checkout as a scope rather than a `let` is what makes it
    /// hard to get wrong: the connection cannot outlive the call, so a
    /// repository read added later cannot silently end up inside its lifetime.
    pub async fn with_conn<T, F>(&self, f: F) -> AutumnResult<T>
    where
        F: AsyncFnOnce(
            &mut autumn_web::reexports::diesel_async::AsyncPgConnection,
        ) -> AutumnResult<T>,
    {
        let mut conn = self.conn().await?;
        f(&mut conn).await
    }

    /// Save a post, allocating a slug that is free across the types sharing
    /// the bare URL path, and retrying if a concurrent write takes it first.
    ///
    /// Shared by the editor and the importer. `idx_posts_bare_path_slug` makes
    /// the invariant the database's, which means *every* insert path has to
    /// allocate through here — the importer did not, so restoring a page whose
    /// slug an existing post already held aborted the run part-way, after
    /// earlier rows had committed.
    pub async fn save_post_with_unique_slug(
        &self,
        new: crate::models::NewPost,
    ) -> AutumnResult<Post> {
        use crate::repositories::PostRepository as _;

        let desired = crate::hooks::normalize_slug(&new.slug, &new.title);
        for _ in 0..5 {
            let slug = {
                let mut conn = self.conn().await?;
                crate::content::ensure_unique_slug(
                    &mut conn,
                    &new.post_type,
                    &desired,
                    new.parent_id,
                    None,
                )
                .await?
            };
            let attempt = crate::models::NewPost {
                slug,
                ..new.clone()
            };
            match self.posts.save(&attempt).await {
                Ok(post) => return Ok(post),
                // Both slug indexes are retried, because either can be the one
                // a lost race reports. `ensure_unique_slug` picked a slug that
                // was free when it looked; a concurrent create that took it
                // first violates `idx_posts_type_slug` for a custom type and
                // may report either index for a `post`/`page`. Re-running
                // allocation is exactly the right response to both — the next
                // pass finds the taken slug and returns the `-2`. A conflict on
                // anything else (a primary key, say) is a different problem
                // that a suffix would not fix, and still propagates.
                Err(error)
                    if autumn_web::error::unique_violation_field(
                        &error,
                        crate::content::SLUG_COLLISION_INDEXES,
                    )
                    .is_some() =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        Err(AutumnError::conflict_msg(
            "Could not allocate a unique URL for this content; try a different title or slug",
        ))
    }

    /// The terms a post is filed under, across every taxonomy.
    ///
    /// Two queries: the filings, then the terms. A post carries a handful of
    /// terms, so loading them individually is bounded by the editor's patience
    /// rather than by the size of the site.
    pub async fn post_terms(&self, post_id: i64) -> AutumnResult<Vec<Term>> {
        let links = self.post_term_links.find_by_post_id(post_id).await?;
        let mut terms = Vec::with_capacity(links.len());
        for link in links {
            if let Some(term) = self.terms.find_by_id(link.term_id).await? {
                terms.push(term);
            }
        }
        Ok(terms)
    }

    /// The published posts filed under a term, newest first, for one page of an
    /// archive.
    ///
    /// The filings are read first and paginated *before* the posts are loaded,
    /// so the per-post lookups are bounded by the page size (at most 100) and
    /// not by how much content the term has accumulated. A derived finder
    /// taking a list of ids would collapse this to two queries; the repository
    /// codegen has no `IN`-shaped finder, and reaching for a raw connection
    /// here would mean holding a `Db` alongside these pool-backed repositories.
    pub async fn posts_in_term(
        &self,
        term_id: i64,
        offset: usize,
        limit: usize,
    ) -> AutumnResult<(Vec<Post>, usize)> {
        let mut conn = self.conn().await?;
        let (rows, total) = crate::content::published_posts_in_term(
            &mut conn,
            term_id,
            i64::try_from(offset).unwrap_or(0),
            i64::try_from(limit).unwrap_or(10),
        )
        .await?;
        Ok((rows, usize::try_from(total).unwrap_or(0)))
    }

    /// One page of an author's published posts, plus the total.
    pub async fn posts_by_author(
        &self,
        author_id: i64,
        offset: usize,
        limit: usize,
    ) -> AutumnResult<(Vec<Post>, usize)> {
        let mut conn = self.conn().await?;
        let (rows, total) = crate::content::published_posts_by_author(
            &mut conn,
            author_id,
            i64::try_from(offset).unwrap_or(0),
            i64::try_from(limit).unwrap_or(10),
        )
        .await?;
        Ok((rows, usize::try_from(total).unwrap_or(0)))
    }

    /// One page of a date archive, plus the total.
    pub async fn posts_in_period(
        &self,
        post_type: &str,
        from: chrono::NaiveDateTime,
        until: chrono::NaiveDateTime,
        offset: usize,
        limit: usize,
    ) -> AutumnResult<(Vec<Post>, usize)> {
        let mut conn = self.conn().await?;
        let (rows, total) = crate::content::published_posts_in_period(
            &mut conn,
            post_type,
            from,
            until,
            i64::try_from(offset).unwrap_or(0),
            i64::try_from(limit).unwrap_or(10),
        )
        .await?;
        Ok((rows, usize::try_from(total).unwrap_or(0)))
    }
}

/// Render a page inside the active theme's chrome.
pub async fn render(
    repos: &Repos,
    session: &Session,
    csrf: &Csrf,
    title: &str,
    content: Markup,
) -> AutumnResult<Markup> {
    let settings = repos.settings().await?;
    let chrome = repos.chrome(session, &settings, csrf).await?;
    Ok(theme::active_theme(&settings).layout(&chrome, title, content))
}

/// The CSRF token plus the configured field name to submit it under.
///
/// Bundled because every form needs both and getting either wrong fails the
/// same way: `CsrfLayer` scans the request body for the **configured** field
/// name (`security.csrf.form_field`, default `_csrf`), so a form that hardcodes
/// a different name submits a token the layer never looks for and 403s on its
/// first POST.
pub struct Csrf {
    /// `None` when `CsrfLayer` is not mounted — see the extractor below.
    token: Option<CsrfToken>,
    field: Option<CsrfFormField>,
}

impl FromRequestParts<AppState> for Csrf {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Both underlying extractors reject when `CsrfLayer` is not mounted,
        // which is exactly what `security.csrf.enabled = false` produces — a
        // supported configuration, and the default in `TestApp`. Failing here
        // would make every page in the application 500 under that setting,
        // which is a far worse outcome than rendering forms without a token
        // that nothing is going to check. So the absence is represented, not
        // treated as an error.
        Ok(Self {
            token: CsrfToken::from_request_parts(parts, state).await.ok(),
            field: CsrfFormField::from_request_parts(parts, state).await.ok(),
        })
    }
}

impl Csrf {
    /// The hidden input every `POST` form must carry.
    ///
    /// Renders nothing when CSRF is disabled, so a form is never asked to
    /// submit a field the layer is not there to validate.
    #[must_use]
    pub fn input(&self) -> Markup {
        match (&self.token, &self.field) {
            (Some(token), Some(field)) => html! {
                input type="hidden" name=(field.0) value=(token.token());
            },
            _ => html! {},
        }
    }

    /// The raw token, for a form that builds its own field. Empty when CSRF is
    /// disabled.
    #[must_use]
    pub fn token(&self) -> &str {
        self.token.as_ref().map_or("", CsrfToken::token)
    }
}

/// The one-time submit token, absent when `SubmitTokenLayer` is not mounted.
///
/// Same reasoning as [`Csrf`]: `security.submit_token.enabled = false` is a
/// supported configuration, and a hard failure there would make the
/// registration screen 500 rather than simply render without an at-most-once
/// guard it was told not to enforce.
pub struct Submit(Option<autumn_web::security::SubmitToken>);

impl FromRequestParts<AppState> for Submit {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(
            autumn_web::security::SubmitToken::from_request_parts(parts, state)
                .await
                .ok(),
        ))
    }
}

impl Submit {
    /// The token, or an empty string when the layer is not mounted.
    #[must_use]
    pub fn token(&self) -> &str {
        self.0
            .as_ref()
            .map_or("", autumn_web::security::SubmitToken::token)
    }
}

/// A post's permalink, built from an already-loaded map of ancestors.
///
/// The map-based twin of [`Repos::permalink`], for callers that have loaded a
/// batch: the sitemap and the navigation both render many posts at once, and
/// walking the ancestor chain a row at a time made each of them cost a query
/// per level per post.
#[must_use]
pub fn permalink_from(
    post: &Post,
    ancestors: &std::collections::HashMap<i64, Post>,
    settings: &Settings,
) -> String {
    let ancestry = if post.post_type == "page" {
        ancestry_from(post, ancestors)
    } else {
        Vec::new()
    };
    settings
        .permalink_structure
        .permalink(post, &ancestry, settings.zone())
}

/// A menu item's target: its post, its term, or its raw URL — in that order,
/// matching the three link kinds the menu editor offers.
///
/// Pure, over maps the caller loaded in bulk. It used to take `&self` and query
/// per item, which is what made a menu cost a round trip per entry on every
/// public render.
fn menu_item_url(
    item: &MenuItem,
    posts: &std::collections::HashMap<i64, Post>,
    terms: &std::collections::HashMap<i64, crate::models::Term>,
    settings: &Settings,
) -> String {
    if let Some(post) = item.post_id.and_then(|id| posts.get(&id)) {
        return permalink_from(post, posts, settings);
    }
    if let Some(term) = item.term_id.and_then(|id| terms.get(&id)) {
        return theme::term_url(term);
    }
    if item.url.trim().is_empty() {
        "/".to_owned()
    } else {
        item.url.clone()
    }
}

/// Whether a menu item still points at something a visitor can reach.
///
/// A raw URL is the author's own and is always kept — the menu editor accepts
/// anything there, including an off-site link. A post or term target is only
/// kept while the row exists and is publicly routable.
fn menu_item_is_visible(
    item: &MenuItem,
    posts: &std::collections::HashMap<i64, Post>,
    terms: &std::collections::HashMap<i64, crate::models::Term>,
) -> bool {
    if let Some(post_id) = item.post_id {
        return posts.get(&post_id).is_some_and(|post| {
            post.is_public() && crate::content::is_public_type(&post.post_type)
        });
    }
    if let Some(term_id) = item.term_id {
        return terms.contains_key(&term_id);
    }
    true
}

/// A page's ancestor slugs, outermost first, read from an already-loaded map.
///
/// The same bound and the same cycle guard as `Repos::page_ancestry`; an
/// ancestor missing from the map ends the walk, exactly as a missing row does
/// there.
fn ancestry_from(post: &Post, posts: &std::collections::HashMap<i64, Post>) -> Vec<String> {
    const MAX_DEPTH: usize = crate::content::MAX_PAGE_DEPTH;
    let mut slugs = Vec::new();
    let mut cursor = post.parent_id;
    let mut seen = vec![post.id];
    while let Some(parent_id) = cursor {
        if slugs.len() >= MAX_DEPTH || seen.contains(&parent_id) {
            break;
        }
        seen.push(parent_id);
        match posts.get(&parent_id) {
            Some(parent) => {
                slugs.push(parent.slug.clone());
                cursor = parent.parent_id;
            }
            None => break,
        }
    }
    slugs.reverse();
    slugs
}
