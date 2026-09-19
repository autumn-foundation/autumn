//! Data-access repositories.
//!
//! `#[autumn_web::repository]` generates a `Pg…Repository` per model with the
//! usual CRUD surface, a `FromRequestParts` extractor so handlers can take one
//! as a parameter, and — where declared — the derived finders whose SQL is
//! inferred from the method signature.

use crate::hooks::{CommentHooks, PostHooks, TermHooks, UserHooks};
use crate::models::{
    Attachment, Comment, CommentDraftExt, Menu, MenuItem, NewAttachment, NewComment, NewMenu,
    NewMenuItem, NewPost, NewPostMeta, NewSiteOption, NewTerm, NewUser, NewWidget, Post,
    PostDraftExt, PostMeta, SiteOption, Term, TermDraftExt, UpdateAttachment, UpdateComment,
    UpdateMenu, UpdateMenuItem, UpdatePost, UpdatePostMeta, UpdateSiteOption, UpdateTerm,
    UpdateUser, UpdateWidget, User, UserDraftExt, Widget,
};
use crate::schema::{
    attachments, comments, menu_items, menus, options, post_meta, posts, terms, users, widgets,
};

/// Accounts.
///
/// `User` is a hand-written model rather than an `#[autumn_web::model]` (so
/// `password_hash` is never auto-serialized), but it supplies everything a
/// repository needs — `Queryable`/`Selectable` on the read struct, `Insertable`
/// on `NewUser`, `AsChangeset` on `UpdateUser` — so it still gets the generated
/// CRUD surface and, more importantly, the pool-backed extractor. That matters
/// on every rendered page: a repository holds the *pool* and acquires a
/// connection per call, while a `Db` extractor pins one for the whole request,
/// so resolving the signed-in user through this rather than through `Db` keeps
/// page renders down to one connection at a time.
#[autumn_web::repository(User, table = "users", hooks = UserHooks)]
pub trait UserRepository {
    fn find_by_username(username: String) -> Vec<User>;
    fn find_by_email(email: String) -> Vec<User>;
    fn find_by_role(role: String) -> Vec<User>;
}

/// Content: posts, pages and every custom type.
///
/// `searchable` is what turns the model's `#[searchable]` columns into a
/// `search()` method backed by the `search_vector` GIN index — the front end's
/// `?s=` query and the admin's content filter both run through it, so neither
/// falls back to an unindexable `LIKE '%…%'`.
#[autumn_web::repository(Post, hooks = PostHooks, searchable)]
pub trait PostRepository {
    fn find_by_slug(slug: String) -> Vec<Post>;
    fn find_by_status(status: String) -> Vec<Post>;
    fn find_by_author_id(author_id: i64) -> Vec<Post>;
    fn find_by_post_type(post_type: String) -> Vec<Post>;
    /// The query every public listing is built from — index-backed by
    /// `idx_posts_type_status`, so a blog index does not read draft or trashed
    /// rows only to discard them in Rust.
    fn find_by_post_type_and_status(post_type: String, status: String) -> Vec<Post>;
    fn find_by_author_id_and_status(author_id: i64, status: String) -> Vec<Post>;
    /// Dashboard counters — a `COUNT(*)`, not a `find_all().len()`.
    fn count_by_post_type_and_status(post_type: String, status: String) -> i64;
    fn count_by_status(status: String) -> i64;
}

/// Terms across every taxonomy.
#[autumn_web::repository(Term, hooks = TermHooks)]
pub trait TermRepository {
    fn find_by_taxonomy(taxonomy: String) -> Vec<Term>;
    fn find_by_slug(slug: String) -> Vec<Term>;
}

/// Comments, including the moderation queue.
#[autumn_web::repository(Comment, hooks = CommentHooks)]
pub trait CommentRepository {
    fn find_by_post_id(post_id: i64) -> Vec<Comment>;
    fn find_by_status(status: String) -> Vec<Comment>;
    /// The moderation queue's badge count.
    fn count_by_status(status: String) -> i64;
}

/// The media library.
#[autumn_web::repository(Attachment)]
pub trait AttachmentRepository {
    fn find_by_slug(slug: String) -> Vec<Attachment>;
}

/// Custom fields.
#[autumn_web::repository(PostMeta, table = "post_meta")]
pub trait PostMetaRepository {
    fn find_by_post_id(post_id: i64) -> Vec<PostMeta>;
}

/// The site settings store.
///
/// `invalidates(crate::settings::cached_settings)` discharges the build-time
/// cache-coherence obligation created by the memoized settings read: the
/// settings screen writes through this repository, and the declaration is what
/// lets `autumn cache audit` prove the memoized value is dropped when it does.
#[autumn_web::repository(
    SiteOption,
    table = "options",
    invalidates(crate::settings::cached_settings)
)]
pub trait SiteOptionRepository {
    fn find_by_name(name: String) -> Vec<SiteOption>;
}

/// Navigation menus.
#[autumn_web::repository(Menu)]
pub trait MenuRepository {
    fn find_by_slug(slug: String) -> Vec<Menu>;
    fn find_by_location(location: String) -> Vec<Menu>;
}

/// Menu entries.
#[autumn_web::repository(MenuItem)]
pub trait MenuItemRepository {
    fn find_by_menu_id(menu_id: i64) -> Vec<MenuItem>;
}

/// Sidebar widgets.
#[autumn_web::repository(Widget)]
pub trait WidgetRepository {
    fn find_by_sidebar(sidebar: String) -> Vec<Widget>;
}
