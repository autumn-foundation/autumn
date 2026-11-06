pub mod about;
pub mod auth;
pub mod avatars;
pub mod consent;
pub mod errors;
pub mod layout;
pub mod live;
pub mod partials;
pub mod posts;
pub mod subreddits;
pub mod votes;
pub mod webhooks;

/// The comment-page cursor in a page's query string (#3325).
///
/// The comment widget's "More comments" link adds `?after=<id>`. The page
/// passes it to `comment_thread_page`. A value that is not an id is a `400`.
#[derive(Debug, Default, serde::Deserialize)]
pub struct CommentPageQuery {
    /// The [`ThreadPage::next`](autumn_web::commentable::ThreadPage::next)
    /// cursor of the previous page. `None` for the first page.
    #[serde(default)]
    pub after: Option<i64>,
}

/// The number of top-level comments on one page.
pub const COMMENTS_PER_PAGE: usize = autumn_web::commentable::DEFAULT_THREAD_PAGE_SIZE;
