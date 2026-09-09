//! Mutation hooks — the invariants every write path shares.
//!
//! WordPress spreads this logic across `wp_insert_post`, a dozen `wp_*` filter
//! callbacks and whatever the active theme's `functions.php` adds, which is why
//! "why did my slug change?" is a genre of support question. Here every rule
//! that must hold for *any* write — the REST API, the admin editor, the
//! importer, a seed — lives in one hook implementation that the repository runs
//! inside the mutation's own transaction.
//!
//! Revisions are deliberately *not* written here: `after_create` / `after_update`
//! receive no connection, so a revision recorded from them would commit (or
//! fail) separately from the edit it describes. They are appended by
//! [`crate::content`], which owns the transaction.

use autumn_web::AutumnError;
use autumn_web::AutumnResult;
use autumn_web::hooks::{MutationContext, MutationHooks, UpdateDraft};
use autumn_web::slugify;

use crate::models::{
    Comment, NewComment, NewPost, NewTerm, NewUser, Post, Term, UpdateComment, UpdatePost,
    UpdateTerm, UpdateUser, User,
};

/// Normalize a submitted slug the one way every write path must.
///
/// An empty slug is derived from the title (the editor says so on the field);
/// a supplied one is still slugified, so a hand-typed "My Post!" can never
/// become a URL that needs escaping. A title of only punctuation slugifies to
/// nothing, which would store `""` and render a permalink of `/` — so it falls
/// back to a stable placeholder instead.
///
/// Public because the editor's update path writes through direct Diesel (to
/// keep the edit and its revision in one transaction) and therefore never runs
/// `PostHooks::before_update`. Both call this rather than each spelling the
/// rule out.
#[must_use]
pub fn normalize_slug(slug: &str, title: &str) -> String {
    let candidate = if slug.trim().is_empty() {
        slugify(title)
    } else {
        slugify(slug)
    };
    if candidate.is_empty() {
        "untitled".to_owned()
    } else {
        candidate
    }
}

/// Statuses a post may be *created* in. `future`, `private` and `trash` are
/// reachable only by transitioning from one of these, so the state machine sees
/// every move into them.
const CREATABLE_STATUSES: &[&str] = &["draft", "pending", "publish"];

/// The largest comment body accepted, matching the `#[validate]` cap declared
/// on the `Comment` model.
pub const MAX_COMMENT_BODY_BYTES: usize = 10_000;

/// The largest guest name accepted, matching the form's `maxlength`.
pub const MAX_COMMENT_NAME_BYTES: usize = 80;

/// The largest guest email accepted — the longest address RFC 5321 allows, and
/// the form's `maxlength`.
pub const MAX_COMMENT_EMAIL_BYTES: usize = 254;

/// The largest commenter website accepted.
///
/// There is no input for it on the form at all, which is exactly why it needs a
/// server-side cap: the only way to set it is a request that was not made by a
/// browser.
pub const MAX_COMMENT_URL_BYTES: usize = 200;

/// Comment moderation states.
pub const COMMENT_STATUSES: &[&str] = &["approved", "pending", "spam", "trash"];

/// Normalize and validate a comment draft.
///
/// Public because [`crate::content::create_comment`] inserts through direct
/// Diesel — it has to, so the row and the post's approved-comment counter move
/// in one transaction — and therefore never runs `CommentHooks::before_create`.
/// These are the *only* server-side checks a comment ever gets — on its body
/// and on its identity fields alike; the form's `required` and `maxlength`
/// attributes are a browser convenience a crafted request ignores, and a
/// signed-in commenter's empty body would otherwise be inserted pre-approved
/// and increment the counter.
pub fn validate_comment(new: &mut NewComment) -> AutumnResult<()> {
    new.body = new.body.trim().to_owned();
    if new.body.is_empty() {
        return Err(AutumnError::unprocessable_msg("Comment cannot be empty"));
    }
    // The model declares `#[validate(length(max = 10000))]`, but the direct
    // insert never runs the derived validator — and the form's `maxlength` is a
    // browser convenience a crafted request ignores. Without this, bodies grow
    // to the global request-body limit and make moderation and thread rendering
    // unexpectedly expensive.
    if new.body.len() > MAX_COMMENT_BODY_BYTES {
        return Err(AutumnError::unprocessable_msg(format!(
            "Comment must be at most {MAX_COMMENT_BODY_BYTES} characters"
        )));
    }
    if new.status.trim().is_empty() {
        new.status = "pending".to_owned();
    }
    if !COMMENT_STATUSES.contains(&new.status.as_str()) {
        return Err(AutumnError::bad_request_msg(format!(
            "Unknown comment status `{}`",
            new.status
        )));
    }
    // A comment with neither an account nor a name is unattributable. Guest
    // comments are legitimate — anonymous ones are not.
    new.author_name = new.author_name.trim().to_owned();
    if new.author_id.is_none() && new.author_name.is_empty() {
        return Err(AutumnError::unprocessable_msg("Name is required"));
    }

    // The identity fields need caps for the same reason the body does, and the
    // reason is sharper here: `/comments/{post_id}` is unauthenticated with the
    // shipped defaults, and the form's `maxlength` attributes are a browser
    // convenience a crafted POST ignores. Uncapped, a handful of accepted
    // comments carry request-sized names and emails, which the moderation
    // queue then loads and renders fifty at a time.
    //
    // Name and email are checked only for a guest, because those are the only
    // submissions where they come from the request: a signed-in commenter's
    // are copied from their account by the handler, and rejecting a comment
    // over the length of its author's own display name would enforce an
    // account rule at the wrong door. `author_url` is checked for everyone —
    // the handler passes it through from the form either way, and there is no
    // input for it on the form at all, so the only way to set it is a request
    // that never went through one.
    new.author_email = new.author_email.trim().to_owned();
    new.author_url = new.author_url.trim().to_owned();
    let mut capped: Vec<(&str, &str, usize)> =
        vec![("Website", &new.author_url, MAX_COMMENT_URL_BYTES)];
    if new.author_id.is_none() {
        capped.push(("Name", &new.author_name, MAX_COMMENT_NAME_BYTES));
        capped.push(("Email", &new.author_email, MAX_COMMENT_EMAIL_BYTES));
    }
    for (label, value, cap) in capped {
        if value.len() > cap {
            return Err(AutumnError::unprocessable_msg(format!(
                "{label} must be at most {cap} characters"
            )));
        }
    }
    Ok(())
}

/// Normalize and validate a new post.
///
/// Public for the same reason [`validate_comment`], [`normalize_new_user`] and
/// [`normalize_new_term`] are: the importer inserts through direct Diesel so
/// that the row and its `_import_source_slug` marker commit together, and
/// therefore never reaches `PostHooks::before_create`. An unmarked row is the
/// one shape a retry cannot recognise, so the marker has to be in the same
/// transaction as the insert — which means this validation has to be reachable
/// from outside the hook.
pub fn normalize_new_post(new: &mut NewPost) -> AutumnResult<()> {
    if new.post_type.trim().is_empty() {
        new.post_type = "post".to_owned();
    }
    if crate::content_types::find_post_type(&new.post_type).is_none() {
        return Err(AutumnError::bad_request_msg(format!(
            "Unknown post type `{}`",
            new.post_type
        )));
    }

    new.slug = normalize_slug(&new.slug, &new.title);

    if new.status.trim().is_empty() {
        new.status = "draft".to_owned();
    }
    if !CREATABLE_STATUSES.contains(&new.status.as_str()) {
        return Err(AutumnError::bad_request_msg(format!(
            "Content cannot be created directly in `{}`; create it as a draft and transition it",
            new.status
        )));
    }
    // The same guard the state machine puts on every publishing edge, so a
    // direct create cannot bypass what a transition would refuse.
    if new.status == "publish" && new.title.trim().is_empty() {
        return Err(AutumnError::unprocessable_msg(
            "A published post must have a title",
        ));
    }

    if !matches!(new.comment_status.as_str(), "open" | "closed") {
        new.comment_status = "open".to_owned();
    }

    // A post created *directly* as published needs its publish date stamped
    // here — `before_update` only ever sees a post that was already saved,
    // so without this a post that never passed through the editor's
    // draft→publish transition would carry no date at all: no byline date
    // on the page, no ordering on the index, and no `<lastmod>` in the
    // sitemap. A caller that supplied one (an importer preserving the
    // original date) keeps it.
    if new.published_at.is_none() && new.status == "publish" {
        new.published_at = Some(chrono::Utc::now().naive_utc());
    }
    // The model's declared cap, which a direct insert never runs — the same
    // check `validate_post_update` makes on the edit path.
    if new.title.chars().count() > MAX_POST_TITLE {
        return Err(AutumnError::unprocessable_msg(format!(
            "A title must be at most {MAX_POST_TITLE} characters"
        )));
    }

    Ok(())
}

#[derive(Clone, Default)]
pub struct PostHooks;

impl MutationHooks for PostHooks {
    type Model = Post;
    type NewModel = NewPost;
    type UpdateModel = UpdatePost;

    async fn before_create(
        &self,
        _ctx: &mut MutationContext,
        new: &mut NewPost,
    ) -> AutumnResult<()> {
        normalize_new_post(new)
    }

    async fn before_update(
        &self,
        _ctx: &mut MutationContext,
        draft: &mut UpdateDraft<Post>,
    ) -> AutumnResult<()> {
        validate_post_update(&draft.before, &mut draft.after)?;

        draft.after.updated_at = chrono::Utc::now().naive_utc();

        Ok(())
    }
}

/// The longest post title accepted, matching the `#[validate]` cap declared on
/// the `Post` model — and the editor's `maxlength`.
pub const MAX_POST_TITLE: usize = 300;

/// The invariants every edit to an existing post has to satisfy, whichever path
/// makes it.
///
/// Extracted from `PostHooks::before_update` because it is not the only writer.
/// `content::update_post_with_revision` applies the editor's changes to a
/// row it holds a lock on and writes the fields back with plain Diesel — which
/// is what makes the edit and its revision one transaction, and is also what
/// bypasses the hook. Without this, an Author editing an already-published post
/// could submit an empty title with `status=publish` unchanged: the
/// state-machine check below only fires on a status *change*, so nothing
/// rejected it and the row went live untitled, violating the heading-and-link
/// invariant the public templates rely on.
///
/// `after` is taken by `&mut` because two of these are normalisations rather
/// than refusals.
pub fn validate_post_update(before: &Post, after: &mut Post) -> AutumnResult<()> {
    // Normalise a slug the caller changed; regenerate one they cleared.
    if after.slug.trim().is_empty() || after.slug != before.slug {
        after.slug = normalize_slug(&after.slug, &after.title);
    }

    // Enforce the state machine on EVERY update path, not just the dedicated
    // transition route: the REST API and the importer can both set `status`
    // directly, and without this an API client could move a post from `trash`
    // straight to `private` — an edge the graph does not have. Build the
    // proposed row (new content, OLD status) so the `can_publish` guard
    // evaluates the content actually being saved.
    if after.status != before.status {
        let mut proposed = after.clone();
        proposed.status.clone_from(&before.status);
        proposed.transition_status_to(&after.status)?;
    }

    // A post that is live must keep a title, whether or not this edit is the
    // one that changed the status.
    if matches!(after.status.as_str(), "publish" | "private" | "future") && !after.can_publish() {
        return Err(AutumnError::unprocessable_msg(
            "A published post must have a title",
        ));
    }

    // And the model's declared cap, which the direct-Diesel path never runs.
    // Checked for every status, not just the live ones: the form's `maxlength`
    // is a browser convenience a crafted edit ignores, so an author could park
    // a request-sized title on a draft and publish it afterwards — at which
    // point every listing, feed and API response carries it. The lower bound is
    // deliberately not enforced here: an untitled *draft* is legitimate, which
    // is what the live-status check above is for.
    if after.title.chars().count() > MAX_POST_TITLE {
        return Err(AutumnError::unprocessable_msg(format!(
            "A title must be at most {MAX_POST_TITLE} characters"
        )));
    }

    // `published_at` is the timestamp the front end orders and dates by. Stamp
    // it the first time a post goes live and never overwrite it afterwards — an
    // edit to a two-year-old post must not reorder the blog index. A scheduled
    // post carries the author's chosen future date, so an existing value always
    // wins.
    if after.published_at.is_none() && matches!(after.status.as_str(), "publish" | "private") {
        after.published_at = Some(chrono::Utc::now().naive_utc());
    }

    Ok(())
}

/// Normalize and validate a new term.
///
/// Public for the same reason [`validate_comment`] and [`normalize_new_user`]
/// are: [`crate::content::import_terms`] restores a file's whole taxonomy —
/// the rows and their ancestry — in one transaction, which it can only do
/// through direct Diesel, and therefore never reaches
/// `TermHooks::before_create`.
pub fn normalize_new_term(new: &mut NewTerm) -> AutumnResult<()> {
    if new.taxonomy.trim().is_empty() {
        new.taxonomy = "category".to_owned();
    }
    let Some(taxonomy) = crate::content_types::find_taxonomy(&new.taxonomy) else {
        return Err(AutumnError::bad_request_msg(format!(
            "Unknown taxonomy `{}`",
            new.taxonomy
        )));
    };
    // A flat taxonomy has no hierarchy to put a term in. Silently dropping
    // the parent is friendlier than a 400 here: the field simply does not
    // exist for tags, so a client that sends one is confused, not hostile.
    if !taxonomy.hierarchical {
        new.parent_id = None;
    }

    new.slug = if new.slug.trim().is_empty() {
        slugify(&new.name)
    } else {
        slugify(&new.slug)
    };
    if new.slug.is_empty() {
        return Err(AutumnError::unprocessable_msg(
            "Term name must contain at least one alphanumeric character",
        ));
    }
    Ok(())
}

#[derive(Clone, Default)]
pub struct TermHooks;

impl MutationHooks for TermHooks {
    type Model = Term;
    type NewModel = NewTerm;
    type UpdateModel = UpdateTerm;

    async fn before_create(
        &self,
        _ctx: &mut MutationContext,
        new: &mut NewTerm,
    ) -> AutumnResult<()> {
        normalize_new_term(new)
    }

    async fn before_update(
        &self,
        _ctx: &mut MutationContext,
        draft: &mut UpdateDraft<Term>,
    ) -> AutumnResult<()> {
        if draft.after.slug.trim().is_empty() {
            draft.after.slug = slugify(&draft.after.name);
        } else if draft.after.slug != draft.before.slug {
            draft.after.slug = slugify(&draft.after.slug);
        }
        if draft.after.slug.is_empty() {
            return Err(AutumnError::unprocessable_msg(
                "Term name must contain at least one alphanumeric character",
            ));
        }
        // A term cannot be its own parent, and the taxonomy cannot change under
        // a term that posts are already filed against.
        if draft.after.parent_id == Some(draft.before.id) {
            return Err(AutumnError::unprocessable_msg(
                "A term cannot be its own parent",
            ));
        }
        if draft.after.taxonomy != draft.before.taxonomy {
            return Err(AutumnError::unprocessable_msg(
                "A term's taxonomy cannot be changed; delete it and create it in the new taxonomy",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct CommentHooks;

impl MutationHooks for CommentHooks {
    type Model = Comment;
    type NewModel = NewComment;
    type UpdateModel = UpdateComment;

    async fn before_create(
        &self,
        _ctx: &mut MutationContext,
        new: &mut NewComment,
    ) -> AutumnResult<()> {
        validate_comment(new)
    }

    async fn before_update(
        &self,
        _ctx: &mut MutationContext,
        draft: &mut UpdateDraft<Comment>,
    ) -> AutumnResult<()> {
        if !COMMENT_STATUSES.contains(&draft.after.status.as_str()) {
            return Err(AutumnError::bad_request_msg(format!(
                "Unknown comment status `{}`",
                draft.after.status
            )));
        }
        Ok(())
    }
}

/// Normalize and validate a new account.
///
/// Public for the same reason [`validate_comment`] is: account creation runs
/// inside a transaction that also elects the first administrator (see
/// [`crate::content::register_user`]), so it inserts through direct Diesel and
/// never reaches `UserHooks::before_create`.
pub fn normalize_new_user(new: &mut NewUser) -> AutumnResult<()> {
    new.username = new.username.trim().to_lowercase();
    new.email = new.email.trim().to_lowercase();
    if new.username.is_empty() {
        return Err(AutumnError::unprocessable_msg("Username is required"));
    }
    // The username *is* the author archive's URL segment — bylines link to
    // `/author/{username}` and the resolver matches an author archive only on a
    // two-segment path. So `alice/news` produced an account that could log in
    // and publish normally while every byline and API author URL it generated
    // pointed at a 404.
    //
    // Refusing rather than silently rewriting: the username is what the person
    // types to log in, and quietly storing something else is worse than saying
    // no. `slugify` supplies the suggestion.
    if new.username != autumn_web::slugify(&new.username) {
        return Err(AutumnError::unprocessable_msg(format!(
            "A username can only contain lowercase letters, numbers and hyphens \
             (try `{}`)",
            autumn_web::slugify(&new.username)
        )));
    }
    // An unrecognised role degrades to the least-privileged one rather than
    // being stored verbatim, so `users.role` can only ever hold a value
    // `Role::parse` round-trips.
    new.role = crate::capabilities::Role::parse(&new.role)
        .slug()
        .to_owned();
    Ok(())
}

/// Keeps `users.updated_at` honest.
///
/// `updated_at` is a `#[default]` column — the database supplies it on insert —
/// which means it is absent from the generated `UpdateUser` and nothing would
/// ever advance it. Postgres has no `ON UPDATE` trigger, so a hook is where
/// that belongs: every write path gets it, including the REST API and any
/// future importer, rather than each call site remembering.
#[derive(Clone, Default)]
pub struct UserHooks;

impl MutationHooks for UserHooks {
    type Model = User;
    type NewModel = NewUser;
    type UpdateModel = UpdateUser;

    async fn before_create(
        &self,
        _ctx: &mut MutationContext,
        new: &mut NewUser,
    ) -> AutumnResult<()> {
        // Delegated, not duplicated. This hook carried its own copy of the
        // normalisation, so the routable-username rule added to
        // `normalize_new_user` reached the registration path and not the
        // admin's "add user" screen — an administrator could still create
        // `alice/news`, whose bylines all 404. One definition is what stops the
        // two drifting again.
        normalize_new_user(new)
    }

    async fn before_update(
        &self,
        _ctx: &mut MutationContext,
        draft: &mut UpdateDraft<User>,
    ) -> AutumnResult<()> {
        draft.after.email = draft.after.email.trim().to_lowercase();
        draft.after.role = crate::capabilities::Role::parse(&draft.after.role)
            .slug()
            .to_owned();
        draft.after.updated_at = chrono::Utc::now().naive_utc();
        Ok(())
    }
}
