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

use crate::models::{Comment, NewRevision, Post, Revision, User};
use crate::schema::{comments, post_terms, posts, revisions, terms, users};

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
pub async fn update_post_with_revision(
    db: &mut Db,
    post_id: i64,
    editor_id: i64,
    summary: &str,
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

            post.record_revision(conn, &summary).await?;

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
