//! The content service — the transactional operations the routes are built from.
//!
//! Everything here is an operation that must be atomic across more than one
//! table: publishing a post *and* recording its revision, moving a comment
//! through moderation *and* adjusting the approved-comment counter, replacing a
//! post's terms *and* the affected terms' post counts. Handlers call these; they
//! never open a transaction themselves.

use autumn_web::AutumnError;
use autumn_web::AutumnResult;
use autumn_web::db::Db;
use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use scoped_futures::ScopedFutureExt;

use crate::models::{Comment, NewRevision, Post, Revision, Term, User};
use crate::schema::{comments, menus, post_terms, posts, revisions, terms, users};

/// The maximum reply nesting a comment thread accepts.
///
/// WordPress's own default (`thread_comments_depth`) is 5. It is enforced on
/// the **write** path, so the renderer never has to defend itself against a
/// chain deep enough to blow the stack.
pub const MAX_COMMENT_DEPTH: usize = 5;

/// How many revisions to keep per post. WordPress keeps every revision by
/// default and grows `wp_posts` without bound; capping is the small improvement,
/// and the cap is generous enough that no editor will hit it in a session.
pub const REVISION_LIMIT: i64 = 25;

// ── Content edits ───────────────────────────────────────────────────────────

/// Apply an edit to a post and record the revision that describes it, in one
/// transaction.
///
/// The revision snapshots the row **as it was before** the edit, which is what
/// makes "restore this revision" mean something: the newest revision is the
/// state you would return to by undoing the current content.
/// `expected_lock_version` is the version the editor's form was rendered from.
/// When it does not match the row read under the lock, someone else saved in
/// between and this submission is built on content that no longer exists — the
/// write is refused with `409 Conflict` rather than overwriting their work.
/// `None` skips the check, for callers with no form behind them.
pub async fn update_post_with_revision(
    db: &mut Db,
    post_id: i64,
    editor_id: i64,
    summary: &str,
    expected_lock_version: Option<i32>,
    record_revision: bool,
    apply: impl for<'a> FnOnce(&'a mut Post) + Send + 'static,
) -> AutumnResult<Post> {
    let summary = summary.to_owned();
    db.tx(move |conn| {
        async move {
            // Lock the row for the duration: two editors saving the same post
            // must serialise, or the second silently overwrites the first and
            // the revision trail records an edit that never happened.
            let mut post: Post = posts::table
                .find(post_id)
                .select(Post::as_select())
                .for_update()
                .first(conn)
                .await
                .map_err(AutumnError::not_found)?;

            // Stale-edit detection. The row lock above serializes concurrent
            // saves but does not make the second one *correct*: without this,
            // the later request applies its whole stale form snapshot over the
            // row the first editor just wrote, silently losing their changes.
            if let Some(expected) = expected_lock_version
                && expected != post.lock_version
            {
                return Err(AutumnError::conflict_msg(
                    "Somebody else saved this content while you were editing. \
                     Reload the page to see their changes before saving again.",
                ));
            }

            // `supports_revisions: false` on the registered type means exactly
            // that — no snapshot, rather than a flag the editor ignores.
            if record_revision {
                post.record_revision(conn, &summary).await?;
            }

            apply(&mut post);
            post.updated_at = chrono::Utc::now().naive_utc();
            post.lock_version += 1;

            let saved: Post = diesel::update(posts::table.find(post_id))
                .set((
                    posts::title.eq(&post.title),
                    posts::slug.eq(&post.slug),
                    posts::excerpt.eq(&post.excerpt),
                    posts::body.eq(&post.body),
                    posts::status.eq(&post.status),
                    posts::parent_id.eq(post.parent_id),
                    posts::featured_media_id.eq(post.featured_media_id),
                    posts::menu_order.eq(post.menu_order),
                    posts::comment_status.eq(&post.comment_status),
                    posts::password.eq(&post.password),
                    posts::sticky.eq(post.sticky),
                    posts::published_at.eq(post.published_at),
                    posts::lock_version.eq(post.lock_version),
                    posts::updated_at.eq(post.updated_at),
                ))
                .returning(Post::as_returning())
                .get_result(conn)
                .await?;

            prune_revisions(conn, post_id).await?;
            let _ = editor_id;
            Ok::<_, AutumnError>(saved)
        }
        .scope_boxed()
    })
    .await
}

/// Move a post to a new status, enforcing the state machine and recording the
/// change as a revision — atomically.
///
/// This is the only path that changes `posts.status` outside the repository's
/// own update, and it is what the admin's Publish / Move to Trash / Restore
/// buttons call.
pub async fn transition_status(db: &mut Db, post_id: i64, target: &str) -> AutumnResult<Post> {
    let target = target.to_owned();
    db.tx(move |conn| {
        async move {
            let post: Post = posts::table
                .find(post_id)
                .select(Post::as_select())
                .for_update()
                .first(conn)
                .await
                .map_err(AutumnError::not_found)?;

            // The macro-generated enforcing transition: an undeclared edge or a
            // failed guard is a 400 and nothing is written.
            let new_status = post.transition_status_to(&target)?;

            post.record_revision(conn, &format!("Status: {} → {new_status}", post.status))
                .await?;

            // Stamp the first publish date, and never move it afterwards — an
            // unpublish/republish cycle must not reorder the blog index.
            let published_at = match (post.published_at, new_status.as_str()) {
                (None, "publish" | "private") => Some(chrono::Utc::now().naive_utc()),
                (existing, _) => existing,
            };

            let saved: Post = diesel::update(posts::table.find(post_id))
                .set((
                    posts::status.eq(&new_status),
                    posts::published_at.eq(published_at),
                    posts::lock_version.eq(post.lock_version + 1),
                    posts::updated_at.eq(chrono::Utc::now().naive_utc()),
                ))
                .returning(Post::as_returning())
                .get_result(conn)
                .await?;

            // A post entering or leaving public visibility changes every term's
            // published-post count, so rebuild the ones it is filed under.
            recount_terms_for_post(conn, post_id).await?;

            Ok::<_, AutumnError>(saved)
        }
        .scope_boxed()
    })
    .await
}

/// Restore a post to the content of one of its revisions.
///
/// The restore is itself an edit, so it appends a new revision rather than
/// rewinding the trail — the history of a document is append-only or it is not
/// a history.
pub async fn restore_revision(db: &mut Db, post_id: i64, revision_id: i64) -> AutumnResult<Post> {
    db.tx(move |conn| {
        async move {
            let revision: Revision = revisions::table
                .find(revision_id)
                .filter(revisions::post_id.eq(post_id))
                .select(Revision::as_select())
                .first(conn)
                .await
                .map_err(AutumnError::not_found)?;

            let post: Post = posts::table
                .find(post_id)
                .select(Post::as_select())
                .for_update()
                .first(conn)
                .await
                .map_err(AutumnError::not_found)?;

            post.record_revision(conn, &format!("Restored revision #{}", revision.id))
                .await?;

            // The restore replaces content only. Status is deliberately left
            // alone: restoring the text of a draft must not silently republish
            // it, and restoring a published post's older text must not
            // unpublish it.
            let saved: Post = diesel::update(posts::table.find(post_id))
                .set((
                    posts::title.eq(&revision.title),
                    posts::excerpt.eq(&revision.excerpt),
                    posts::body.eq(&revision.body),
                    posts::lock_version.eq(post.lock_version + 1),
                    posts::updated_at.eq(chrono::Utc::now().naive_utc()),
                ))
                .returning(Post::as_returning())
                .get_result(conn)
                .await?;

            prune_revisions(conn, post_id).await?;
            Ok::<_, AutumnError>(saved)
        }
        .scope_boxed()
    })
    .await
}

/// Append the "Created" revision for a freshly-inserted post, so a post's
/// history starts at its creation rather than at its first *edit*.
///
/// A single statement, so it needs no transaction of its own.
pub async fn record_initial_revision(db: &mut Db, post: &Post) -> AutumnResult<()> {
    let conn = &mut **db;
    diesel::insert_into(revisions::table)
        .values(&NewRevision {
            post_id: post.id,
            title: post.title.clone(),
            excerpt: post.excerpt.clone(),
            body: post.body.clone(),
            status: post.status.clone(),
            author_id: Some(post.author_id),
            summary: "Created".to_owned(),
        })
        .execute(conn)
        .await?;
    Ok(())
}

/// Drop the oldest revisions beyond [`REVISION_LIMIT`].
async fn prune_revisions(conn: &mut AsyncPgConnection, post_id: i64) -> AutumnResult<()> {
    let keep: Vec<i64> = revisions::table
        .filter(revisions::post_id.eq(post_id))
        .order(revisions::created_at.desc())
        .limit(REVISION_LIMIT)
        .select(revisions::id)
        .load(conn)
        .await?;
    diesel::delete(
        revisions::table
            .filter(revisions::post_id.eq(post_id))
            .filter(revisions::id.ne_all(keep)),
    )
    .execute(conn)
    .await?;
    Ok(())
}

// ── Taxonomy assignment ─────────────────────────────────────────────────────

/// Replace the set of terms a post is filed under, and rebuild the affected
/// terms' published-post counts — atomically.
///
/// Both the terms being removed and the terms being added need recounting, so
/// the union is computed before the join rows change.
pub async fn set_post_terms(db: &mut Db, post_id: i64, term_ids: Vec<i64>) -> AutumnResult<()> {
    db.tx(move |conn| {
        async move {
            let previous: Vec<i64> = post_terms::table
                .filter(post_terms::post_id.eq(post_id))
                .select(post_terms::term_id)
                .load(conn)
                .await?;

            diesel::delete(post_terms::table.filter(post_terms::post_id.eq(post_id)))
                .execute(conn)
                .await?;

            let mut wanted = term_ids.clone();
            wanted.sort_unstable();
            wanted.dedup();
            if !wanted.is_empty() {
                let rows: Vec<_> = wanted
                    .iter()
                    .map(|term_id| {
                        (
                            post_terms::post_id.eq(post_id),
                            post_terms::term_id.eq(*term_id),
                        )
                    })
                    .collect();
                diesel::insert_into(post_terms::table)
                    .values(rows)
                    .on_conflict((post_terms::post_id, post_terms::term_id))
                    .do_nothing()
                    .execute(conn)
                    .await?;
            }

            let mut affected = previous;
            affected.extend(wanted);
            affected.sort_unstable();
            affected.dedup();
            for term_id in affected {
                recount_term(conn, term_id).await?;
            }
            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
}

/// Rebuild the counts of every term a post is filed under.
///
/// Public because the scheduled publish sweep changes `status` with its own
/// guarded `UPDATE` (so two replicas cannot both claim a post) rather than
/// through `transition_status`, and therefore has to recount explicitly.
pub async fn recount_terms_for_post_public(
    conn: &mut AsyncPgConnection,
    post_id: i64,
) -> AutumnResult<()> {
    recount_terms_for_post(conn, post_id).await
}

/// Rebuild one term's published-post count from ground truth.
pub async fn recount_term(conn: &mut AsyncPgConnection, term_id: i64) -> AutumnResult<i64> {
    let count: i64 = post_terms::table
        .inner_join(posts::table.on(posts::id.eq(post_terms::post_id)))
        .filter(post_terms::term_id.eq(term_id))
        .filter(posts::status.eq("publish"))
        .count()
        .get_result(conn)
        .await?;
    diesel::update(terms::table.find(term_id))
        .set(terms::post_count.eq(count))
        .execute(conn)
        .await?;
    Ok(count)
}

/// Rebuild the counts of every term a post is filed under.
async fn recount_terms_for_post(conn: &mut AsyncPgConnection, post_id: i64) -> AutumnResult<()> {
    let term_ids: Vec<i64> = post_terms::table
        .filter(post_terms::post_id.eq(post_id))
        .select(post_terms::term_id)
        .load(conn)
        .await?;
    for term_id in term_ids {
        recount_term(conn, term_id).await?;
    }
    Ok(())
}

/// The ids of the terms a post is filed under, within one taxonomy.
pub async fn post_term_ids(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    taxonomy: &str,
) -> AutumnResult<Vec<i64>> {
    Ok(post_terms::table
        .inner_join(terms::table.on(terms::id.eq(post_terms::term_id)))
        .filter(post_terms::post_id.eq(post_id))
        .filter(terms::taxonomy.eq(taxonomy.to_owned()))
        .select(post_terms::term_id)
        .load(conn)
        .await?)
}

// ── Comments ────────────────────────────────────────────────────────────────

/// How deep a reply to `parent_id` would sit.
///
/// Walks the parent chain rather than trusting a stored depth, so an imported
/// or hand-written row cannot understate its own nesting.
pub async fn reply_depth(conn: &mut AsyncPgConnection, parent_id: i64) -> AutumnResult<usize> {
    let mut depth = 1_usize;
    let mut cursor = Some(parent_id);
    // Bounded by the cap plus a margin: a cycle introduced by a bad import must
    // terminate the walk rather than spin.
    while let Some(id) = cursor {
        if depth > MAX_COMMENT_DEPTH + 2 {
            break;
        }
        let parent: Option<i64> = comments::table
            .find(id)
            .select(comments::parent_id)
            .first(conn)
            .await
            .optional()?
            .flatten();
        match parent {
            Some(next) => {
                depth += 1;
                cursor = Some(next);
            }
            None => break,
        }
    }
    Ok(depth)
}

/// Move a comment through the moderation queue, keeping
/// `posts.comment_count` — which counts **approved** comments, as WordPress's
/// does — correct in the same transaction.
pub async fn moderate_comment(db: &mut Db, comment_id: i64, target: &str) -> AutumnResult<Comment> {
    if !crate::hooks::COMMENT_STATUSES.contains(&target) {
        return Err(AutumnError::bad_request_msg(format!(
            "Unknown comment status `{target}`"
        )));
    }
    let target = target.to_owned();
    db.tx(move |conn| {
        async move {
            let comment: Comment = comments::table
                .find(comment_id)
                .select(Comment::as_select())
                .for_update()
                .first(conn)
                .await
                .map_err(AutumnError::not_found)?;

            if comment.status == target {
                // Idempotent: a double-clicked Approve must not increment twice.
                return Ok::<_, AutumnError>(comment);
            }

            let saved: Comment = diesel::update(comments::table.find(comment_id))
                .set(comments::status.eq(&target))
                .returning(Comment::as_returning())
                .get_result(conn)
                .await?;

            // The delta is derived from the before/after pair rather than from
            // the action name, so every path — approve, spam, trash, restore —
            // moves the counter by the right amount without its own branch.
            let delta = i64::from(target == "approved") - i64::from(comment.status == "approved");
            if delta != 0 {
                diesel::update(posts::table.find(comment.post_id))
                    .set(posts::comment_count.eq(posts::comment_count + delta))
                    .execute(conn)
                    .await?;
            }
            Ok::<_, AutumnError>(saved)
        }
        .scope_boxed()
    })
    .await
}

/// Insert a comment, bumping the approved counter when it lands approved.
pub async fn create_comment(db: &mut Db, new: crate::models::NewComment) -> AutumnResult<Comment> {
    db.tx(move |conn| {
        async move {
            let approved = new.status == "approved";
            let post_id = new.post_id;
            let saved: Comment = diesel::insert_into(comments::table)
                .values(&new)
                .returning(Comment::as_returning())
                .get_result(conn)
                .await?;
            if approved {
                diesel::update(posts::table.find(post_id))
                    .set(posts::comment_count.eq(posts::comment_count + 1))
                    .execute(conn)
                    .await?;
            }
            Ok::<_, AutumnError>(saved)
        }
        .scope_boxed()
    })
    .await
}

/// Rebuild a post's approved-comment counter from ground truth.
///
/// The repair for the drift an import, a seed or a hand-written `UPDATE` can
/// introduce — the same role `recompute_counter_caches` plays for the
/// framework's own counters.
pub async fn recount_comments(conn: &mut AsyncPgConnection, post_id: i64) -> AutumnResult<i64> {
    let count: i64 = comments::table
        .filter(comments::post_id.eq(post_id))
        .filter(comments::status.eq("approved"))
        .count()
        .get_result(conn)
        .await?;
    diesel::update(posts::table.find(post_id))
        .set(posts::comment_count.eq(count))
        .execute(conn)
        .await?;
    Ok(count)
}

/// A comment plus the author name to render above it.
#[derive(Debug, Clone)]
pub struct ThreadNode {
    pub comment: Comment,
    pub author: String,
    pub depth: usize,
    pub replies: Vec<ThreadNode>,
}

/// Load a post's approved comments as a nested thread.
///
/// One query for the comments and one for the author names, whatever the
/// nesting depth — the tree is assembled in memory, never by walking the
/// parent chain per row.
pub async fn comment_thread(
    conn: &mut AsyncPgConnection,
    post_id: i64,
) -> AutumnResult<Vec<ThreadNode>> {
    let rows: Vec<Comment> = comments::table
        .filter(comments::post_id.eq(post_id))
        .filter(comments::status.eq("approved"))
        .order((comments::created_at.asc(), comments::id.asc()))
        .select(Comment::as_select())
        .load(conn)
        .await?;

    // Registered commenters render under their account's public name, which may
    // have changed since they commented; guests render under the name they gave.
    let account_ids: Vec<i64> = rows.iter().filter_map(|c| c.author_id).collect();
    let accounts: Vec<User> = if account_ids.is_empty() {
        Vec::new()
    } else {
        users::table
            .filter(users::id.eq_any(&account_ids))
            .select(User::as_select())
            .load(conn)
            .await?
    };
    let name_of = |comment: &Comment| -> String {
        comment
            .author_id
            .and_then(|id| accounts.iter().find(|u| u.id == id))
            .map_or_else(
                || comment.display_name().to_owned(),
                |user| user.public_name().to_owned(),
            )
    };

    Ok(assemble_thread(&rows, None, 0, &name_of))
}

/// Assemble the flat comment list into a tree.
///
/// Depth is capped at [`MAX_COMMENT_DEPTH`]: a chain deeper than that (only
/// reachable through an import or a direct write, since the write path refuses
/// it) is flattened into its ancestor rather than recursed into, so a malformed
/// chain cannot overflow the stack during render.
pub fn assemble_thread(
    rows: &[Comment],
    parent: Option<i64>,
    depth: usize,
    name_of: &impl Fn(&Comment) -> String,
) -> Vec<ThreadNode> {
    if depth > MAX_COMMENT_DEPTH {
        return Vec::new();
    }
    rows.iter()
        .filter(|c| c.parent_id == parent)
        .map(|comment| ThreadNode {
            comment: comment.clone(),
            author: name_of(comment),
            depth,
            replies: assemble_thread(rows, Some(comment.id), depth + 1, name_of),
        })
        .collect()
}

/// Convert a thread into the framework's renderable comment views.
///
/// The table and the moderation queue are this app's, but the *renderer* is the
/// framework's: `widgets::comment_thread` emits nested `<ol>`s with the depth
/// exposed to assistive technology and a no-JavaScript reply form on every
/// node. `CommentView`'s fields are public, so owning the storage costs nothing
/// on the render side.
#[must_use]
pub fn to_comment_views(nodes: &[ThreadNode]) -> Vec<autumn_web::widgets::CommentView> {
    nodes
        .iter()
        .map(|node| autumn_web::widgets::CommentView {
            id: node.comment.id,
            author: node.author.clone(),
            body: node.comment.body.clone(),
            datetime: Some(
                node.comment
                    .created_at
                    .and_utc()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ),
            timestamp: node.comment.created_at.format("%Y-%m-%d %H:%M").to_string(),
            replies: to_comment_views(&node.replies),
        })
        .collect()
}

/// A post's revision history, newest first.
pub async fn revisions_for(db: &mut Db, post_id: i64) -> AutumnResult<Vec<Revision>> {
    Ok(revisions::table
        .filter(revisions::post_id.eq(post_id))
        .order((revisions::created_at.desc(), revisions::id.desc()))
        .select(Revision::as_select())
        .load(&mut **db)
        .await?)
}

/// The terms every post by `author_id` is filed under.
///
/// Collected *before* the author is deleted: `posts.author_id` cascades, which
/// takes the `post_terms` rows with it, so afterwards there is nothing left to
/// tell you which terms need rebuilding.
pub async fn term_ids_for_author(db: &mut Db, author_id: i64) -> AutumnResult<Vec<i64>> {
    let mut ids: Vec<i64> = post_terms::table
        .inner_join(posts::table.on(posts::id.eq(post_terms::post_id)))
        .filter(posts::author_id.eq(author_id))
        .select(post_terms::term_id)
        .load(&mut **db)
        .await?;
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// Rebuild the published-post counts of the given terms.
pub async fn recount_terms(db: &mut Db, term_ids: &[i64]) -> AutumnResult<()> {
    for term_id in term_ids {
        recount_term(db, *term_id).await?;
    }
    Ok(())
}

/// Whether making `candidate_parent_id` the parent of `post_id` would create a
/// cycle — i.e. the candidate is the post itself or one of its descendants.
///
/// Walks up from the candidate rather than down from the post: the ancestor
/// chain is bounded by the tree's depth, while the descendant set is not. A
/// cycle already present in the data (only reachable by a direct write)
/// terminates the walk at the depth bound rather than spinning.
pub async fn would_create_cycle(
    db: &mut Db,
    post_id: i64,
    candidate_parent_id: i64,
) -> AutumnResult<bool> {
    const MAX_DEPTH: usize = 64;

    if candidate_parent_id == post_id {
        return Ok(true);
    }
    let mut cursor = Some(candidate_parent_id);
    let mut steps = 0usize;
    while let Some(current) = cursor {
        if current == post_id {
            return Ok(true);
        }
        steps += 1;
        if steps > MAX_DEPTH {
            // A pre-existing cycle among other rows. Refusing is the safe
            // answer: it cannot make the tree worse.
            return Ok(true);
        }
        cursor = posts::table
            .find(current)
            .select(posts::parent_id)
            .first::<Option<i64>>(&mut **db)
            .await
            .optional()?
            .flatten();
    }
    Ok(false)
}

// ── Accounts ────────────────────────────────────────────────────────────────

/// Advisory-lock key serializing every change to the site's administrator set.
///
/// Two questions in this app are read-then-write over the *whole* users table
/// rather than over one row, so no row lock can serialize them: "is this the
/// first account?" (which grants ownership) and "is this the last
/// administrator?" (which refuses removal). Concurrent requests would each
/// read a snapshot taken before the other wrote — electing two owners, or
/// removing both remaining administrators and locking everyone out.
///
/// A transaction-scoped advisory lock is the right shape: it is released on
/// commit or rollback, needs no row to exist, and costs one statement. The key
/// is arbitrary but must be stable and unique within the database.
const ADMIN_SET_LOCK_KEY: i64 = 7_717_260_231_001;

/// Create an account, electing the first one created as the site owner —
/// atomically.
///
/// The election and the insert share one transaction under
/// [`ADMIN_SET_LOCK_KEY`]. Without it, two signups can both observe an empty
/// table and both be persisted as administrators: the window is not
/// theoretical, because password hashing (bcrypt, deliberately slow) happens
/// between the check and the insert.
pub async fn register_user(db: &mut Db, new: crate::models::NewUser) -> AutumnResult<User> {
    let mut new = new;
    crate::hooks::normalize_new_user(&mut new)?;
    db.tx(move |conn| {
        async move {
            diesel::sql_query(format!(
                "SELECT pg_advisory_xact_lock({ADMIN_SET_LOCK_KEY})"
            ))
            .execute(conn)
            .await?;

            let existing: i64 = users::table.count().get_result(conn).await?;
            if existing == 0 {
                new.role = crate::capabilities::Role::Administrator.slug().to_owned();
            }

            let created: User = diesel::insert_into(users::table)
                .values(&new)
                .returning(User::as_returning())
                .get_result(conn)
                .await?;
            Ok::<_, AutumnError>(created)
        }
        .scope_boxed()
    })
    .await
}

/// Refuse a change that would leave the site with no administrator, and apply
/// it — atomically.
///
/// `mutate` runs inside the same transaction as the count, under
/// [`ADMIN_SET_LOCK_KEY`], so two requests demoting each other cannot both see
/// a spare administrator and both proceed.
async fn with_administrator_guard<F>(
    db: &mut Db,
    target_id: i64,
    new_role: crate::capabilities::Role,
    mutate: F,
) -> AutumnResult<()>
where
    F: for<'a> FnOnce(
            &'a mut AsyncPgConnection,
        ) -> scoped_futures::ScopedBoxFuture<'a, 'a, AutumnResult<()>>
        + Send
        + 'static,
{
    db.tx(move |conn| {
        async move {
            diesel::sql_query(format!(
                "SELECT pg_advisory_xact_lock({ADMIN_SET_LOCK_KEY})"
            ))
            .execute(conn)
            .await?;

            let target: User = users::table
                .find(target_id)
                .select(User::as_select())
                .first(conn)
                .await
                .map_err(AutumnError::not_found)?;

            let administrator = crate::capabilities::Role::Administrator;
            let losing_an_administrator =
                target.role() == administrator && new_role != administrator;
            if losing_an_administrator {
                let remaining: i64 = users::table
                    .filter(users::role.eq(administrator.slug()))
                    .count()
                    .get_result(conn)
                    .await?;
                if remaining <= 1 {
                    return Err(AutumnError::unprocessable_msg(
                        "This is the only administrator account; promote another user first",
                    ));
                }
            }

            mutate(conn).await?;
            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
}

/// Change an account's role and profile fields, guarding the last
/// administrator.
pub async fn update_user(
    db: &mut Db,
    target_id: i64,
    role: crate::capabilities::Role,
    email: String,
    display_name: String,
    bio: String,
    website: String,
) -> AutumnResult<()> {
    with_administrator_guard(db, target_id, role, move |conn| {
        async move {
            diesel::update(users::table.find(target_id))
                .set((
                    users::role.eq(role.slug()),
                    users::email.eq(email.trim().to_lowercase()),
                    users::display_name.eq(display_name),
                    users::bio.eq(bio),
                    users::website.eq(website),
                    users::updated_at.eq(chrono::Utc::now().naive_utc()),
                ))
                .execute(conn)
                .await?;
            Ok(())
        }
        .scope_boxed()
    })
    .await
}

/// Delete an account, guarding the last administrator.
///
/// Returns the terms its cascaded posts were filed under, so the caller can
/// rebuild their counts — the cascade reaches `post_terms` and nothing in it
/// maintains `terms.post_count`.
pub async fn delete_user(db: &mut Db, target_id: i64) -> AutumnResult<()> {
    // Collected before the delete: the cascade takes the `post_terms` rows
    // with it, so afterwards nothing names the terms that need rebuilding.
    let affected_terms = term_ids_for_author(db, target_id).await?;
    // Deleting is a demotion to "no role at all", so it takes the same guard.
    // The recounts run inside the same transaction as the cascade: doing them
    // afterwards means a transient failure leaves the account and its posts
    // permanently gone with the counts stale, and a retry finds no user to
    // delete, so nothing ever repairs them.
    with_administrator_guard(
        db,
        target_id,
        crate::capabilities::Role::Subscriber,
        move |conn| {
            async move {
                diesel::delete(users::table.find(target_id))
                    .execute(conn)
                    .await?;
                for term_id in affected_terms {
                    recount_term(conn, term_id).await?;
                }
                Ok(())
            }
            .scope_boxed()
        },
    )
    .await
}

/// The post types that are reachable on the public front end.
///
/// Every public listing query filters on this. It exists because "published"
/// and "publicly routable" are two different questions, and answering only the
/// first — once per query, in whichever query was written most recently — is
/// how the same defect kept reappearing on a new screen each review round.
/// Whether one named post type is reachable on the public front end.
#[must_use]
pub fn is_public_type(post_type: &str) -> bool {
    crate::content_types::find_post_type(post_type).is_some_and(|registered| registered.public)
}

#[must_use]
pub fn public_type_slugs() -> Vec<String> {
    crate::content_types::all_post_types()
        .into_iter()
        .filter(|registered| registered.public)
        .map(|registered| registered.slug.to_owned())
        .collect()
}

// ── Set-based listing queries ───────────────────────────────────────────────
//
// Every public listing goes through these. They exist because the obvious
// repository shape — load the rows, sort in Rust, `skip`/`take` — makes each
// request cost the size of the whole corpus rather than the size of the page
// being rendered, and the sidebar's Recent Posts widget runs on essentially
// every public page. Ordering, filtering, counting and pagination all belong
// in SQL; the repository codegen has no finder that can express them together,
// so these are hand-written against a pooled connection.

/// One page of published content of a type, newest first, with the total.
///
/// Sticky posts sort first, then by publish date — WordPress's blog-index
/// ordering — and the whole ordering is done by the database so `LIMIT`/
/// `OFFSET` mean what they say.
pub async fn published_posts_page(
    conn: &mut AsyncPgConnection,
    post_type: &str,
    offset: i64,
    limit: i64,
) -> AutumnResult<(Vec<Post>, i64)> {
    // A named type still has to *be* public. `post` and `page` are registered
    // like any other and can be re-registered `public: false` through the
    // supported replacement mechanism, so naming the type is not the same as
    // establishing it has a public route.
    if !is_public_type(post_type) {
        return Ok((Vec::new(), 0));
    }

    let total: i64 = posts::table
        .filter(posts::post_type.eq(post_type))
        .filter(posts::status.eq("publish"))
        .count()
        .get_result(conn)
        .await?;

    let rows: Vec<Post> = posts::table
        .filter(posts::post_type.eq(post_type))
        .filter(posts::status.eq("publish"))
        .order((
            posts::sticky.desc(),
            posts::published_at.desc(),
            posts::id.desc(),
        ))
        .offset(offset.max(0))
        .limit(limit.max(0))
        .select(Post::as_select())
        .load(conn)
        .await?;

    Ok((rows, total))
}

/// The newest published posts of a type — the sidebar's Recent Posts.
pub async fn recent_published_posts(
    conn: &mut AsyncPgConnection,
    post_type: &str,
    limit: i64,
) -> AutumnResult<Vec<Post>> {
    // See `published_posts_page`: naming a type is not the same as
    // establishing it is publicly routable.
    if !is_public_type(post_type) {
        return Ok(Vec::new());
    }
    Ok(posts::table
        .filter(posts::post_type.eq(post_type))
        .filter(posts::status.eq("publish"))
        .order((posts::published_at.desc(), posts::id.desc()))
        .limit(limit.max(0))
        .select(Post::as_select())
        .load(conn)
        .await?)
}

/// One page of the published posts filed under a term, with the total.
///
/// A single join with `LIMIT`/`OFFSET`, plus a count. The previous shape —
/// read every filing, fetch each post by id, then paginate in memory — made a
/// popular category URL an unauthenticated way to issue thousands of queries
/// per request.
pub async fn published_posts_in_term(
    conn: &mut AsyncPgConnection,
    term_id: i64,
    offset: i64,
    limit: i64,
) -> AutumnResult<(Vec<Post>, i64)> {
    let public_types = public_type_slugs();

    let total: i64 = post_terms::table
        .inner_join(posts::table.on(posts::id.eq(post_terms::post_id)))
        .filter(post_terms::term_id.eq(term_id))
        .filter(posts::status.eq("publish"))
        .filter(posts::post_type.eq_any(&public_types))
        .count()
        .get_result(conn)
        .await?;

    let rows: Vec<Post> = post_terms::table
        .inner_join(posts::table.on(posts::id.eq(post_terms::post_id)))
        .filter(post_terms::term_id.eq(term_id))
        .filter(posts::status.eq("publish"))
        .filter(posts::post_type.eq_any(&public_types))
        .order((posts::published_at.desc(), posts::id.desc()))
        .offset(offset.max(0))
        .limit(limit.max(0))
        .select(Post::as_select())
        .load(conn)
        .await?;

    Ok((rows, total))
}

/// One page of a date archive.
pub async fn published_posts_in_period(
    conn: &mut AsyncPgConnection,
    post_type: &str,
    from: chrono::NaiveDateTime,
    until: chrono::NaiveDateTime,
    offset: i64,
    limit: i64,
) -> AutumnResult<(Vec<Post>, i64)> {
    // Same guard as every other listing: naming a type is not the same as
    // establishing it has a public route.
    if !is_public_type(post_type) {
        return Ok((Vec::new(), 0));
    }

    let total: i64 = posts::table
        .filter(posts::post_type.eq(post_type))
        .filter(posts::status.eq("publish"))
        .filter(posts::published_at.ge(from))
        .filter(posts::published_at.lt(until))
        .count()
        .get_result(conn)
        .await?;

    let rows: Vec<Post> = posts::table
        .filter(posts::post_type.eq(post_type))
        .filter(posts::status.eq("publish"))
        .filter(posts::published_at.ge(from))
        .filter(posts::published_at.lt(until))
        .order((posts::published_at.desc(), posts::id.desc()))
        .offset(offset.max(0))
        .limit(limit.max(0))
        .select(Post::as_select())
        .load(conn)
        .await?;

    Ok((rows, total))
}

/// One page of an author's published posts.
/// This archive spans post types, so it filters on registered visibility as
/// well as status — a `public: false` type has no public route, and a listing
/// that renders its title and a body-derived excerpt is a public route.
pub async fn published_posts_by_author(
    conn: &mut AsyncPgConnection,
    author_id: i64,
    offset: i64,
    limit: i64,
) -> AutumnResult<(Vec<Post>, i64)> {
    let public_types = public_type_slugs();

    let total: i64 = posts::table
        .filter(posts::author_id.eq(author_id))
        .filter(posts::status.eq("publish"))
        .filter(posts::post_type.eq_any(&public_types))
        .count()
        .get_result(conn)
        .await?;

    let rows: Vec<Post> = posts::table
        .filter(posts::author_id.eq(author_id))
        .filter(posts::status.eq("publish"))
        .filter(posts::post_type.eq_any(&public_types))
        .order((posts::published_at.desc(), posts::id.desc()))
        .offset(offset.max(0))
        .limit(limit.max(0))
        .select(Post::as_select())
        .load(conn)
        .await?;

    Ok((rows, total))
}

/// The distinct authors of published, publicly-routable content.
///
/// One query rather than loading every post to deduplicate its `author_id` and
/// then querying per author — the cost of listing bylines should scale with the
/// number of authors, not with the size of the corpus.
pub async fn published_authors(conn: &mut AsyncPgConnection) -> AutumnResult<Vec<User>> {
    let author_ids: Vec<i64> = posts::table
        .filter(posts::status.eq("publish"))
        .filter(posts::post_type.eq_any(public_type_slugs()))
        .select(posts::author_id)
        .distinct()
        .load(conn)
        .await?;

    if author_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(users::table
        .filter(users::id.eq_any(&author_ids))
        .order(users::username.asc())
        .select(User::as_select())
        .load(conn)
        .await?)
}

/// Whether a single-segment slug would be resolved as a date archive.
///
/// `permalinks::resolve` treats a lone four-digit numeric segment as a year.
/// See `ensure_unique_slug` for why that shape is reserved rather than
/// resolved by fallback.
#[must_use]
pub fn reads_as_date_archive(slug: &str) -> bool {
    slug.len() == 4 && slug.chars().all(|c| c.is_ascii_digit())
}

/// Bare paths the application's own routes claim.
///
/// Kept beside the reservation rather than derived from the router: the route
/// table is built from typed handlers with no runtime list of first segments,
/// and a wrong answer here is a silently unreachable page. The integration
/// suite's `every_reserved_prefix_has_a_literal_route` covers the same names
/// from the other direction, so a route added without updating this list is
/// visible there.
const RESERVED_PATHS: &[&str] = &[
    "admin",
    "api",
    "comments",
    "feed",
    "login",
    "logout",
    "media",
    "register",
    "search",
    "unlock",
    "sitemap.xml",
    "robots.txt",
    "static",
    "archives",
    "author",
    "category",
    "tag",
];

/// Whether a slug would be shadowed by one of the application's own routes.
#[must_use]
pub fn is_reserved_path(slug: &str) -> bool {
    RESERVED_PATHS.contains(&slug)
}

/// A slug that is free across every post type sharing the bare URL path.
///
/// `posts` is unique on `(post_type, slug)`, so a post and a page may both be
/// slugged `about` — but both mint `/about`, and the front controller can only
/// serve one of them, leaving the other unreachable at its own canonical URL.
/// WordPress solves this by making the slug unique across the types that share
/// the root, appending `-2`, `-3`, … ; this does the same.
///
/// Only `post` and `page` compete: a custom type is addressed under its own
/// prefix (`/product/widget`), so it cannot collide with them.
pub async fn ensure_unique_slug(
    conn: &mut AsyncPgConnection,
    post_type: &str,
    desired: &str,
    exclude_id: Option<i64>,
) -> AutumnResult<String> {
    // Which rows this slug must be unique against. `post` and `page` share the
    // bare URL path, so they compete with each other; a custom type is
    // addressed under its own prefix (`/product/widget`) and competes only with
    // itself — but it still has to compete, because `idx_posts_type_slug`
    // requires uniqueness within a type. Returning early for custom types made
    // a second item with the same title fail with a constraint error instead of
    // getting the usual `-2`.
    const BARE_PATH_TYPES: &[&str] = &["post", "page"];
    let competing_types: Vec<&str> = if BARE_PATH_TYPES.contains(&post_type) {
        BARE_PATH_TYPES.to_vec()
    } else {
        vec![post_type]
    };

    // Two shapes are reserved because a literal route already owns the bare
    // path they would mint, so content given one is unreachable at its own
    // canonical URL:
    //
    //   * a four-digit slug, which the resolver reads as a year archive; and
    //   * the names of the application's own literal routes.
    //
    // Reserving is what keeps both features working. Falling back to content
    // when the archive or route "has nothing" would instead make `/2026` or
    // `/search` mean different things depending on what happens to exist.
    let shadowed_by_a_route = BARE_PATH_TYPES.contains(&post_type)
        && (reads_as_date_archive(desired) || is_reserved_path(desired));
    let mut candidate = if shadowed_by_a_route {
        format!("{desired}-2")
    } else {
        desired.to_owned()
    };
    for suffix in 2..=200u32 {
        let mut query = posts::table
            .filter(posts::slug.eq(candidate.clone()))
            .filter(posts::post_type.eq_any(&competing_types))
            .into_boxed();
        if let Some(id) = exclude_id {
            query = query.filter(posts::id.ne(id));
        }
        let taken: i64 = query.count().get_result(conn).await?;
        if taken == 0 {
            return Ok(candidate);
        }
        candidate = format!("{desired}-{suffix}");
    }
    // 200 collisions on one slug is not a naming accident. Refuse rather than
    // loop or silently overwrite.
    Err(AutumnError::unprocessable_msg(
        "Too many posts share this slug; choose a different one",
    ))
}

/// The deepest page hierarchy the site will address.
///
/// The permalink builder walks a page's ancestors to construct its path, and
/// the resolver walks back down from a row whose parent is `NULL`. Both have to
/// agree on a bound, or a page deeper than the walker's limit gets a URL
/// starting mid-tree that resolves to nothing. Enforcing it where a parent is
/// *assigned* is what keeps every stored page addressable, rather than
/// truncating at render time and emitting a 404 link.
pub const MAX_PAGE_DEPTH: usize = 8;

/// How many ancestors a page would have under `candidate_parent_id`.
pub async fn depth_under(db: &mut Db, candidate_parent_id: i64) -> AutumnResult<usize> {
    let mut depth = 1usize;
    let mut cursor = Some(candidate_parent_id);
    while let Some(current) = cursor {
        if depth > MAX_PAGE_DEPTH + 2 {
            break;
        }
        cursor = posts::table
            .find(current)
            .select(posts::parent_id)
            .first::<Option<i64>>(&mut **db)
            .await
            .optional()?
            .flatten();
        if cursor.is_some() {
            depth += 1;
        }
    }
    Ok(depth)
}

/// Validate a proposed parent for a page: no cycle, and within the depth the
/// permalink builder can render.
///
/// One function so create and update cannot diverge — the update path had this
/// and creation did not, which let repeated creates build a hierarchy deeper
/// than `page_ancestry` walks, whose canonical URL then starts mid-tree and
/// resolves to nothing. `post_id` is `None` when creating (no row to cycle
/// back to yet).
pub async fn validate_parent(
    db: &mut Db,
    post_id: Option<i64>,
    post_type: &str,
    candidate_parent_id: i64,
) -> AutumnResult<()> {
    // The parent must be a live row of the SAME hierarchical type. The foreign
    // key only says "some post", so a crafted form could name a normal post:
    // `page_ancestry` would then put that row's slug in the canonical URL while
    // `resolve_page_path` requires every ancestor to be a page, leaving the
    // child permanently unreachable.
    let parent: Option<Post> = posts::table
        .find(candidate_parent_id)
        .select(Post::as_select())
        .first(&mut **db)
        .await
        .optional()?;
    let Some(parent) = parent else {
        return Err(AutumnError::unprocessable_msg("That parent does not exist"));
    };
    if parent.post_type != post_type || parent.status == "trash" {
        return Err(AutumnError::unprocessable_msg(
            "A parent must be another item of the same type, and not in the trash",
        ));
    }

    if let Some(post_id) = post_id
        && would_create_cycle(db, post_id, candidate_parent_id).await?
    {
        return Err(AutumnError::unprocessable_msg(
            "A page cannot be placed under itself or one of its own children",
        ));
    }
    if depth_under(db, candidate_parent_id).await? >= MAX_PAGE_DEPTH {
        return Err(AutumnError::unprocessable_msg(format!(
            "Pages can be nested at most {MAX_PAGE_DEPTH} levels deep"
        )));
    }
    Ok(())
}

/// Re-parent a post. Used by the importer's ancestry pass.
pub async fn set_post_parent(db: &mut Db, post_id: i64, parent_id: i64) -> AutumnResult<()> {
    if would_create_cycle(db, post_id, parent_id).await? {
        return Ok(());
    }
    diesel::update(posts::table.find(post_id))
        .set(posts::parent_id.eq(parent_id))
        .execute(&mut **db)
        .await?;
    Ok(())
}

/// One page of published, publicly-routable content matching a full-text query.
///
/// The visibility predicates are part of the query and the count, not a filter
/// applied to the page that comes back. Filtering afterwards paginates the
/// unrestricted result set: a page can return empty while public matches sit on
/// later pages, and the total would count — and so disclose the number of —
/// draft and non-public-type matches.
///
/// Two statements: the ranked ids (bounded by `LIMIT`), then the rows for those
/// ids. `#[model]` derives `Queryable`, not `QueryableByName`, so the rows
/// cannot be loaded by `sql_query` directly — and the `search_vector` generated
/// column is deliberately absent from `crate::schema::posts` (the
/// `#[searchable]` codegen owns it), so the match predicate has to be raw SQL.
/// Every value is bound, never interpolated.
///
/// `websearch_to_tsquery` rather than `plainto_tsquery`: it accepts what a
/// person actually types into a search box — quoted phrases, `or`, `-term` —
/// instead of erroring on it.
pub async fn search_published(
    conn: &mut AsyncPgConnection,
    query: &str,
    public_types: &[String],
    offset: i64,
    limit: i64,
) -> AutumnResult<(Vec<Post>, usize)> {
    use diesel::sql_types::{Array, BigInt, Text};

    if public_types.is_empty() {
        return Ok((Vec::new(), 0));
    }

    #[derive(diesel::QueryableByName)]
    struct Total {
        #[diesel(sql_type = BigInt)]
        count: i64,
    }

    #[derive(diesel::QueryableByName)]
    struct MatchedId {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }

    const MATCH_PREDICATE: &str = "status = 'publish' \
         AND search_vector @@ websearch_to_tsquery('english', $1) \
         AND post_type = ANY($2)";

    let total: i64 = diesel::sql_query(format!(
        "SELECT COUNT(*) AS count FROM posts WHERE {MATCH_PREDICATE}"
    ))
    .bind::<Text, _>(query)
    .bind::<Array<Text>, _>(public_types.to_vec())
    .get_result::<Total>(conn)
    .await?
    .count;

    let matched: Vec<i64> = diesel::sql_query(format!(
        "SELECT id FROM posts WHERE {MATCH_PREDICATE} \
         ORDER BY ts_rank(search_vector, websearch_to_tsquery('english', $1)) DESC, \
                  published_at DESC NULLS LAST, id DESC \
         LIMIT $3 OFFSET $4"
    ))
    .bind::<Text, _>(query)
    .bind::<Array<Text>, _>(public_types.to_vec())
    .bind::<BigInt, _>(limit.max(0))
    .bind::<BigInt, _>(offset.max(0))
    .load::<MatchedId>(conn)
    .await?
    .into_iter()
    .map(|row| row.id)
    .collect();

    if matched.is_empty() {
        return Ok((Vec::new(), usize::try_from(total).unwrap_or(0)));
    }

    let mut rows: Vec<Post> = posts::table
        .filter(posts::id.eq_any(&matched))
        .select(Post::as_select())
        .load(conn)
        .await?;

    // Restore the rank order the id query established; `eq_any` does not
    // preserve it.
    rows.sort_by_key(|post| {
        matched
            .iter()
            .position(|id| *id == post.id)
            .unwrap_or(usize::MAX)
    });

    Ok((rows, usize::try_from(total).unwrap_or(0)))
}

/// Assign a theme location to a new menu, clearing the previous holder — in one
/// transaction.
///
/// Only one menu can hold a location, so creating a replacement has to detach
/// the incumbent. Doing that as two statements means a failed insert (a
/// duplicate slug is the easy way to get one) leaves the site with *no* menu at
/// that location — the navigation simply disappears, from a request that
/// reported an error.
pub async fn replace_menu_at_location(db: &mut Db, name: &str, location: &str) -> AutumnResult<()> {
    let name = name.to_owned();
    let location = location.to_owned();
    db.tx(move |conn| {
        async move {
            if !location.is_empty() {
                diesel::update(menus::table.filter(menus::location.eq(&location)))
                    .set(menus::location.eq(""))
                    .execute(conn)
                    .await?;
            }
            diesel::insert_into(menus::table)
                .values((
                    menus::name.eq(&name),
                    menus::slug.eq(autumn_web::slugify(&name)),
                    menus::location.eq(&location),
                ))
                .execute(conn)
                .await?;
            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
}

/// Terms of a taxonomy that have at least one published post, bounded.
///
/// The `post_count > 0` filter and the limit are both applied in SQL. The
/// sitemap is an unauthenticated endpoint whose advertised cap has to bound the
/// work, not only the response.
pub async fn populated_terms(
    conn: &mut AsyncPgConnection,
    taxonomy: &str,
    limit: i64,
) -> AutumnResult<Vec<Term>> {
    Ok(terms::table
        .filter(terms::taxonomy.eq(taxonomy))
        .filter(terms::post_count.gt(0))
        .order((terms::post_count.desc(), terms::id.asc()))
        .limit(limit.max(0))
        .select(Term::as_select())
        .load(conn)
        .await?)
}

/// One page of a taxonomy's terms, ordered by name, bounded in SQL.
///
/// The public terms endpoint is unauthenticated, so "return everything" makes
/// its database, memory and response cost a function of the site's taxonomy
/// size rather than of the request. The caller clamps `limit`; the offset and
/// the limit are both applied by Postgres.
pub async fn terms_page(
    conn: &mut AsyncPgConnection,
    taxonomy: &str,
    offset: i64,
    limit: i64,
) -> AutumnResult<Vec<Term>> {
    Ok(terms::table
        .filter(terms::taxonomy.eq(taxonomy))
        .order((terms::name.asc(), terms::id.asc()))
        .offset(offset.max(0))
        .limit(limit.max(0))
        .select(Term::as_select())
        .load(conn)
        .await?)
}

/// The most approved comments one rendered thread holds.
///
/// A public post's thread is served to anyone, and with guest comments enabled
/// anyone can also grow it. Without a bound, a post that has accumulated tens
/// of thousands of comments makes every single page view cost the whole set in
/// database and application memory — and the pending, spam and trashed rows a
/// moderation queue collects made it worse, because they were loaded and then
/// discarded in Rust. The bound is generous enough that no real discussion
/// reaches it, and the renderer says so when one does.
pub const MAX_THREAD_COMMENTS: i64 = 200;

/// A post's approved comments, oldest first, bounded.
///
/// Both the status filter and the bound are applied in SQL. Ordering oldest
/// first is what makes truncation safe for threading: a reply is always created
/// after the comment it replies to, so any comment inside the window has its
/// parent inside the window too, and `assemble_thread` never drops a subtree
/// because its root fell off the end.
pub async fn approved_comments_page(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    offset: i64,
    limit: i64,
) -> AutumnResult<Vec<Comment>> {
    Ok(comments::table
        .filter(comments::post_id.eq(post_id))
        .filter(comments::status.eq("approved"))
        .order((comments::created_at.asc(), comments::id.asc()))
        .offset(offset.max(0))
        .limit(limit.max(0))
        .select(Comment::as_select())
        .load(conn)
        .await?)
}

/// How many approved comments a post has, counted in SQL.
pub async fn approved_comment_count(
    conn: &mut AsyncPgConnection,
    post_id: i64,
) -> AutumnResult<i64> {
    Ok(comments::table
        .filter(comments::post_id.eq(post_id))
        .filter(comments::status.eq("approved"))
        .count()
        .get_result(conn)
        .await?)
}

/// Publish one due scheduled post and rebuild its terms' counts — atomically.
///
/// Returns whether this call was the one that published it.
///
/// The two halves must commit together. Recounting afterwards and returning the
/// error on failure does not make it retryable: the sweep selects only
/// `status = 'future'`, so once the row is `publish` nothing ever revisits it
/// and the counts stay wrong for good. Rolling the publication back instead
/// leaves the post `future`, which the next sweep picks up and retries whole.
///
/// The `UPDATE` is guarded on the status *and* on the `published_at` the caller
/// observed, so two replicas racing the sweep cannot both claim the post, and an
/// editor who reschedules between the query and this call prevents publication
/// rather than being overridden by it.
pub async fn publish_due_post(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    observed_published_at: Option<chrono::NaiveDateTime>,
    new_status: &str,
) -> AutumnResult<bool> {
    use diesel_async::AsyncConnection as _;

    let new_status = new_status.to_owned();
    conn.transaction(async |conn| {
        let now = chrono::Utc::now().naive_utc();
        let updated = diesel::update(
            posts::table
                .find(post_id)
                .filter(posts::status.eq("future"))
                .filter(posts::published_at.eq(observed_published_at))
                .filter(posts::published_at.le(now)),
        )
        .set((posts::status.eq(&new_status), posts::updated_at.eq(now)))
        .execute(conn)
        .await?;

        if updated == 0 {
            return Ok(false);
        }

        // The post was `future` when its terms were last counted, so every
        // term it is filed under excluded it. This is the moment it became
        // public.
        recount_terms_for_post(conn, post_id).await?;
        Ok::<_, AutumnError>(true)
    })
    .await
}

#[cfg(test)]
mod slug_shape_tests {
    use super::reads_as_date_archive;

    #[test]
    fn only_a_lone_four_digit_slug_reads_as_a_year() {
        // `permalinks::resolve` reads this shape as a year archive, before it
        // ever looks for content.
        assert!(reads_as_date_archive("2026"));
        assert!(reads_as_date_archive("1999"));
        // Everything else is ordinary content.
        assert!(!reads_as_date_archive("202"));
        assert!(!reads_as_date_archive("20260"));
        assert!(!reads_as_date_archive("2026-review"));
        assert!(!reads_as_date_archive("about"));
        assert!(!reads_as_date_archive(""));
    }
}
