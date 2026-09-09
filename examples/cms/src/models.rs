//! The content model.
//!
//! Every WordPress-core concept has a struct here, and the mapping is
//! deliberately one-to-one so a reader who knows WordPress can find their
//! bearings: `Post` covers posts, pages and every custom post type; `Term`
//! covers categories, tags and custom taxonomies; `SiteOption` is the options
//! table; `Attachment` is the media library.

use autumn_web::AutumnResult;
use autumn_web::storage::Blob;
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl;

// Every table module a `#[model]` in this file maps. `post_terms` is
// deliberately absent: `#[has_many(…, through = post_terms)]` declares its own
// hidden `post_terms` table inside a private module that does `use super::*`,
// so importing the `crate::schema` one here would put two `table!` expansions
// for the same name in that module's scope and collide their trait impls.
// Modules that need the join table for a read (`repositories.rs`) import it
// from `crate::schema` directly.
use crate::schema::{
    attachments, comments, menu_items, menus, options, post_meta, posts, revisions, terms, users,
    widgets,
};

// ── Users ───────────────────────────────────────────────────────────────────

/// An account.
///
/// `#[serde(skip)]` on `password_hash` is load-bearing: `#[model]` derives
/// `Serialize` across every mapped column, and this model is what the REST
/// API's user endpoints return. `examples/reddit-clone` hand-writes its `User`
/// to keep the hash out of JSON; that is the safer default in general, but a
/// hand-written struct cannot back an `#[autumn_web::repository]` (the
/// repository codegen needs `#[model]`'s internals), and the Users admin screen
/// needs the full generated CRUD surface. The attribute is carried through to
/// the derive, and `password_hash_is_never_serialized` below is the regression
/// test that keeps it that way.
#[autumn_web::model(table = "users")]
pub struct User {
    #[id]
    pub id: i64,
    #[indexed]
    #[validate(length(min = 1, max = 60))]
    pub username: String,
    #[indexed]
    #[validate(email)]
    pub email: String,
    #[serde(skip)]
    pub password_hash: String,
    pub display_name: String,
    /// One of the five WordPress core roles — see [`crate::capabilities::Role`].
    pub role: String,
    pub bio: String,
    pub website: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
    #[default]
    pub updated_at: chrono::NaiveDateTime,
}

// `#[model]` already implements `Preloadable` for `User`, so the explicit
// `impl_preloadable_leaf!` a hand-written model would need is not just
// unnecessary here — it is a conflicting impl.

impl User {
    /// The name to show in a byline: the display name when set, else the
    /// username. WordPress calls this the "public display name".
    #[must_use]
    pub fn public_name(&self) -> &str {
        if self.display_name.trim().is_empty() {
            &self.username
        } else {
            &self.display_name
        }
    }

    /// The account's parsed role. An unrecognised value in the column degrades
    /// to the least-privileged role rather than panicking, so a hand-edited or
    /// imported row can never accidentally grant more than it names.
    #[must_use]
    pub fn role(&self) -> crate::capabilities::Role {
        crate::capabilities::Role::parse(&self.role)
    }
}

// ── Options (site settings) ─────────────────────────────────────────────────

/// One row of the site-wide settings store.
///
/// Named `SiteOption` rather than `Option` for the obvious reason — a model
/// called `Option` would shadow `std::option::Option` in every module that
/// imports it — with `table = "options"` keeping the WordPress table name.
#[autumn_web::model(table = "options")]
pub struct SiteOption {
    #[id]
    pub id: i64,
    #[indexed]
    pub name: String,
    pub value: String,
    pub autoload: bool,
    #[default]
    pub updated_at: chrono::NaiveDateTime,
}

// ── Media library ───────────────────────────────────────────────────────────

/// An uploaded file.
///
/// `file` is a [`Blob`] handle: the bytes live in the configured `BlobStore`
/// (local disk in development, S3 in production) and only the handle is stored
/// here, so the database never carries megabytes of image.
///
/// It is `Option<Blob>` rather than `Blob` because `#[model]` requires every
/// mapped field type to implement `Default` and a blob handle has no
/// meaningful empty value. Every write path sets it; [`Attachment::blob`] is
/// the accessor that turns the always-`Some` invariant into an explicit error
/// rather than an `unwrap`.
#[autumn_web::model]
pub struct Attachment {
    #[id]
    pub id: i64,
    #[validate(length(min = 1, max = 300))]
    pub title: String,
    #[indexed]
    pub slug: String,
    pub file: Option<Blob>,
    pub mime_type: String,
    pub byte_size: i64,
    pub width: Option<i32>,
    pub height: Option<i32>,
    /// The accessible name for an `<img>`. Empty is legitimate and means
    /// "decorative" — the admin UI says so rather than nagging.
    pub alt_text: String,
    pub caption: String,
    pub uploader_id: Option<i64>,
    #[default]
    pub created_at: chrono::NaiveDateTime,
    #[default]
    pub updated_at: chrono::NaiveDateTime,
}

impl Attachment {
    /// The stored blob handle.
    ///
    /// The column is nullable only because `#[model]` needs `Default` on every
    /// mapped field (see the type note above); every write path sets it, so a
    /// `None` here means the row was written around the application and is a
    /// 500, not a state any screen has to render.
    pub fn blob(&self) -> AutumnResult<&Blob> {
        self.file.as_ref().ok_or_else(|| {
            autumn_web::AutumnError::internal_server_error_msg(format!(
                "attachment {} has no stored file",
                self.id
            ))
        })
    }

    /// Whether this attachment can be rendered inline as an image.
    #[must_use]
    pub fn is_image(&self) -> bool {
        self.mime_type.starts_with("image/")
    }
}

// ── Content ─────────────────────────────────────────────────────────────────

/// A piece of content: a post, a page, or any registered custom post type.
///
/// The `status` field is a real [`state_machine`] rather than a free string, so
/// the WordPress lifecycle is enforced by the type system instead of by
/// convention. The graph is WordPress's own, with one deliberate difference:
/// there is no edge *out of* `trash` other than back to `draft` or `publish`
/// (WordPress restores to the previous status, which it stores in a meta key —
/// a trail that is easy to lose; restoring to draft is the safe default and the
/// editor can publish from there in one more click).
#[autumn_web::model]
#[belongs_to(User, fk = author_id)]
#[has_many(Term, through = post_terms)]
#[searchable(language = "english")]
pub struct Post {
    #[id]
    pub id: i64,
    /// `post`, `page`, or a custom type registered in
    /// [`crate::content_types`]. Kept as a string, like WordPress, so a new
    /// type is a registry entry rather than a migration.
    #[indexed]
    pub post_type: String,
    #[searchable(weight = "A")]
    #[validate(length(min = 1, max = 300))]
    pub title: String,
    #[indexed]
    pub slug: String,
    #[searchable(weight = "B")]
    pub excerpt: String,
    #[searchable(weight = "C")]
    pub body: String,
    #[state_machine(transitions(
        draft -> pending,
        draft -> publish: guard = "can_publish",
        draft -> future: guard = "can_publish",
        draft -> private: guard = "can_publish",
        draft -> trash,
        pending -> draft,
        pending -> publish: guard = "can_publish",
        pending -> future: guard = "can_publish",
        pending -> trash,
        publish -> draft,
        publish -> private,
        publish -> trash,
        private -> publish,
        private -> draft,
        private -> trash,
        future -> publish: guard = "can_publish",
        future -> draft,
        future -> trash,
        trash -> draft,
        trash -> publish: guard = "can_publish",
    ))]
    pub status: String,
    #[indexed]
    pub author_id: i64,
    /// Hierarchical content (pages, and any custom type registered as
    /// hierarchical). `None` is a top-level item.
    pub parent_id: Option<i64>,
    /// WordPress's "featured image".
    pub featured_media_id: Option<i64>,
    /// Manual ordering for hierarchical types; ignored by date-ordered ones.
    pub menu_order: i32,
    /// `open` or `closed` — whether new comments are accepted.
    pub comment_status: String,
    /// Non-empty makes the post password-protected: the body is withheld from
    /// the front end until the visitor supplies this value.
    pub password: String,
    /// Pinned to the top of the blog index.
    pub sticky: bool,
    /// Approved comments only — see the note on [`Comment`].
    #[default]
    pub comment_count: i64,
    /// When the post went live, or (for `future`) when it is due to. `None`
    /// until the first publish.
    pub published_at: Option<chrono::NaiveDateTime>,
    /// Optimistic locking, so two editors on the same post cannot silently
    /// overwrite each other — the second save is rejected rather than lost.
    #[lock_version]
    pub lock_version: i32,
    #[default]
    pub created_at: chrono::NaiveDateTime,
    #[default]
    pub updated_at: chrono::NaiveDateTime,
}

impl Post {
    /// Guard on every edge that makes content publicly reachable. WordPress
    /// happily publishes an untitled empty post; refusing is the small
    /// improvement, and it is the one invariant the front end relies on to
    /// render a heading and a link for every published row.
    #[must_use]
    pub fn can_publish(&self) -> bool {
        !self.title.trim().is_empty()
    }

    /// Whether the post is visible to an anonymous visitor.
    #[must_use]
    pub fn is_public(&self) -> bool {
        self.status == "publish"
    }

    /// Whether a password must be supplied before the body is shown.
    #[must_use]
    pub fn is_password_protected(&self) -> bool {
        !self.password.is_empty()
    }

    /// The excerpt to show in a listing: the authored one when present, else
    /// the first 55 words of the body — WordPress's own default length.
    ///
    /// A password-protected post never derives one. An authored excerpt is
    /// still shown, because the author wrote it knowing the listing is public;
    /// deriving from the body is a different thing entirely, and doing it here
    /// would hand out the first 55 words of the very content the password
    /// withholds — through the blog index, the REST API and the syndication
    /// feeds at once, since all three call this.
    #[must_use]
    pub fn display_excerpt(&self) -> String {
        if !self.excerpt.trim().is_empty() {
            return self.excerpt.clone();
        }
        if self.is_password_protected() {
            return String::new();
        }
        let text = plain_text(&self.body);
        let mut words = text.split_whitespace();
        let head: Vec<&str> = words.by_ref().take(55).collect();
        let mut out = head.join(" ");
        if words.next().is_some() {
            out.push('…');
        }
        out
    }

    /// Append a revision snapshot of this row as it currently stands.
    ///
    /// Takes the connection so callers can run it inside the same transaction
    /// as the write it is recording — a revision that commits without its edit
    /// (or vice versa) is worse than no revision at all.
    pub async fn record_revision(
        &self,
        conn: &mut AsyncPgConnection,
        summary: &str,
    ) -> AutumnResult<()> {
        self.record_revision_by(conn, summary, self.author_id).await
    }

    /// Append a revision snapshot, attributed to the account that made the
    /// change.
    ///
    /// `revisions.author_id` answers "who edited", not "who wrote the post" —
    /// those differ on every collaborative edit, and recording the owner for an
    /// Editor's change makes the history confidently wrong. [`record_revision`]
    /// keeps the owner for the callers where the two are the same by
    /// construction (an initial snapshot, a status transition made by a path
    /// with no acting user to hand).
    pub async fn record_revision_by(
        &self,
        conn: &mut AsyncPgConnection,
        summary: &str,
        editor_id: i64,
    ) -> AutumnResult<()> {
        diesel::insert_into(revisions::table)
            .values(&NewRevision {
                post_id: self.id,
                title: self.title.clone(),
                excerpt: self.excerpt.clone(),
                body: self.body.clone(),
                status: self.status.clone(),
                author_id: Some(editor_id),
                summary: summary.to_owned(),
            })
            .execute(conn)
            .await?;
        Ok(())
    }
}

// ── Post meta (custom fields) ───────────────────────────────────────────────

/// One custom field on a post. WordPress's `wp_postmeta`, minus the serialized
/// PHP: values are stored as text and interpreted by whoever wrote them.
#[autumn_web::model(table = "post_meta")]
#[belongs_to(Post)]
pub struct PostMeta {
    #[id]
    pub id: i64,
    #[indexed]
    pub post_id: i64,
    pub meta_key: String,
    pub meta_value: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

// ── Taxonomies ──────────────────────────────────────────────────────────────

/// A term in a taxonomy: a category, a tag, or a term of any custom taxonomy
/// registered in [`crate::content_types`].
#[autumn_web::model]
#[has_many(Post, through = post_terms)]
pub struct Term {
    #[id]
    pub id: i64,
    /// `category`, `post_tag`, or a registered custom taxonomy.
    #[indexed]
    pub taxonomy: String,
    #[validate(length(min = 1, max = 200))]
    pub name: String,
    #[indexed]
    pub slug: String,
    pub description: String,
    /// Hierarchical taxonomies only (categories have parents; tags do not).
    pub parent_id: Option<i64>,
    /// Published posts carrying this term. Maintained by the term-assignment
    /// paths rather than a `counter_cache`, because it counts only *published*
    /// children and counter caches are documented as flat, unconditional counts.
    #[default]
    pub post_count: i64,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

/// Remove `[shortcode ...]` tags, leaving surrounding prose intact.
///
/// A `[` with no closing `]` on the same line is ordinary text (a footnote
/// marker, a bracketed aside) and is left alone.
fn strip_shortcodes(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(open) = rest.find('[') {
        let (before, after) = rest.split_at(open);
        out.push_str(before);
        let body = &after[1..];
        // A tag never spans a line break.
        let line_end = body.find('\n').unwrap_or(body.len());
        match body[..line_end].find(']') {
            Some(close) => rest = &body[close + 1..],
            None => {
                out.push('[');
                rest = body;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Strip enough Markdown syntax to build a readable plain-text excerpt.
///
/// Deliberately small: it is used only to derive a preview when the author
/// wrote none, so it trades completeness for never mangling ordinary prose.
/// Anything that reaches a page goes through
/// `autumn_web::markdown::render_user_content`, which sanitizes properly.
#[must_use]
pub fn plain_text(markdown: &str) -> String {
    // Drop shortcode tags whole rather than stripping their brackets: leaving
    // `note text="hi"` behind turns an auto-excerpt into markup soup, which is
    // exactly what an excerpt is supposed to avoid. Their *output* is not
    // available here either — expansion happens at render time, against a
    // registry this pure function has no business consulting.
    let markdown = &strip_shortcodes(markdown);
    let mut out = String::with_capacity(markdown.len());
    for line in markdown.lines() {
        let line = line.trim_start();
        // Drop fenced-code delimiters, ATX heading markers, blockquote and
        // list bullets — the characters that would otherwise read as noise.
        if line.starts_with("```") {
            continue;
        }
        let line = line
            .trim_start_matches('#')
            .trim_start_matches('>')
            .trim_start_matches(['-', '*', '+'])
            .trim_start();
        for ch in line.chars() {
            match ch {
                '*' | '_' | '`' | '[' | ']' | '#' => {}
                other => out.push(other),
            }
        }
        out.push(' ');
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ── Revisions ───────────────────────────────────────────────────────────────

/// An immutable snapshot of a post's editable fields.
///
/// Write-only from the mutation hooks, read-only from the admin history screen —
/// so it is plain Diesel rather than a model with a repository.
#[derive(Debug, Clone, diesel::Queryable, diesel::Selectable, serde::Serialize)]
#[diesel(table_name = revisions)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Revision {
    pub id: i64,
    pub post_id: i64,
    pub title: String,
    pub excerpt: String,
    pub body: String,
    pub status: String,
    pub author_id: Option<i64>,
    pub summary: String,
    pub created_at: chrono::NaiveDateTime,
}

#[derive(Debug, Clone, diesel::Insertable)]
#[diesel(table_name = revisions)]
pub struct NewRevision {
    pub post_id: i64,
    pub title: String,
    pub excerpt: String,
    pub body: String,
    pub status: String,
    pub author_id: Option<i64>,
    pub summary: String,
}

// ── Comments ────────────────────────────────────────────────────────────────

/// A comment, with WordPress's moderation state.
///
/// `author_id` is nullable on purpose: WordPress core accepts comments from
/// visitors who have no account, identified by the name/email/url triple. That
/// requirement, plus the `status` moderation queue, is why this is a
/// hand-rolled table rather than the framework's polymorphic `#[commentable]`
/// association — that one keys every comment to a registered `User` and models
/// no moderation state.
///
/// `posts.comment_count` counts **approved** comments only, matching
/// WordPress, so it is maintained explicitly by the moderation transitions in
/// [`crate::repositories`] rather than by `counter_cache` — which is documented
/// as a flat, unconditional count of all children.
#[autumn_web::model]
#[belongs_to(Post)]
pub struct Comment {
    #[id]
    pub id: i64,
    #[indexed]
    pub post_id: i64,
    /// Threading. `None` is a top-level comment.
    pub parent_id: Option<i64>,
    /// The commenter's account, when they had one.
    pub author_id: Option<i64>,
    pub author_name: String,
    pub author_email: String,
    pub author_url: String,
    /// Retained for spam heuristics and abuse reports, never rendered.
    pub author_ip: String,
    #[validate(length(min = 1, max = 10000))]
    pub body: String,
    /// `approved` | `pending` | `spam` | `trash`.
    pub status: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

impl Comment {
    /// The name to render above the comment.
    #[must_use]
    pub fn display_name(&self) -> &str {
        if self.author_name.trim().is_empty() {
            "Anonymous"
        } else {
            &self.author_name
        }
    }
}

// ── Navigation menus ────────────────────────────────────────────────────────

/// A named navigation menu, assigned to a theme location.
#[autumn_web::model]
pub struct Menu {
    #[id]
    pub id: i64,
    #[validate(length(min = 1, max = 200))]
    pub name: String,
    #[indexed]
    pub slug: String,
    /// The theme location this menu fills (`primary`, `footer`, …), or empty
    /// for an unassigned menu.
    pub location: String,
    #[default]
    pub created_at: chrono::NaiveDateTime,
}

/// One entry in a navigation menu.
///
/// The target is whichever of `post_id`, `term_id`, `url` is set, resolved in
/// that order — the same three link kinds the WordPress menu editor offers.
#[autumn_web::model]
#[belongs_to(Menu)]
pub struct MenuItem {
    #[id]
    pub id: i64,
    #[indexed]
    pub menu_id: i64,
    pub parent_id: Option<i64>,
    #[validate(length(min = 1, max = 200))]
    pub label: String,
    pub url: String,
    pub post_id: Option<i64>,
    pub term_id: Option<i64>,
    pub position: i32,
}

// ── Widgets ─────────────────────────────────────────────────────────────────

/// An instance of a registered widget kind, placed in a named sidebar.
#[autumn_web::model]
pub struct Widget {
    #[id]
    pub id: i64,
    #[indexed]
    pub sidebar: String,
    /// Which registered widget renders this instance — see
    /// [`crate::theme::widgets`].
    pub kind: String,
    pub title: String,
    /// Kind-specific configuration (how many posts to list, the text to show).
    pub settings: serde_json::Value,
    pub position: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_user() -> User {
        let now = chrono::Utc::now().naive_utc();
        User {
            id: 1,
            username: "jane".to_owned(),
            email: "jane@example.com".to_owned(),
            password_hash: "$2b$12$SUPER-SECRET-HASH".to_owned(),
            display_name: "Jane".to_owned(),
            role: "editor".to_owned(),
            bio: String::new(),
            website: String::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// `#[model]` derives `Serialize` over every mapped column, and `User` is
    /// returned by the REST API. Without the `#[serde(skip)]` on the field this
    /// asserts, every bcrypt hash on the site would be one `GET /api/v1/users`
    /// away. This is the regression test for that attribute.
    #[test]
    fn password_hash_is_never_serialized() {
        let json = serde_json::to_string(&sample_user()).expect("user serializes");
        assert!(
            !json.contains("password_hash"),
            "the hash's field name leaked into JSON: {json}"
        );
        assert!(
            !json.contains("SUPER-SECRET-HASH"),
            "the hash itself leaked into JSON: {json}"
        );
        // …and the rest of the record is still there, so the test would fail if
        // serialization broke entirely rather than the field being skipped.
        assert!(json.contains("jane@example.com"), "json: {json}");
    }

    #[test]
    fn public_name_falls_back_to_the_username() {
        let mut user = sample_user();
        assert_eq!(user.public_name(), "Jane");
        user.display_name = "   ".to_owned();
        assert_eq!(user.public_name(), "jane");
    }

    #[test]
    fn excerpt_falls_back_to_the_first_55_words_of_the_body() {
        let mut post = crate::permalinks::tests_support::sample_post();
        post.body = (1..=80)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let excerpt = post.display_excerpt();
        assert!(excerpt.starts_with("1 2 3"));
        assert!(
            excerpt.ends_with('…'),
            "a truncated excerpt is elided: {excerpt}"
        );
        assert_eq!(excerpt.trim_end_matches('…').split_whitespace().count(), 55);

        // An authored excerpt always wins.
        post.excerpt = "Hand written.".to_owned();
        assert_eq!(post.display_excerpt(), "Hand written.");
    }

    #[test]
    fn excerpt_strips_markdown_syntax() {
        let mut post = crate::permalinks::tests_support::sample_post();
        post.body = "## A heading\n\n- **bold** item\n- `code` item".to_owned();
        assert_eq!(post.display_excerpt(), "A heading bold item code item");
    }

    #[test]
    fn short_bodies_are_not_elided() {
        let mut post = crate::permalinks::tests_support::sample_post();
        post.body = "Just a few words.".to_owned();
        assert_eq!(post.display_excerpt(), "Just a few words.");
    }
}

#[cfg(test)]
mod excerpt_tests {
    use super::*;

    #[test]
    fn shortcodes_are_removed_from_auto_excerpts_entirely() {
        // Leaving the brackets' contents behind (`note text="hi"`) is what an
        // excerpt is supposed to prevent.
        let mut post = crate::permalinks::tests_support::sample_post();
        post.body = r#"A CMS in **Rust**, with [note text="shortcodes work"] and a year: [year]."#
            .to_owned();
        assert_eq!(post.display_excerpt(), "A CMS in Rust, with and a year: .");
    }

    #[test]
    fn an_unclosed_bracket_is_left_alone_by_the_shortcode_stripper() {
        // A `[` with no `]` on the same line is prose, not a tag.
        assert_eq!(strip_shortcodes("see [1] and [2"), "see  and [2");
        assert_eq!(strip_shortcodes("a [tag] b"), "a  b");
        // A tag never spans a newline.
        assert_eq!(strip_shortcodes("a [one\ntwo] b"), "a [one\ntwo] b");
    }

    #[test]
    fn plain_text_also_drops_markdown_link_brackets() {
        // `plain_text` runs the shortcode stripper first and *then* removes
        // Markdown syntax characters, so a surviving bracket goes too. Both
        // stages are deliberate: this is the excerpt path, not a renderer.
        assert_eq!(plain_text("see [1] and [2"), "see and 2");
    }
}
