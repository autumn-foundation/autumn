//! The REST API — WordPress's `/wp-json/wp/v2`, typed.
//!
//! Read endpoints are public and serve only published content, so they can back
//! a static-site build or a mobile client with no credentials. Writes require a
//! session and the same capabilities the admin screens check — there is one
//! authorization model, not an admin one and an API one that drift apart.

use autumn_web::AutumnResult;
use autumn_web::prelude::*;
use serde::{Deserialize, Serialize};

use crate::capabilities::Capability;
use crate::content_types;
use crate::models::{NewPost, Post};
use crate::plugins::{Action, do_action};
use crate::repositories::PostRepository as _;

use super::site::Repos;

/// The requested page, clamped to something the offset arithmetic can hold.
///
/// `page` is an unbounded `usize` from the query string, and multiplying it by
/// a page size overflows long before a real corpus does — a panic under
/// overflow checks, a wrapped and unrelated page in release. See
/// `front::MAX_PAGE`.
fn clamped_page(page: Option<usize>) -> usize {
    page.unwrap_or(1).clamp(1, super::front::MAX_PAGE)
}

/// Whether a post's registered type is reachable on the public front end.
///
/// `Post::is_public` answers for the row's status; this answers for its type.
/// Both have to hold before anything about the row leaves this module.
fn is_publicly_routable(post: &Post) -> bool {
    content_types::find_post_type(&post.post_type).is_some_and(|registered| registered.public)
}

/// A post as the API returns it.
///
/// A hand-written projection rather than the model itself: `Post` carries
/// `password` (the plaintext gate for protected content) and `body` (which a
/// protected post must not hand out), and serializing the model directly would
/// publish both.
#[derive(Debug, Serialize)]
pub struct PostView {
    pub id: i64,
    pub post_type: String,
    pub title: String,
    pub slug: String,
    pub excerpt: String,
    /// Absent for password-protected posts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub status: String,
    pub author_id: i64,
    pub comment_count: i64,
    pub published_at: Option<chrono::NaiveDateTime>,
    pub url: String,
    pub password_protected: bool,
}

impl PostView {
    fn from(post: &Post, url: String) -> Self {
        Self {
            id: post.id,
            post_type: post.post_type.clone(),
            title: post.title.clone(),
            slug: post.slug.clone(),
            excerpt: crate::theme::render_excerpt(post),
            body: if post.is_password_protected() {
                None
            } else {
                Some(post.body.clone())
            },
            status: post.status.clone(),
            author_id: post.author_id,
            comment_count: post.comment_count,
            published_at: post.published_at,
            url,
            password_protected: post.is_password_protected(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct PostQuery {
    #[serde(default)]
    pub post_type: Option<String>,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub page: Option<usize>,
    #[serde(default)]
    pub per_page: Option<usize>,
}

/// `GET /api/v1/posts` — published content only.
#[get("/api/v1/posts")]
pub async fn list_posts(
    repos: Repos,
    Query(query): Query<PostQuery>,
) -> AutumnResult<Json<Vec<PostView>>> {
    let settings = repos.settings().await?;
    let post_type = query.post_type.unwrap_or_else(|| "post".to_owned());
    // `public: false` on a registered type means "no public route". Accepting
    // it here would let a caller enumerate exactly the content the type
    // declares should not be reachable — `Post::is_public` only answers for
    // the row's *status*, which is a different question.
    if !content_types::find_post_type(&post_type).is_some_and(|t| t.public) {
        return Err(AutumnError::not_found_msg(format!(
            "No public post type `{post_type}`"
        )));
    }

    // The page size is clamped before it reaches any query: an unbounded
    // `per_page` is a denial-of-service by query string.
    let per_page = query.per_page.unwrap_or(20).clamp(1, 100);
    // Both branches take the same offset. Without one, a site with more than
    // `per_page` matches had no request that could reach the older rows at all
    // — the endpoint was bounded but not navigable.
    let offset = clamped_page(query.page)
        .saturating_sub(1)
        .saturating_mul(per_page);
    let posts = match query.search.as_deref().map(str::trim) {
        Some(term) if !term.is_empty() => {
            // The same bounded, visibility-aware search the front end uses.
            // The generated `search()` is unbounded, and truncating after it
            // returns means a broad unauthenticated query still costs the whole
            // match set in database and application memory.
            let mut conn = repos.conn().await?;
            crate::content::search_published(
                &mut conn,
                term,
                std::slice::from_ref(&post_type),
                i64::try_from(offset).unwrap_or(0),
                i64::try_from(per_page).unwrap_or(20),
            )
            .await?
            .0
        }
        _ => {
            repos
                .published_posts_page(&post_type, offset, per_page)
                .await?
                .0
        }
    };

    let mut out = Vec::with_capacity(posts.len());
    for post in &posts {
        out.push(PostView::from(
            post,
            repos.permalink(post, &settings).await?,
        ));
    }
    Ok(Json(out))
}

/// `GET /api/v1/posts/{id}`.
#[get("/api/v1/posts/{id}")]
pub async fn get_post(repos: Repos, Path(id): Path<i64>) -> AutumnResult<Json<PostView>> {
    let settings = repos.settings().await?;
    let post = repos
        .posts
        .find_by_id(id)
        .await?
        // Unpublished content is a 404 on the public API, not a 403: a 403
        // would confirm that a draft exists at that id. A published row of a
        // non-public *type* is equally out of bounds — status and type
        // visibility are separate questions and both have to pass.
        .filter(|post| post.is_public() && is_publicly_routable(post))
        .ok_or_else(|| AutumnError::not_found_msg("No such post"))?;
    let url = repos.permalink(&post, &settings).await?;
    Ok(Json(PostView::from(&post, url)))
}

/// What `POST /api/v1/posts` accepts.
#[derive(Debug, Deserialize)]
pub struct CreatePostBody {
    pub title: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub excerpt: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub post_type: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

/// `POST /api/v1/posts` — create content.
///
/// Requires a session; the API deliberately has no second credential type,
/// because a token system that bypasses the capability checks is how an API
/// becomes the weak side of an application's authorization.
#[post("/api/v1/posts")]
pub async fn create_post(
    repos: Repos,
    session: Session,
    Json(body): Json<CreatePostBody>,
) -> AutumnResult<(StatusCode, Json<PostView>)> {
    let user = repos.require_user(&session).await?;
    if !user.role().can(Capability::EditPosts) {
        return Err(AutumnError::forbidden_msg(
            "Your role cannot create content",
        ));
    }

    let requested = body.status.unwrap_or_else(|| "draft".to_owned());
    // Same clamp the admin editor applies: without `publish_posts`, the API
    // cannot publish either.
    let status = if user.role().can(Capability::PublishPosts) {
        requested
    } else if requested == "pending" {
        "pending".to_owned()
    } else {
        "draft".to_owned()
    };

    let settings = repos.settings().await?;
    let post_type = body.post_type.unwrap_or_else(|| "post".to_owned());
    let registered = content_types::find_post_type(&post_type)
        .ok_or_else(|| AutumnError::bad_request_msg(format!("Unknown post type `{post_type}`")))?;

    // The site-wide default only applies to a type that has comments at all.
    // A `page` registers `supports_comments: false`, and storing `open` on one
    // would let a direct request add comments the editor never offered and the
    // theme then renders.
    let comment_status = if registered.supports_comments {
        settings.default_comment_status.clone()
    } else {
        "closed".to_owned()
    };

    // `private` is only reachable through a `draft -> private` transition, so
    // creating it directly is refused by `PostHooks::before_create` — which
    // left the API unable to perform a creation its own capability check
    // allows and the admin editor supports. Create as a draft and transition,
    // exactly as the admin path does, keeping the state machine the single
    // authority on which statuses are reachable how.
    // `future` is rejected rather than accepted: `CreatePostBody` carries no
    // publish date, so the row would be created with `published_at = NULL` and
    // the sweep — which selects `published_at <= now` — could never see it.
    // Returning 201 for a post that can never publish itself is worse than
    // refusing the status.
    if status == "future" {
        return Err(AutumnError::unprocessable_msg(
            "Scheduling is not available through the API; create a draft and publish it",
        ));
    }
    // A deferred transition is checked *before* the insert. `private` is
    // reached by transitioning a draft, and that edge carries the `can_publish`
    // guard — so a request with an empty title used to commit a draft and then
    // fail, leaving a row behind that the client never asked for and did not
    // learn about, with each retry allocating another suffixed slug. The guard
    // depends only on the content being submitted, so asking first costs
    // nothing and makes the failure clean.
    crate::content::guard_deferred_transition(&status, &body.title)?;
    let deferred_transition = (status == "private").then(|| status.clone());
    let initial_status = if deferred_transition.is_some() {
        "draft".to_owned()
    } else {
        status.clone()
    };

    // The insert and any deferred transition are one transaction.
    //
    // They were not: the insert went through the pool-backed allocator and the
    // transition followed behind an unwind. An unwind covers a failed
    // statement, not a cancelled request or a process that stops existing —
    // either of which left a draft the caller never received and never asked
    // for, with each retry consuming another suffixed slug. The connection-
    // scoped allocator makes one transaction possible; the admin editor and the
    // importer are on it already, and this was the path left behind.
    //
    // Still through the shared allocator rather than a direct save:
    // `idx_posts_bare_path_slug` and `idx_posts_type_slug` make slug uniqueness
    // the database's invariant, so an API client creating a second item with an
    // existing title would otherwise hit a constraint error where the editor
    // and the importer get the usual `-2` suffix.
    let draft = NewPost {
        post_type: registered.slug.to_owned(),
        title: body.title,
        slug: body.slug,
        excerpt: body.excerpt,
        body: body.body,
        status: initial_status,
        author_id: user.id,
        parent_id: None,
        featured_media_id: None,
        menu_order: 0,
        comment_status,
        password: String::new(),
        sticky: false,
        published_at: None,
    };
    let deferred_transition_fired = deferred_transition.is_some();
    let created = repos
        .with_conn(async |conn| {
            use autumn_web::reexports::diesel_async::AsyncConnection as _;
            conn.transaction(async move |conn| {
                let created = crate::content::insert_post_with_unique_slug(conn, draft).await?;
                // The same snapshot the admin's create path records, inside the
                // same transaction. Without it a post created through the API
                // started with no history at all, so "restore this revision"
                // meant something different depending on which supported write
                // surface made the content — and the first *edit* would then
                // snapshot a body nobody could see the predecessor of.
                if crate::content::type_supports_revisions(&created.post_type) {
                    crate::content::record_initial_revision(conn, &created).await?;
                }
                match deferred_transition {
                    Some(target) => {
                        crate::content::transition_status(
                            conn,
                            created.id,
                            &target,
                            Some(user.id),
                            Some(&user),
                        )
                        .await
                    }
                    None => Ok::<_, AutumnError>(created),
                }
            })
            .await
        })
        .await?;

    // The same actions the admin, import and scheduler paths fire, and only
    // once the post is actually complete — a listener that reads it back must
    // not see a state that is about to be unwound.
    do_action(Action::PostSaved, created.id);
    if deferred_transition_fired {
        do_action(Action::PostTransitioned, created.id);
    }

    let url = repos.permalink(&created, &settings).await?;
    Ok((StatusCode::CREATED, Json(PostView::from(&created, url))))
}

/// A term as the API returns it.
#[derive(Debug, Serialize)]
pub struct TermView {
    pub id: i64,
    pub taxonomy: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub parent_id: Option<i64>,
    pub post_count: i64,
    pub url: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct TermQuery {
    #[serde(default)]
    pub taxonomy: Option<String>,
    #[serde(default)]
    pub page: Option<usize>,
    #[serde(default)]
    pub per_page: Option<usize>,
}

/// The pagination half of a query string, for endpoints that need nothing else.
#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub page: Option<usize>,
    #[serde(default)]
    pub per_page: Option<usize>,
}

/// `GET /api/v1/terms`.
#[get("/api/v1/terms")]
pub async fn list_terms(
    repos: Repos,
    Query(query): Query<TermQuery>,
) -> AutumnResult<Json<Vec<TermView>>> {
    let taxonomy = query.taxonomy.unwrap_or_else(|| "category".to_owned());
    if content_types::find_taxonomy(&taxonomy).is_none() {
        return Err(AutumnError::bad_request_msg(format!(
            "Unknown taxonomy `{taxonomy}`"
        )));
    }
    // Clamped and applied in SQL. The generated `find_by_taxonomy` is
    // unbounded, so an unauthenticated request against a large taxonomy
    // materialized and serialized every row of it.
    let per_page = i64::try_from(query.per_page.unwrap_or(50).clamp(1, 100)).unwrap_or(50);
    let offset = i64::try_from(clamped_page(query.page).saturating_sub(1))
        .unwrap_or(0)
        .saturating_mul(per_page);
    let mut conn = repos.conn().await?;
    let terms = crate::content::terms_page(&mut conn, &taxonomy, offset, per_page).await?;
    // Counted live rather than read off `terms.post_count`. The stored number
    // is computed from the *registry* — which types are public — so it is stale
    // the moment a deployment registers a type differently or restores content
    // whose plugin is disabled, and no row changes to repair it. The archive
    // this count describes is already visibility-aware, so publishing the
    // stored number meant the API and the archive disagreed.
    let term_ids: Vec<i64> = terms.iter().map(|term| term.id).collect();
    let counts = crate::content::term_post_counts(&mut conn, &term_ids).await?;
    Ok(Json(
        terms
            .iter()
            .map(|term| TermView {
                id: term.id,
                taxonomy: term.taxonomy.clone(),
                name: term.name.clone(),
                slug: term.slug.clone(),
                description: term.description.clone(),
                parent_id: term.parent_id,
                post_count: counts.get(&term.id).copied().unwrap_or(0),
                url: crate::theme::term_url(term),
            })
            .collect(),
    ))
}

/// A comment as the API returns it — approved only, and never with the
/// commenter's email address or IP.
#[derive(Debug, Serialize)]
pub struct CommentView {
    pub id: i64,
    pub post_id: i64,
    pub parent_id: Option<i64>,
    pub author: String,
    pub body: String,
    pub created_at: chrono::NaiveDateTime,
}

/// `GET /api/v1/posts/{id}/comments`.
#[get("/api/v1/posts/{id}/comments")]
pub async fn list_comments(
    repos: Repos,
    Path(id): Path<i64>,
    Query(query): Query<PageQuery>,
) -> AutumnResult<Json<Vec<CommentView>>> {
    // Only for content the public can see — otherwise the comment endpoint
    // would leak the existence of, and the discussion on, an unpublished post.
    let post = repos
        .posts
        .find_by_id(id)
        .await?
        .filter(|post| post.is_public() && is_publicly_routable(post))
        .ok_or_else(|| AutumnError::not_found_msg("No such post"))?;

    // A password-protected post withholds its whole thread on the front end
    // until the session unlocks it. Serving the same comments here without the
    // password would make the protection decorative — the discussion is part
    // of what the password gates.
    if post.is_password_protected() {
        return Err(AutumnError::not_found_msg("No such post"));
    }

    // Status filter, ordering and bound all in SQL. The generated
    // `find_by_post_id` returns every row of every status, so retaining in Rust
    // made this unauthenticated endpoint cost the post's whole moderation
    // queue — which, with guest comments enabled, anyone can grow.
    let per_page = i64::try_from(query.per_page.unwrap_or(50).clamp(1, 100)).unwrap_or(50);
    let offset = i64::try_from(clamped_page(query.page).saturating_sub(1))
        .unwrap_or(0)
        .saturating_mul(per_page);
    let mut conn = repos.conn().await?;
    let rows = crate::content::approved_comments_page(&mut conn, id, offset, per_page).await?;

    Ok(Json(
        rows.iter()
            .map(|comment| CommentView {
                id: comment.id,
                post_id: comment.post_id,
                parent_id: comment.parent_id,
                author: comment.display_name().to_owned(),
                body: comment.body.clone(),
                created_at: comment.created_at,
            })
            .collect(),
    ))
}

/// A public author profile.
#[derive(Debug, Serialize)]
pub struct AuthorView {
    pub id: i64,
    pub username: String,
    pub name: String,
    pub bio: String,
    pub website: String,
    pub url: String,
}

/// `GET /api/v1/authors` — accounts that have published something.
///
/// Deliberately **not** "every user": listing subscriber accounts would publish
/// the site's membership, and the email column would be one careless
/// serialization away from going with it.
#[get("/api/v1/authors")]
pub async fn list_authors(
    repos: Repos,
    Query(query): Query<PageQuery>,
) -> AutumnResult<Json<Vec<AuthorView>>> {
    // One `SELECT DISTINCT` join rather than loading every published post to
    // deduplicate its author id and then querying once per author: the cost of
    // listing bylines should scale with the number of authors, not the size of
    // the corpus.
    let mut conn = repos.conn().await?;
    let per_page = i64::try_from(query.per_page.unwrap_or(50).clamp(1, 100)).unwrap_or(50);
    let offset = i64::try_from(clamped_page(query.page).saturating_sub(1))
        .unwrap_or(0)
        .saturating_mul(per_page);
    let authors = crate::content::published_authors_page(&mut conn, offset, per_page).await?;

    Ok(Json(
        authors
            .iter()
            .map(|user| AuthorView {
                id: user.id,
                username: user.username.clone(),
                name: user.public_name().to_owned(),
                bio: user.bio.clone(),
                website: user.website.clone(),
                url: format!("/author/{}", user.username),
            })
            .collect(),
    ))
}

/// The site's public metadata — WordPress's `/wp-json` root document.
#[derive(Debug, Serialize)]
pub struct SiteInfo {
    pub name: String,
    pub description: String,
    pub post_types: Vec<String>,
    pub taxonomies: Vec<String>,
    pub permalink_structure: String,
}

/// `GET /api/v1` — what this site is and what it exposes.
#[get("/api/v1")]
pub async fn site_info(repos: Repos) -> AutumnResult<Json<SiteInfo>> {
    let settings = repos.settings().await?;
    Ok(Json(SiteInfo {
        name: settings.site_title.clone(),
        description: settings.tagline.clone(),
        post_types: content_types::all_post_types()
            .into_iter()
            .filter(|t| t.public)
            .map(|t| t.slug.to_owned())
            .collect(),
        taxonomies: content_types::all_taxonomies()
            .into_iter()
            .map(|t| t.slug.to_owned())
            .collect(),
        permalink_structure: settings.permalink_structure.as_str().to_owned(),
    }))
}
