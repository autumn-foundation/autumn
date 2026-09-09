//! The content service — the transactional operations the routes are built from.
//!
//! Everything here is an operation that must be atomic across more than one
//! table: publishing a post *and* recording its revision, moving a comment
//! through moderation *and* adjusting the approved-comment counter, replacing a
//! post's terms *and* the affected terms' post counts. Handlers call these; they
//! never open a transaction themselves.

use autumn_web::AutumnError;
use autumn_web::AutumnResult;
use diesel::prelude::*;
use diesel_async::{AsyncConnection as _, AsyncPgConnection, RunQueryDsl};
use scoped_futures::ScopedFutureExt;

use crate::models::{Comment, NewRevision, Post, Revision, Term, User};
use crate::schema::{
    attachments, comments, menu_items, menus, post_meta, post_terms, posts, revisions, terms,
    users, widgets,
};

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
/// Everything about an edit except the edit itself: who made it, whether it is
/// worth a snapshot, and what the transaction is allowed to touch.
///
/// A struct rather than five more parameters — the call site reads as prose,
/// and a new question about an edit does not mean another positional `bool`
/// nobody can tell apart at the call site.
pub struct EditContext {
    /// The account that made the edit — who the revision is credited to, which
    /// is not necessarily the post's author.
    pub editor_id: i64,
    /// The revision's one-line description.
    pub summary: String,
    /// The version the editor's form was rendered from. When it does not match
    /// the row read under the lock, someone else saved in between and this
    /// submission is built on content that no longer exists — the write is
    /// refused with `409 Conflict` rather than overwriting their work. `None`
    /// skips the check, for callers with no form behind them.
    pub expected_lock_version: Option<i32>,
    /// Whether the registered type takes revisions.
    pub record_revision: bool,
    /// Whether this *transaction* may touch the page hierarchy — a re-parent
    /// here, or a trash transition composed after it. Declared by the caller
    /// because the answer is about the transaction, not about this call.
    pub may_touch_hierarchy: bool,
    /// The account making the edit, re-authorized against the row *as locked*.
    ///
    /// `None` is a path with no acting user — the scheduler, the seeder and the
    /// importer, whose authority is the process's rather than a session's.
    pub actor: Option<crate::models::User>,
}

pub async fn update_post_with_revision(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    context: EditContext,
    apply: impl for<'a> FnOnce(&'a mut Post) + Send + 'static,
) -> AutumnResult<Post> {
    let EditContext {
        editor_id,
        summary,
        expected_lock_version,
        record_revision,
        may_touch_hierarchy,
        actor,
    } = context;
    conn.transaction(async move |conn| {
        // The hierarchy lock first, when this edit could need it — see
        // `transition_status` for the order and why it is that way round.
        //
        // Whether it is needed cannot be decided here: the answer depends on
        // the *transaction*, not on this call. Only a re-parent needs it, and a
        // re-parent is only visible after `apply` has run against the locked
        // row — by which point the order is already wrong. So the caller
        // declares it, and a caller composing this with a trash transition
        // declares it for the whole transaction rather than letting the second
        // operation reach for the lock behind the first one's row lock.
        if may_touch_hierarchy {
            lock_page_hierarchy(conn).await?;
        }

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

        // Authorization is re-checked against the row *as locked*, exactly as
        // `transition_status` does — the handler's check ran on a connection
        // released before this transaction opened. A Contributor saving their
        // draft while an Editor publishes it was authorized by the stale
        // `draft` and then wrote over content they may no longer touch.
        //
        // The stale-edit check below does not close this: `lock_version` is
        // form data, so a crafted request can name the version the row will
        // have *after* the transition it is racing and sail through. An
        // authorization decision cannot rest on a number the caller chooses.
        if let Some(actor) = &actor
            && !crate::capabilities::can_edit_post(
                actor.role(),
                actor.id,
                post.author_id,
                &post.status,
            )
        {
            return Err(AutumnError::forbidden_msg(
                "You do not have permission to edit this content",
            ));
        }

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
        //
        // The snapshot is attributed to the account that *made* this edit, not
        // to the post's author. On a collaborative site those differ every time
        // an Editor touches somebody else's draft, and a history that credits
        // the owner for every change is worse than no history: it is confidently
        // wrong about who did what.
        if record_revision {
            post.record_revision_by(conn, &summary, editor_id).await?;
        }

        let before = post.clone();
        apply(&mut post);

        // Re-parenting is validated *here*, inside the transaction that writes
        // it, under the advisory lock over the whole hierarchy taken at the top.
        // Validating on a connection released before this one opens let two
        // editors each check an acyclic tree and both commit the edge that
        // closed a cycle.
        //
        // A caller that can re-parent must have declared it: without the lock
        // this check is back to racing another editor, so say so loudly rather
        // than validating against a tree that can move underneath the answer.
        if post.parent_id != before.parent_id
            && let Some(parent_id) = post.parent_id
        {
            if !may_touch_hierarchy {
                return Err(AutumnError::internal_server_error_msg(
                    "This edit moves the post in the page tree but did not take the \
                     hierarchy lock",
                ));
            }
            validate_parent(conn, Some(post_id), &post.post_type, parent_id).await?;
        }

        // The same invariants `PostHooks::before_update` enforces. This path
        // writes the fields with plain Diesel — which is what puts the edit and
        // its revision in one transaction, and is also what bypasses the hook —
        // so the rules have to be applied here explicitly rather than assumed.
        // Without them an Author could submit an empty title on a post that is
        // already `publish` and it would go live untitled: the state-machine
        // check only fires on a status change.
        crate::hooks::validate_post_update(&before, &mut post)?;

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
        // After the write, inside this transaction, so a refusal rolls the edit
        // back: the page's full path is only settled once the slug and the
        // parent are both stored, and this statement is what stores them.
        guard_page_path(conn, post_id).await?;
        // And the custom-type shape, which the insert allocator already checks.
        // Renaming an existing item is the other way onto a claimed path, and
        // guarding only creation left it open.
        if !BARE_PATH_TYPES.contains(&saved.post_type.as_str()) {
            guard_claimed_path(
                &[saved.post_type.clone(), saved.slug.clone()],
                &saved.post_type,
            )?;
        }
        Ok::<_, AutumnError>(saved)
    })
    .await
}

/// Move a post to a new status, enforcing the state machine and recording the
/// change as a revision — atomically.
///
/// This is the only path that changes `posts.status` outside the repository's
/// own update, and it is what the admin's Publish / Move to Trash / Restore
/// buttons call.
pub async fn transition_status(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    target: &str,
    editor_id: Option<i64>,
    actor: Option<&crate::models::User>,
) -> AutumnResult<Post> {
    let target = target.to_owned();
    conn.transaction(async move |conn| {
        // Before the row lock, not after it. A trash guarded by the hierarchy
        // lock but locking the post row first sat on the opposite order from
        // every create and re-parent, which take the hierarchy lock and then
        // key-share the parent row through the foreign key: each holds what the
        // other is waiting for, and PostgreSQL aborts one valid operation.
        //
        // The rule this establishes, and the one every path here follows:
        // **the page-hierarchy lock is taken before any post row lock.** It is
        // a single global mutex, so once it is held no other hierarchy mutation
        // is running to deadlock against, whatever row locks follow.
        //
        // Taken on `target` rather than on the state machine's answer, because
        // the answer needs the row — and `transition_status_to` only ever
        // returns the target or an error, so the two agree.
        // Which of the two hierarchy guards this transition needs is not
        // knowable before the row is read: trashing needs the child count, and
        // *leaving* the trash needs the ancestor check, which depends on the
        // status the row currently holds. So the type decides, read here
        // without a lock — `posts.post_type` is written once at insert and by
        // nothing afterwards, so a stale answer is not a possible answer.
        let hierarchical: bool = posts::table
            .find(post_id)
            .select(posts::post_type)
            .first::<String>(conn)
            .await
            .optional()?
            .is_some_and(|post_type| is_hierarchical_type(&post_type));
        if target == "trash" || hierarchical {
            lock_page_hierarchy(conn).await?;
        }

        let post: Post = posts::table
            .find(post_id)
            .select(Post::as_select())
            .for_update()
            .first(conn)
            .await
            .map_err(AutumnError::not_found)?;

        // Authorization is re-checked against the row *as locked*, not against
        // the status the handler read a moment earlier. A Contributor whose
        // draft an Editor publishes in between would otherwise still be
        // authorized by the stale `draft` — and go on to trash content they no
        // longer have the capability to touch.
        //
        // `None` is a path with no acting user: the scheduler and the seeder,
        // whose authority is the process's rather than a session's.
        if let Some(actor) = actor {
            let permitted = if target == "trash" {
                crate::capabilities::can_delete_post(
                    actor.role(),
                    actor.id,
                    post.author_id,
                    &post.status,
                )
            } else if matches!(target.as_str(), "publish" | "private" | "future") {
                actor
                    .role()
                    .can(crate::capabilities::Capability::PublishPosts)
                    && crate::capabilities::can_edit_post(
                        actor.role(),
                        actor.id,
                        post.author_id,
                        &post.status,
                    )
            } else {
                crate::capabilities::can_edit_post(
                    actor.role(),
                    actor.id,
                    post.author_id,
                    &post.status,
                )
            };
            if !permitted {
                return Err(AutumnError::forbidden_msg(
                    "You do not have permission to change this content's status",
                ));
            }
        }

        // Scheduling is checked against the locked row's date, not against the
        // one a handler read a moment earlier. The status endpoint carries no
        // date at all — it can only move a post to `future` when the row
        // already holds a future one — so an editor who clears or rewinds
        // `published_at` in between would otherwise have that request schedule
        // a post that either never publishes (`NULL` never matches the sweep's
        // `published_at <= now`) or publishes on the very next sweep.
        //
        // The editor's own paths write `published_at` earlier in this same
        // transaction, so the locked read sees what they are about to commit
        // rather than what was there before.
        require_future_publish_date(&target, post.published_at)?;

        // The macro-generated enforcing transition: an undeclared edge or a
        // failed guard is a 400 and nothing is written.
        let new_status = post.transition_status_to(&target)?;

        // The inverse of the guard below, and it was missing. Trashing a parent
        // is refused while it has a live child, so the only way to get a live
        // page under a trashed ancestor is to bring the child back first: trash
        // the child, trash the parent, restore the child. The child then
        // becomes a draft — and can be published — while `page_ancestry` still
        // puts the trashed parent's slug in its canonical URL and
        // `resolve_page_path` refuses that ancestor. Listings and the sitemap
        // then advertise a URL that always 404s, which is exactly the orphaned
        // permalink the trash guard exists to prevent, reached from the other
        // direction.
        //
        // Under the hierarchy lock taken above, so a concurrent trash of an
        // ancestor cannot commit between this check and this transition.
        if hierarchical
            && post.status == "trash"
            && new_status != "trash"
            && let Some(ancestor) = trashed_ancestor(conn, &post).await?
        {
            return Err(AutumnError::unprocessable_msg(format!(
                "\"{}\" is still in the trash, and this page's URL is built from it. \
                 Restore it first.",
                ancestor.title
            )));
        }

        // A trashed parent takes its children's URLs with it: `page_ancestry`
        // keeps putting its slug in their permalinks while `resolve_page_path`
        // refuses a trashed ancestor, so every published child 404s at its own
        // canonical URL and the sitemap keeps advertising it.
        //
        // Refused rather than silently re-parented: every automatic answer
        // changes the children's URLs too, and a CMS that moves published pages
        // without being asked is worse than one that says what is in the way.
        // The editor can trash or move the children first.
        if new_status == "trash" {
            // The hierarchy lock is already held, taken above the row lock and
            // held through this count and the status update — so a create or
            // re-parent cannot attach a child after the count and before the
            // commit, which would leave exactly the orphaned permalink this
            // guard exists to prevent.
            let children = live_child_count(conn, post_id).await?;
            if children > 0 {
                return Err(AutumnError::unprocessable_msg(format!(
                    "This page still has {children} live child {}. Trash or move them \
                     first — their URLs are built from this one.",
                    if children == 1 { "page" } else { "pages" }
                )));
            }
        }

        // Same rule the editor's save path follows: a type registered
        // `supports_revisions: false` gets no snapshot, from any path.
        // Publishing, trashing, restoring and importing all land here, so
        // leaving it out made the flag cosmetic — the link was hidden while
        // the rows accumulated anyway.
        //
        // Attributed to whoever acted, when there is one. `None` is the
        // scheduler and the seeder — paths with no user behind them, where the
        // owner is the only honest answer.
        if type_supports_revisions(&post.post_type) {
            let summary = format!("Status: {} → {new_status}", post.status);
            match editor_id {
                Some(actor) => post.record_revision_by(conn, &summary, actor).await?,
                None => post.record_revision(conn, &summary).await?,
            }
            // Pruned here as well as on the edit and restore paths. Without it
            // `REVISION_LIMIT` bounded only *edits*: a post cycled through
            // draft → publish → draft grew its history without limit, which is
            // exactly the unbounded `wp_posts` growth the cap exists to avoid.
            prune_revisions(conn, post_id).await?;
        }

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
    })
    .await
}

/// Restore a post to the content of one of its revisions.
///
/// The restore is itself an edit, so it appends a new revision rather than
/// rewinding the trail — the history of a document is append-only or it is not
/// a history.
pub async fn restore_revision(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    revision_id: i64,
    editor_id: Option<i64>,
    actor: Option<&crate::models::User>,
) -> AutumnResult<Post> {
    conn.transaction(async move |conn| {
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

        // Re-checked against the row *as locked*, for the same reason
        // `transition_status` does it: a restore is an edit, and the handler's
        // check ran against the status it read before this lock existed. A
        // Contributor who opens the revision screen on their own draft, and
        // whose draft an Editor publishes in the meantime, would otherwise
        // rewrite the body of a live post they can no longer edit.
        //
        // `None` is a path with no acting user — the importer and the seeder,
        // whose authority is the process's rather than a session's.
        if let Some(actor) = actor
            && !crate::capabilities::can_edit_post(
                actor.role(),
                actor.id,
                post.author_id,
                &post.status,
            )
        {
            return Err(AutumnError::forbidden_msg(
                "You do not have permission to edit this content",
            ));
        }

        let summary = format!("Restored revision #{}", revision.id);
        match editor_id {
            Some(actor) => post.record_revision_by(conn, &summary, actor).await?,
            None => post.record_revision(conn, &summary).await?,
        }

        // The restore replaces content only. Status is deliberately left
        // alone: restoring the text of a draft must not silently republish
        // it, and restoring a published post's older text must not
        // unpublish it.
        //
        // Which is exactly why the merged row has to be revalidated. A revision
        // captured while the post was an untitled draft is a legitimate
        // snapshot; restoring it onto the *live* post keeps the live status and
        // would write the empty title straight past the invariant every other
        // edit path now enforces.
        let mut merged = post.clone();
        merged.title.clone_from(&revision.title);
        merged.excerpt.clone_from(&revision.excerpt);
        merged.body.clone_from(&revision.body);
        crate::hooks::validate_post_update(&post, &mut merged)?;

        let saved: Post = diesel::update(posts::table.find(post_id))
            .set((
                posts::title.eq(&merged.title),
                posts::slug.eq(&merged.slug),
                posts::excerpt.eq(&merged.excerpt),
                posts::body.eq(&merged.body),
                posts::published_at.eq(merged.published_at),
                posts::lock_version.eq(post.lock_version + 1),
                posts::updated_at.eq(chrono::Utc::now().naive_utc()),
            ))
            .returning(Post::as_returning())
            .get_result(conn)
            .await?;

        prune_revisions(conn, post_id).await?;
        Ok::<_, AutumnError>(saved)
    })
    .await
}

/// Append the "Created" revision for a freshly-inserted post, so a post's
/// history starts at its creation rather than at its first *edit*.
///
/// A single statement, so it needs no transaction of its own.
pub async fn record_initial_revision(
    conn: &mut AsyncPgConnection,
    post: &Post,
) -> AutumnResult<()> {
    let conn = &mut *conn;
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
pub async fn set_post_terms(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    term_ids: Vec<i64>,
) -> AutumnResult<()> {
    conn.transaction(async move |conn| {
        // The post first, then its terms — one lock order across every path
        // that touches both. `transition_status` and the scheduled-publish
        // sweep each update the post row and *then* recount its terms, so a
        // taxonomy save that took a term lock first and then blocked on the
        // post (the insert below key-shares it through the foreign key) closes
        // the cycle and PostgreSQL aborts one of them. Taking it here also
        // costs nothing on the editor's path, which already holds this lock
        // from the stale-edit check.
        let _locked_post: Option<i64> = posts::table
            .find(post_id)
            .select(posts::id)
            .for_update()
            .first(conn)
            .await
            .optional()?;

        let previous: Vec<i64> = post_terms::table
            .filter(post_terms::post_id.eq(post_id))
            .select(post_terms::term_id)
            .load(conn)
            .await?;

        let mut wanted = term_ids.clone();
        wanted.sort_unstable();
        wanted.dedup();

        // Both sides of the change need recounting, so the union is computed
        // before the join rows move.
        let mut affected = previous;
        affected.extend(wanted.iter().copied());
        affected.sort_unstable();
        affected.dedup();

        // Locked *before* the writes below, not after. Inserting a `post_terms`
        // row makes PostgreSQL take a `FOR KEY SHARE` lock on the referenced
        // term to enforce the foreign key, and `recount_term` then asks the
        // same row for `FOR UPDATE`. Two editors filing different posts under
        // the same term therefore both hold a key-share lock and both try to
        // upgrade it: neither can, and PostgreSQL breaks the cycle by aborting
        // one editor's save as a deadlock. Taking the stronger lock first means
        // there is never an upgrade to deadlock over — the second editor simply
        // waits for the first.
        //
        // Ascending id order, and every fan-out over terms uses the same order,
        // so two transactions touching overlapping sets can never hold the
        // halves of each other's cycle.
        lock_terms(conn, &affected).await?;

        diesel::delete(post_terms::table.filter(post_terms::post_id.eq(post_id)))
            .execute(conn)
            .await?;

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

        recount_terms(conn, &affected).await?;
        Ok::<_, AutumnError>(())
    })
    .await
}

/// Take `FOR UPDATE` on the given term rows, in ascending id order.
///
/// Called before any write that will need to recount them, and *after* the post
/// row is locked — see [`set_post_terms`] for why the order and the timing both
/// matter. The ids are sorted here rather than trusted from the caller, because
/// a caller that forgets is exactly the bug this prevents.
async fn lock_terms(conn: &mut AsyncPgConnection, term_ids: &[i64]) -> AutumnResult<()> {
    let mut ordered = term_ids.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    for term_id in ordered {
        // A term deleted underneath us has no row to lock and nothing to
        // recount; `recount_term` reaches the same conclusion.
        let _locked: Option<i64> = terms::table
            .find(term_id)
            .select(terms::id)
            .for_update()
            .first(conn)
            .await
            .optional()?;
    }
    Ok(())
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

/// The live published-post count of each of `term_ids`, in one query.
///
/// `terms.post_count` is a *stored* number computed from a process-dependent
/// predicate: `public_type_slugs()` answers from the registry, so the counter
/// is right when it is written and stale the moment a deployment registers a
/// type differently — or restores content whose plugin is disabled, which
/// persists a zero that re-enabling the plugin never repairs, because a
/// registry change touches no row.
///
/// So the screens that *show* a count ask this instead. The stored counter
/// stays: `populated_terms` orders by it, where a stale heuristic costs
/// nothing, and it is still what `recount_term` maintains.
///
/// One grouped query rather than one per term — a term list is a page of rows
/// and this is the number beside each of them.
pub async fn term_post_counts(
    conn: &mut AsyncPgConnection,
    term_ids: &[i64],
) -> AutumnResult<std::collections::HashMap<i64, i64>> {
    if term_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    Ok(post_terms::table
        .inner_join(posts::table.on(posts::id.eq(post_terms::post_id)))
        .filter(post_terms::term_id.eq_any(term_ids))
        .filter(posts::status.eq("publish"))
        .filter(posts::post_type.eq_any(public_type_slugs()))
        .group_by(post_terms::term_id)
        .select((post_terms::term_id, diesel::dsl::count_star()))
        .load::<(i64, i64)>(conn)
        .await?
        .into_iter()
        .collect())
}

/// Rebuild one term's published-post count from ground truth.
pub async fn recount_term(conn: &mut AsyncPgConnection, term_id: i64) -> AutumnResult<i64> {
    // The term row is locked *before* the count is taken, so concurrent
    // recounts of the same term serialize around the snapshot rather than
    // around the write.
    //
    // Without it, two posts filed under one term publishing at once each count
    // a set that excludes the other's uncommitted transition, and both then
    // write the same too-low number — the second update overwriting the first
    // with a value that was already stale when it was computed. The term stays
    // under-counted in the admin, the widgets and the sitemap until some
    // unrelated mutation happens to recount it.
    let _locked: Option<i64> = terms::table
        .find(term_id)
        .select(terms::id)
        .for_update()
        .first(conn)
        .await
        .optional()?;

    // The same registered-type predicate the archive queries use. Counting rows
    // of a `public: false` type made the number disagree with the archive it
    // labels: the category widget advertised a count the visibility-aware term
    // query could not produce a single post for, and the sitemap published the
    // archive URL as populated when it renders empty.
    let count: i64 = post_terms::table
        .inner_join(posts::table.on(posts::id.eq(post_terms::post_id)))
        .filter(post_terms::term_id.eq(term_id))
        .filter(posts::status.eq("publish"))
        .filter(posts::post_type.eq_any(public_type_slugs()))
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
    recount_terms(conn, &term_ids).await
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
pub async fn moderate_comment(
    conn: &mut AsyncPgConnection,
    comment_id: i64,
    target: &str,
) -> AutumnResult<(Comment, bool)> {
    if !crate::hooks::COMMENT_STATUSES.contains(&target) {
        return Err(AutumnError::bad_request_msg(format!(
            "Unknown comment status `{target}`"
        )));
    }
    let target = target.to_owned();
    conn.transaction(async move |conn| {
        // The post row is locked *first*, before any comment is read, so every
        // moderation of a thread serializes on one thing.
        //
        // The ancestor check below reads rows this transaction does not
        // otherwise lock, so on its own it is a read that another moderator can
        // invalidate: approve a reply whose parent is approved, have the parent
        // spammed in between, and the reply commits approved under a hidden
        // parent — countable but unrenderable, permanently. Locking the
        // ancestors instead would serialize the two in *opposite* orders and
        // deadlock; the post is the one resource both paths already touch (the
        // recount takes it at the end), so taking it up front is what makes the
        // ordering total.
        let post_id: i64 = comments::table
            .find(comment_id)
            .select(comments::post_id)
            .first(conn)
            .await
            .map_err(AutumnError::not_found)?;
        posts::table
            .find(post_id)
            .select(posts::id)
            .for_update()
            .first::<i64>(conn)
            .await
            .map_err(AutumnError::not_found)?;

        let comment: Comment = comments::table
            .find(comment_id)
            .select(Comment::as_select())
            .for_update()
            .first(conn)
            .await
            .map_err(AutumnError::not_found)?;

        if comment.status == target {
            // Idempotent: a double-clicked Approve must not increment twice.
            // The `false` is what lets the caller keep its *actions* idempotent
            // too — firing `CommentApproved` again would have plugins enqueue a
            // second notification for a request that changed nothing.
            return Ok::<_, AutumnError>((comment, false));
        }

        // Approving a reply is only meaningful if its ancestors are approved
        // too. `assemble_thread` builds from the roots down, so a reply under a
        // hidden parent can never be attached — while `recount_post_comments`
        // would count it, leaving the post advertising a comment no reader can
        // reach. The hiding cascade below does not create this state (it moves
        // the approved descendants with the parent), but it does not prevent
        // it: a reply that was already *pending* when its parent was spammed
        // stays pending, and this is where a moderator would then approve it.
        //
        // Walked up rather than asserted about the immediate parent alone: an
        // ancestor two levels up can be the hidden one. Bounded by the same
        // depth cap the write path enforces.
        if target == "approved"
            && let Some(parent_id) = comment.parent_id
        {
            let mut cursor = Some(parent_id);
            for _ in 0..=MAX_COMMENT_DEPTH {
                let Some(current) = cursor else {
                    break;
                };
                let ancestor: Comment = comments::table
                    .find(current)
                    .select(Comment::as_select())
                    .first(conn)
                    .await
                    .map_err(AutumnError::not_found)?;
                if ancestor.status != "approved" {
                    return Err(AutumnError::unprocessable_msg(
                        "Approve the comment this replies to first — a reply under a \
                         hidden comment cannot be shown",
                    ));
                }
                cursor = ancestor.parent_id;
            }
        }

        let saved: Comment = diesel::update(comments::table.find(comment_id))
            .set(comments::status.eq(&target))
            .returning(Comment::as_returning())
            .get_result(conn)
            .await?;

        // Hiding a comment hides the thread under it. `assemble_thread` builds
        // from the roots down, so a reply whose parent is no longer approved
        // can never be attached or rendered — while it stayed `approved` and
        // stayed in `comment_count`. The post then advertised comments no
        // reader could see. Moving the approved descendants with the parent is
        // what makes counting and rendering ask the same question.
        //
        // The reverse does not cascade: approving a parent must not approve
        // replies nobody has moderated. They stay pending and appear when they
        // are approved on their own.
        if comment.status == "approved" && target != "approved" {
            let mut frontier = vec![comment_id];
            // The reply depth is capped on the write path, so this terminates
            // in at most that many rounds whatever the data looks like.
            for _ in 0..=MAX_COMMENT_DEPTH {
                let children: Vec<i64> = comments::table
                    .filter(comments::parent_id.eq_any(&frontier))
                    .filter(comments::status.eq("approved"))
                    .select(comments::id)
                    .load(conn)
                    .await?;
                if children.is_empty() {
                    break;
                }
                diesel::update(comments::table.filter(comments::id.eq_any(&children)))
                    .set(comments::status.eq(&target))
                    .execute(conn)
                    .await?;
                frontier = children;
            }
        }

        // Recomputed from ground truth rather than moved by a delta: the
        // cascade above changes an unknown number of rows, so no delta derived
        // from the one named in the request would be right. The helper takes
        // the post's row lock before counting, so two moderators working the
        // same post serialize around the snapshot.
        recount_post_comments(conn, comment.post_id).await?;
        Ok::<_, AutumnError>((saved, true))
    })
    .await
}

/// Insert a comment, bumping the approved counter when it lands approved.
/// `observed_password` is the post's password as the handler saw it when it
/// checked the session's unlock. Carried in so the locked re-read can tell that
/// the gate itself moved: an editor who password-protects a post — or changes
/// its password — between the handler's check and this transaction would
/// otherwise have a signed-in submission land *approved* on content the
/// commenter never unlocked. Status, type and `comment_status` were re-checked
/// here already; the password was the one part of "may this person see it" that
/// was not.
pub async fn create_comment(
    conn: &mut AsyncPgConnection,
    mut new: crate::models::NewComment,
    observed_password: &str,
) -> AutumnResult<Comment> {
    let observed_password = observed_password.to_owned();
    // The direct insert below is the reason this call exists: the row and the
    // post's approved-comment counter have to move in one transaction, which
    // the repository's generated `save` cannot do. But going around the
    // repository also goes around `CommentHooks::before_create`, so the *only*
    // server-side validation a comment ever gets is this line. Without it the
    // form's `required` and `maxlength` attributes are the whole defence, and
    // they are a browser convenience a crafted POST ignores: a signed-in
    // commenter's empty body would be inserted pre-approved and increment the
    // counter, a guest could post unattributed, and bodies would grow to the
    // global request-body limit rather than the declared 10,000-byte cap —
    // making the moderation queue and every thread render arbitrarily
    // expensive. Run before the transaction opens: nothing here touches the
    // database, and a rejected submission should not have taken a row lock.
    crate::hooks::validate_comment(&mut new)?;

    conn.transaction(async move |conn| {
        // The post is locked and re-read before anything is inserted. The
        // handler's eligibility check runs on a released connection, so an
        // editor closing comments, unpublishing or trashing the post in between
        // would otherwise have a signed-in submission land *approved* on
        // content that no longer accepts comments — and become publicly visible
        // the moment the post is restored.
        let post: Post = posts::table
            .find(new.post_id)
            .select(Post::as_select())
            .for_update()
            .first(conn)
            .await
            .map_err(AutumnError::not_found)?;
        if !post.is_public()
            || !is_public_type(&post.post_type)
            || !type_supports_comments(&post.post_type)
            || post.comment_status != "open"
            || post.password != observed_password
        {
            return Err(AutumnError::forbidden_msg(
                "Comments are closed on this post",
            ));
        }

        // A reply's parent has to still be approved, checked under a lock so a
        // concurrent moderation cannot slip between the check and the insert.
        //
        // The handler's own check only asks whether the parent is on this post.
        // That leaves the window a rendered page opens: a reply form drawn
        // before its parent was spammed still posts, and a signed-in reply
        // lands `approved` and bumps `comment_count` — while the thread query
        // omits its parent, so it can never be rendered. The count drifts up by
        // a comment nobody can see, permanently.
        if let Some(parent_id) = new.parent_id {
            let parent: Comment = comments::table
                .find(parent_id)
                .select(Comment::as_select())
                .for_update()
                .first(conn)
                .await
                .map_err(AutumnError::not_found)?;
            if parent.post_id != new.post_id {
                return Err(AutumnError::unprocessable_msg(
                    "That comment is not on this post",
                ));
            }
            if parent.status != "approved" {
                return Err(AutumnError::unprocessable_msg(
                    "The comment you are replying to is no longer visible",
                ));
            }
        }

        let approved = new.status == "approved";
        let post_id = new.post_id;
        let saved: Comment = diesel::insert_into(comments::table)
            .values(&new)
            .returning(Comment::as_returning())
            .get_result(conn)
            .await?;
        // Recomputed under the post's lock rather than incremented. A bare
        // `+ 1` is safe against another increment, but not against a
        // concurrent moderation recomputing the whole count from a snapshot
        // taken before this insert — that write would simply lose the new
        // comment. One definition of the counter, taken under one lock, is
        // what makes every combination of these paths agree.
        if approved {
            recount_post_comments(conn, post_id).await?;
        }
        Ok::<_, AutumnError>(saved)
    })
    .await
}

/// Rebuild a post's approved-comment counter from ground truth.
///
/// The repair for the drift an import, a seed or a hand-written `UPDATE` can
/// introduce — the same role `recompute_counter_caches` plays for the
/// framework's own counters.
pub async fn recount_comments(conn: &mut AsyncPgConnection, post_id: i64) -> AutumnResult<i64> {
    recount_post_comments(conn, post_id).await
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
pub fn to_comment_views(
    nodes: &[ThreadNode],
    settings: &crate::settings::Settings,
) -> Vec<autumn_web::widgets::CommentView> {
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
            timestamp: settings.format_datetime(node.comment.created_at),
            replies: to_comment_views(&node.replies, settings),
        })
        .collect()
}

/// A post's revision history, newest first.
pub async fn revisions_for(
    conn: &mut AsyncPgConnection,
    post_id: i64,
) -> AutumnResult<Vec<Revision>> {
    Ok(revisions::table
        .filter(revisions::post_id.eq(post_id))
        .order((revisions::created_at.desc(), revisions::id.desc()))
        .select(Revision::as_select())
        .load(&mut *conn)
        .await?)
}

/// Rebuild the published-post counts of the given terms.
///
/// The single fan-out: every path that recounts more than one term goes through
/// here, so the `FOR UPDATE` each `recount_term` takes is always acquired in
/// ascending id order. Four separate loops in id-of-arrival order is four
/// chances for two transactions with overlapping term sets to hold the halves
/// of each other's cycle and deadlock.
pub async fn recount_terms(conn: &mut AsyncPgConnection, term_ids: &[i64]) -> AutumnResult<()> {
    let mut ordered = term_ids.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    for term_id in ordered {
        recount_term(conn, term_id).await?;
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
    conn: &mut AsyncPgConnection,
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
            .first::<Option<i64>>(&mut *conn)
            .await
            .optional()?
            .flatten();
    }
    Ok(false)
}

// ── Accounts ────────────────────────────────────────────────────────────────

/// Advisory-lock key serializing every change to the page hierarchy.
///
/// Re-parenting is a read-then-write over the *tree*, not over one row, so no
/// row lock serializes it: two editors making A a child of B and B a child of A
/// each validate against a hierarchy that is still acyclic, and both commit —
/// closing a cycle that makes every page in it unreachable, since page
/// resolution walks down from a `NULL` parent.
///
/// Transaction-scoped, so it is released on commit or rollback, and taken only
/// by writes that actually move a post: an edit that leaves `parent_id` alone
/// never contends for it.
pub const PAGE_HIERARCHY_LOCK_KEY: i64 = 7_717_260_231_002;

/// Take the hierarchy lock for the rest of the caller's transaction.
///
/// The lock is what makes a parent check mean something: it is a read over the
/// *tree*, so no row lock serializes it, and a validation that runs on a
/// released connection can be invalidated before the write it was guarding.
/// Exposed so the creation path can hold it across validation *and* insertion,
/// the way `update_post_with_revision` already does for re-parenting.
pub async fn lock_page_hierarchy(conn: &mut AsyncPgConnection) -> AutumnResult<()> {
    diesel::sql_query(format!(
        "SELECT pg_advisory_xact_lock({PAGE_HIERARCHY_LOCK_KEY})"
    ))
    .execute(conn)
    .await?;
    Ok(())
}

/// How many children a post has that are not in the trash.
///
/// Only the immediate children need counting: a grandchild's ancestry runs
/// through one of them, so if every child is trashed no live descendant is
/// reachable through this post at all.
///
/// Trashing a parent leaves its children's `parent_id` pointing at it, so
/// `page_ancestry` keeps generating permalinks that contain the trashed slug
/// while `resolve_page_path` refuses trashed ancestors — every published child
/// starts 404ing at its own canonical URL, and the sitemap advertises those
/// dead URLs.
/// Whether a term's archive is reachable at all.
///
/// A taxonomy lives in a process-global registry a plugin can stop registering,
/// but its terms and their `post_terms` rows stay in the database. `term_url`
/// falls back to the stored taxonomy slug, and `permalinks::resolve` recognises
/// a term archive only by iterating the *currently registered* taxonomies — so
/// a link built for an orphaned term either 404s or, worse, resolves as
/// unrelated page content. Anywhere that renders links for terms it did not
/// choose has to ask this first.
#[must_use]
pub fn is_routable_term(term: &Term) -> bool {
    crate::content_types::find_taxonomy(&term.taxonomy).is_some()
}

/// Whether a registered post type nests.
#[must_use]
pub fn is_hierarchical_type(post_type: &str) -> bool {
    crate::content_types::find_post_type(post_type)
        .is_some_and(|registered| registered.hierarchical)
}

/// The nearest trashed ancestor of a post, if it has one.
///
/// Walks up rather than down, and bounded by `MAX_PAGE_DEPTH` with a seen-set,
/// for the same reason `page_ancestry` is: the chain is bounded by the tree's
/// depth, and a cycle from a direct write must terminate the walk rather than
/// spin.
async fn trashed_ancestor(conn: &mut AsyncPgConnection, post: &Post) -> AutumnResult<Option<Post>> {
    let mut cursor = post.parent_id;
    let mut seen = vec![post.id];
    let mut steps = 0_usize;
    while let Some(parent_id) = cursor {
        steps += 1;
        if steps > MAX_PAGE_DEPTH || seen.contains(&parent_id) {
            break;
        }
        seen.push(parent_id);
        let parent: Option<Post> = posts::table
            .find(parent_id)
            .select(Post::as_select())
            .first(&mut *conn)
            .await
            .optional()?;
        let Some(parent) = parent else {
            break;
        };
        if parent.status == "trash" {
            return Ok(Some(parent));
        }
        cursor = parent.parent_id;
    }
    Ok(None)
}

pub async fn live_child_count(conn: &mut AsyncPgConnection, post_id: i64) -> AutumnResult<i64> {
    Ok(posts::table
        .filter(posts::parent_id.eq(post_id))
        .filter(posts::status.ne("trash"))
        .count()
        .get_result(conn)
        .await?)
}

/// How many live posts would be re-rooted by deleting `author_id`'s content.
///
/// A child whose parent is deleted has its `parent_id` set to `NULL` by the
/// foreign key, so it moves to the top level and its canonical URL changes from
/// `/parent/child` to `/child` — every inbound link and every sitemap entry for
/// it breaks, silently. Children the same author owns are deleted alongside
/// their parent and so are not at risk; it is the ones somebody *else* owns
/// that survive the cascade and get re-rooted.
///
/// The same argument `transition_status` makes about trashing a parent, applied
/// to the other path that can remove one.
pub async fn orphaned_by_deleting_author(
    conn: &mut AsyncPgConnection,
    author_id: i64,
) -> AutumnResult<i64> {
    // Raw SQL because this is a self-join on `posts`, which the query builder
    // needs a table alias for; the shape is simple enough that the alias would
    // cost more than it explains.
    use diesel::sql_types::BigInt;

    #[derive(diesel::QueryableByName)]
    struct Total {
        #[diesel(sql_type = BigInt)]
        count: i64,
    }

    Ok(diesel::sql_query(
        "SELECT COUNT(*) AS count FROM posts child \
         JOIN posts parent ON parent.id = child.parent_id \
         WHERE parent.author_id = $1 AND child.author_id <> $1 AND child.status <> 'trash'",
    )
    .bind::<BigInt, _>(author_id)
    .get_result::<Total>(conn)
    .await?
    .count)
}

/// Persist a whole settings form in one transaction.
///
/// Option-by-option commits let two administrators saving at once interleave
/// into a configuration neither of them submitted, and a failure part-way
/// through left the form half-applied while reporting an error. One
/// transaction, and `ON CONFLICT (name)` rather than read-then-write, which
/// also removes the race inside each individual upsert.
pub async fn save_settings(
    conn: &mut AsyncPgConnection,
    rows: Vec<(&'static str, String)>,
) -> AutumnResult<()> {
    conn.transaction(async move |conn| {
        for (name, value) in rows {
            diesel::sql_query(
                "INSERT INTO options (name, value, autoload, updated_at) \
                 VALUES ($1, $2, TRUE, NOW()) \
                 ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value, updated_at = NOW()",
            )
            .bind::<diesel::sql_types::Text, _>(name)
            .bind::<diesel::sql_types::Text, _>(value)
            .execute(conn)
            .await?;
        }
        Ok::<_, AutumnError>(())
    })
    .await
}

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

/// Create an account on an administrator's behalf, re-authorizing the actor
/// against their *current* row.
///
/// The handler's capability check runs at the start of a request that then
/// spends hundreds of milliseconds hashing a password — deliberately, since the
/// cost is the point of bcrypt. That is a wide window, and the account being
/// created can carry any role: an administrator demoted or deleted while their
/// request was hashing could still land a fresh `administrator` account and
/// keep privileged access through it. An authorization decision that old is not
/// a decision about the account doing the writing.
///
/// Serialized on [`ADMIN_SET_LOCK_KEY`], the same lock every other change to
/// the administrator set takes, so a demotion cannot commit between this
/// re-read and this insert.
pub async fn create_user_as(
    conn: &mut AsyncPgConnection,
    actor_id: i64,
    new: crate::models::NewUser,
) -> AutumnResult<User> {
    let mut new = new;
    // The model's declared rules, which this direct insert never runs.
    crate::hooks::normalize_new_user(&mut new)?;
    conn.transaction(async move |conn| {
        diesel::sql_query(format!(
            "SELECT pg_advisory_xact_lock({ADMIN_SET_LOCK_KEY})"
        ))
        .execute(conn)
        .await?;

        // Re-read rather than trust the session's copy: the row is the truth
        // about what this account may do *now*. A deleted actor has no
        // authority at all, which is why a missing row is a refusal rather than
        // a fall-through.
        let actor: Option<User> = users::table
            .find(actor_id)
            .select(User::as_select())
            .first(conn)
            .await
            .optional()?;
        let permitted =
            actor.is_some_and(|actor| actor.role().can(crate::capabilities::Capability::EditUsers));
        if !permitted {
            return Err(AutumnError::forbidden_msg(
                "Your account can no longer manage users",
            ));
        }

        Ok::<_, AutumnError>(
            diesel::insert_into(users::table)
                .values(&new)
                .returning(User::as_returning())
                .get_result(conn)
                .await?,
        )
    })
    .await
}

/// Create an account, electing the first one created as the site owner —
/// atomically.
///
/// The election and the insert share one transaction under
/// [`ADMIN_SET_LOCK_KEY`]. Without it, two signups can both observe an empty
/// table and both be persisted as administrators: the window is not
/// theoretical, because password hashing (bcrypt, deliberately slow) happens
/// between the check and the insert.
pub async fn register_user(
    conn: &mut AsyncPgConnection,
    new: crate::models::NewUser,
) -> AutumnResult<User> {
    let mut new = new;
    crate::hooks::normalize_new_user(&mut new)?;
    conn.transaction(async move |conn| {
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
    conn: &mut AsyncPgConnection,
    actor_id: i64,
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
    conn.transaction(async move |conn| {
        diesel::sql_query(format!(
            "SELECT pg_advisory_xact_lock({ADMIN_SET_LOCK_KEY})"
        ))
        .execute(conn)
        .await?;

        // The actor, re-read under the same lock as the target. The handler's
        // capability check ran on a released connection, and this guard used to
        // reload only the *target* — so an administrator demoted or deleted
        // while their request was in flight could still promote an account they
        // control and keep privileged access through it. Every write to the
        // administrator set now decides on the actor's current row, which is
        // what `create_user_as` does and what this guard should always have
        // done. A deleted actor has no authority at all, so a missing row is a
        // refusal rather than a fall-through.
        let actor: Option<User> = users::table
            .find(actor_id)
            .select(User::as_select())
            .first(conn)
            .await
            .optional()?;
        let permitted =
            actor.is_some_and(|actor| actor.role().can(crate::capabilities::Capability::EditUsers));
        if !permitted {
            return Err(AutumnError::forbidden_msg(
                "Your account can no longer manage users",
            ));
        }

        let target: User = users::table
            .find(target_id)
            .select(User::as_select())
            .first(conn)
            .await
            .map_err(AutumnError::not_found)?;

        let administrator = crate::capabilities::Role::Administrator;
        let losing_an_administrator = target.role() == administrator && new_role != administrator;
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
    })
    .await
}

/// Change an account's role and profile fields, guarding the last
/// administrator.
/// The profile fields an administrator may change on another account.
///
/// A struct rather than five more parameters, for the reason `EditContext` is
/// one: at the call site four adjacent `String`s are four chances to swap two
/// of them silently.
pub struct UserEdit {
    pub role: crate::capabilities::Role,
    pub email: String,
    pub display_name: String,
    pub bio: String,
    pub website: String,
}

pub async fn update_user(
    conn: &mut AsyncPgConnection,
    actor_id: i64,
    target_id: i64,
    edit: UserEdit,
) -> AutumnResult<()> {
    let UserEdit {
        role,
        email,
        display_name,
        bio,
        website,
    } = edit;
    // The same rule the registration path applies, for the same reason: this is
    // a direct Diesel update, so the model's `#[validate(email)]` never runs.
    // Fixing only the create path left an administrator able to store `user@`
    // on an existing account.
    let email = email.trim().to_lowercase();
    if !autumn_web::reexports::validator::ValidateEmail::validate_email(&email) {
        return Err(AutumnError::unprocessable_msg(
            "That email address is not valid",
        ));
    }
    if email.len() > crate::hooks::MAX_EMAIL_BYTES {
        return Err(AutumnError::unprocessable_msg(format!(
            "Email must be at most {} characters",
            crate::hooks::MAX_EMAIL_BYTES
        )));
    }

    with_administrator_guard(conn, actor_id, target_id, role, move |conn| {
        async move {
            diesel::update(users::table.find(target_id))
                .set((
                    users::role.eq(role.slug()),
                    users::email.eq(&email),
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
pub async fn delete_user(
    conn: &mut AsyncPgConnection,
    actor_id: i64,
    target_id: i64,
) -> AutumnResult<()> {
    // Deleting is a demotion to "no role at all", so it takes the same guard.
    // The recounts run inside the same transaction as the cascade: doing them
    // afterwards means a transient failure leaves the account and its posts
    // permanently gone with the counts stale, and a retry finds no user to
    // delete, so nothing ever repairs them.
    with_administrator_guard(
        conn,
        actor_id,
        target_id,
        crate::capabilities::Role::Subscriber,
        move |conn| {
            async move {
                // The author's posts are locked before their filings are read,
                // and both happen inside this transaction. Reading the term ids
                // outside it left a window: an editor filing one of these posts
                // under a new term in between had that `post_terms` row removed
                // by the cascade while its term was absent from the list to
                // rebuild, so the term kept a count of a post that no longer
                // existed — and kept appearing in widgets and the sitemap on
                // the strength of it.
                //
                // `FOR UPDATE` on the posts is what closes it: inserting a
                // `post_terms` row takes `FOR KEY SHARE` on the post it
                // references, and that conflicts, so a concurrent filing waits
                // for this transaction rather than racing it.
                // The hierarchy lock, taken *before* the check and held to
                // commit, exactly as `set_post_parent` and `transition_status`
                // take it. Being inside one transaction is not enough on its
                // own: the count below reads rows this transaction does not
                // lock, so an editor filing their live page under one of this
                // author's pages in between would commit first and this
                // deletion would then re-root it, having asked the question
                // before the answer changed. Re-parenting takes the same lock,
                // so it waits.
                lock_page_hierarchy(conn).await?;

                // Refused before anything is deleted: another author's live
                // page filed under one of these would be silently re-rooted by
                // `parent_id ... ON DELETE SET NULL`, changing its canonical
                // URL and breaking every link to it. The explicit trash path
                // already refuses for this reason; deleting the author was the
                // way around it.
                let orphaned = orphaned_by_deleting_author(conn, target_id).await?;
                if orphaned > 0 {
                    return Err(AutumnError::unprocessable_msg(format!(
                        "{} other {} filed under this account's pages and would lose \
                         their place in the hierarchy. Re-file or trash them first.",
                        orphaned,
                        if orphaned == 1 {
                            "page is"
                        } else {
                            "pages are"
                        }
                    )));
                }

                let post_ids: Vec<i64> = posts::table
                    .filter(posts::author_id.eq(target_id))
                    .select(posts::id)
                    .for_update()
                    .load(conn)
                    .await?;
                let affected_terms: Vec<i64> = post_terms::table
                    .filter(post_terms::post_id.eq_any(&post_ids))
                    .select(post_terms::term_id)
                    .distinct()
                    .load(conn)
                    .await?;

                diesel::delete(users::table.find(target_id))
                    .execute(conn)
                    .await?;
                recount_terms(conn, &affected_terms).await?;
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

/// Whether a registered post type takes comments at all.
///
/// Separate from the row's `comment_status`: the flag is the *type's* answer
/// and the column is the *item's*. A `page` registers `supports_comments:
/// false`, so a row that somehow carries `comment_status = "open"` — a crafted
/// editor submission, an import, a direct write — must still refuse comments.
#[must_use]
pub fn type_supports_comments(post_type: &str) -> bool {
    crate::content_types::find_post_type(post_type)
        .is_some_and(|registered| registered.supports_comments)
}

/// Whether a registered post type keeps revision history.
///
/// `supports_revisions: false` means no snapshots are written and none are
/// reachable — not a flag the editor hides the link for while the storage grows
/// anyway.
#[must_use]
pub fn type_supports_revisions(post_type: &str) -> bool {
    crate::content_types::find_post_type(post_type)
        .is_some_and(|registered| registered.supports_revisions)
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
/// How many published, publicly-routable posts an account has.
///
/// The question `/author/<username>` has to ask before it renders anything: an
/// account with no public content has no archive, and a 200 page for one
/// discloses that the account exists.
pub async fn published_post_count_by_author(
    conn: &mut AsyncPgConnection,
    author_id: i64,
) -> AutumnResult<i64> {
    Ok(posts::table
        .filter(posts::author_id.eq(author_id))
        .filter(posts::status.eq("publish"))
        .filter(posts::post_type.eq_any(public_type_slugs()))
        .count()
        .get_result(conn)
        .await?)
}

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
pub async fn published_authors_page(
    conn: &mut AsyncPgConnection,
    offset: i64,
    limit: i64,
) -> AutumnResult<Vec<User>> {
    // One query, with the distinct, the order and the bound all inside it.
    //
    // Loading the distinct author ids first and then bounding the *users* query
    // still made an unauthenticated `?per_page=1` cost one row per author on
    // the wire and in memory — a projection of an indexed column rather than a
    // row load, but still proportional to the whole author population rather
    // than to the request. `EXISTS` lets Postgres stop at the page.
    Ok(users::table
        .filter(diesel::dsl::exists(
            posts::table
                .filter(posts::author_id.eq(users::id))
                .filter(posts::status.eq("publish"))
                .filter(posts::post_type.eq_any(public_type_slugs())),
        ))
        .order((users::username.asc(), users::id.asc()))
        .offset(offset.max(0))
        .limit(limit.max(0))
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
///
/// `category` and `tag` are deliberately absent: they are taxonomy rewrite
/// bases, and `segment_claim` reads those from the registry. Listing them
/// statically as well made the built-in taxonomies unable to re-register
/// themselves (the static entry outranked the "not in conflict with yourself"
/// rule), and covered only the two built-ins — a custom taxonomy's base was
/// never reserved against a content slug at all.
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
    // Mounted by the *framework*, not by this app, which is exactly why they
    // were missing: nothing in `all_routes()` mentions them, so the list built
    // from what the app declares had no reason to include them. They are on by
    // default (`health.enabled`), and a literal route beats the front
    // controller's wildcard — so a page titled "Health" took the bare slug
    // `health`, advertised `/health` as its permalink, and was unreachable at
    // it forever.
    //
    // Spelled without the leading slash to match the rest of this list, which
    // holds first path segments. An operator who renames them in `autumn.toml`
    // narrows the reservation rather than widening it, which is the safe
    // direction: a reserved slug nothing serves costs one suffixed URL.
    "health",
    "live",
    "ready",
    "startup",
];

/// The probe paths *this deployment* mounts, once something has looked.
///
/// Each entry is the path split into segments. The four names in
/// `RESERVED_PATHS` are the framework's defaults; all four are configurable,
/// and a configured path need not be a single segment — which is why this
/// stores whole paths rather than bare slugs.
static CONFIGURED_PROBE_PATHS: std::sync::OnceLock<Vec<Vec<String>>> = std::sync::OnceLock::new();

/// Record the probe paths this deployment mounts, from its own configuration.
///
/// Called from the `Repos` extractor, which is the one place every slug-writing
/// path passes through and the earliest place the running configuration is in
/// hand — `bootstrap()` runs before the app is built and has none. Idempotent
/// and read-mostly: after the first request this is a `OnceLock` hit.
///
/// It only ever *adds* to the defaults, so a slug is never un-reserved by a
/// configuration this has not seen yet.
pub fn observe_probe_paths(config: &autumn_web::config::AutumnConfig) {
    if CONFIGURED_PROBE_PATHS.get().is_some() {
        return;
    }
    let _ = CONFIGURED_PROBE_PATHS.set(probe_paths(&config.health));
}

/// The paths a health configuration mounts, split into segments.
///
/// Pure, and separate from the `OnceLock` above so it can be tested: the store
/// is process-global by design — one process runs one configuration — which
/// makes the seeding itself awkward to exercise from a suite that shares a
/// process.
#[must_use]
pub fn probe_paths(health: &autumn_web::config::HealthConfig) -> Vec<Vec<String>> {
    if !health.enabled {
        // Explicitly disabled, so nothing is mounted and nothing is claimed.
        // The defaults in `RESERVED_PATHS` still stand — a reserved slug
        // nothing serves costs one suffixed URL, which is the cheap direction
        // to be wrong in.
        return Vec::new();
    }
    [
        &health.path,
        &health.live_path,
        &health.ready_path,
        &health.startup_path,
    ]
    .iter()
    .filter_map(|path| {
        let segments: Vec<String> = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(std::string::ToString::to_string)
            .collect();
        (!segments.is_empty()).then_some(segments)
    })
    .collect()
}

/// Whether a whole page path is one this deployment's framework routes claim.
///
/// A page is addressed by its full ancestry, so a *nested* probe path can be
/// shadowed too: `health.path = "/internal/probe"` is reachable as a root page
/// `internal` with a child `probe`, whose canonical `/internal/probe` the
/// framework's literal route wins. An earlier version of this discarded
/// multi-segment paths on the grounds that only bare slugs can collide, which
/// is true for posts and false for pages.
#[must_use]
pub fn is_claimed_page_path(segments: &[String]) -> bool {
    CONFIGURED_PROBE_PATHS
        .get()
        .is_some_and(|paths| paths.iter().any(|path| path.as_slice() == segments))
}

/// Whether a slug would be shadowed by one of the application's own routes, or
/// by a framework route this deployment mounts.
#[must_use]
pub fn is_reserved_path(slug: &str) -> bool {
    RESERVED_PATHS.contains(&slug)
        || CONFIGURED_PROBE_PATHS
            .get()
            .is_some_and(|paths| paths.iter().any(|path| path.len() == 1 && path[0] == slug))
}

/// A scheduled post needs a date that is actually in the future.
///
/// "Has a date" was not enough: a published post moved back to draft keeps its
/// original `published_at`, the editor pre-fills that past timestamp, and
/// choosing "Scheduled" without touching the field produced a row that was
/// already due — the next sweep republished it within the minute instead of
/// scheduling it. Nothing about that reads as scheduling to the person who did
/// it. A missing date is the other half: the sweep selects `published_at <=
/// now`, which `NULL` never matches, so the post is stuck `future` forever.
///
/// Lives here rather than in the editor because [`transition_status`] applies
/// it to the row *as locked* — a handler's copy is a read that can go stale
/// between the check and the write.
pub fn require_future_publish_date(
    status: &str,
    scheduled_for: Option<chrono::NaiveDateTime>,
) -> AutumnResult<()> {
    if status != "future" {
        return Ok(());
    }
    match scheduled_for {
        Some(when) if when > chrono::Utc::now().naive_utc() => Ok(()),
        Some(_) => Err(AutumnError::unprocessable_msg(
            "A scheduled post needs a publish date in the future",
        )),
        None => Err(AutumnError::unprocessable_msg(
            "Pick a publish date for a scheduled post",
        )),
    }
}

/// Refuse a creation whose deferred transition would fail after the insert.
///
/// `private` and `future` are only reachable by transitioning a draft, so a
/// creation asking for either saves a draft first and moves it afterwards. When
/// the guard on that edge rejects, the draft is already committed — along with
/// its initial revision and term assignments — so the request reports an error
/// and leaves behind a row the caller never asked for, with each retry
/// consuming another suffixed slug.
///
/// The guard depends only on the content being submitted, so asking first costs
/// one comparison and makes the failure clean. Shared by the admin editor and
/// the REST API, which had the same shape and were fixed one at a time.
pub fn guard_deferred_transition(target_status: &str, title: &str) -> AutumnResult<()> {
    if matches!(target_status, "private" | "future") && title.trim().is_empty() {
        return Err(AutumnError::unprocessable_msg(format!(
            "A {target_status} post must have a title"
        )));
    }
    Ok(())
}

/// The post types addressed at the bare URL path rather than under a prefix of
/// their own. Only these two compete for a bare slug, and only these two can be
/// shadowed by a segment something else claims.
pub const BARE_PATH_TYPES: &[&str] = &["post", "page"];

/// One registration, identified by which registry it lives in as well as by its
/// slug.
///
/// The two registries have separate namespaces — a post type and a taxonomy may
/// both be called `product` as far as either registry is concerned — but they
/// share the *URL* namespace, which is what `segment_claim` arbitrates. So an
/// exclusion has to name both, or one registry's entry excuses the other's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration<'a> {
    PostType(&'a str),
    Taxonomy(&'a str),
}

/// What already owns a first URL segment, if anything does.
///
/// The one answer to "is this segment free?", because the question kept being
/// asked in three places that each knew about a different subset: registration
/// checked reserved paths and taxonomy bases, slug allocation checked reserved
/// paths and year archives, and taxonomy registration checked reserved paths
/// alone. Every combination the incomplete versions missed produced the same
/// outcome — content that saves, appears in the admin, and is unreachable at
/// its own canonical URL, permanently.
///
/// The order mirrors `permalinks::resolve`: whatever it tries first is what a
/// URL actually means.
///
/// `exclude` names the registration being added or replaced, whose own segments
/// must not count against it — as a *kind and slug*, not a bare slug. A bare
/// slug was matched in both registries, so registering a taxonomy `product`
/// silently excused an existing `product` post type from the check and accepted
/// the collision it was meant to catch.
#[must_use]
pub fn segment_claim(segment: &str, exclude: Option<Registration<'_>>) -> Option<String> {
    if is_reserved_path(segment) {
        return Some(format!("the application's own `/{segment}` route"));
    }
    if reads_as_date_archive(segment) {
        return Some("a year archive".to_owned());
    }
    for taxonomy in crate::content_types::all_taxonomies() {
        if taxonomy.rewrite_base == segment
            && exclude != Some(Registration::Taxonomy(taxonomy.slug))
        {
            return Some(format!("the `{}` taxonomy's term archives", taxonomy.slug));
        }
    }
    for registered in crate::content_types::all_post_types() {
        // `post` and `page` are skipped exactly as `permalinks::resolve` skips
        // them: they own the bare paths rather than a prefix, so neither their
        // slug nor their nominal `archive_base` claims a segment. `post` ships
        // with `has_archive: true` and `archive_base: "post"`, so checking it
        // here would reserve `/post` against content the resolver would in fact
        // serve.
        if !registered.public
            || BARE_PATH_TYPES.contains(&registered.slug)
            || exclude == Some(Registration::PostType(registered.slug))
        {
            continue;
        }
        if registered.slug == segment {
            return Some(format!("the `{}` post type's items", registered.slug));
        }
        if registered.has_archive && registered.archive_base == segment {
            return Some(format!("the `{}` post type's archive", registered.slug));
        }
    }
    None
}

/// A slug that is free among the rows minting the same shape of URL.
///
/// What a slug has to be unique against depends on the URL the row actually
/// mints, which is not the same question as its post type:
///
/// * A **nested page** is addressed by its full ancestry, so it competes only
///   with its siblings. `/about/team` and `/company/team` are different URLs
///   and both are legitimate — WordPress allows exactly this, and
///   `resolve_page_path` disambiguates by `parent_id`. Treating every page as
///   globally unique renamed the second one to `team-2` for no reason.
/// * A **post or a top-level page** mints a bare path (`/about`), where only
///   one row can be served — so those two types compete with each other,
///   appending `-2`, `-3`, … as WordPress does.
/// * A **custom type** is addressed under its own prefix (`/product/widget`)
///   and competes only with itself, but it still has to compete, because
///   `idx_posts_type_slug` requires uniqueness within a type.
pub async fn ensure_unique_slug(
    conn: &mut AsyncPgConnection,
    post_type: &str,
    desired: &str,
    parent_id: Option<i64>,
    exclude_id: Option<i64>,
) -> AutumnResult<String> {
    // A page with a parent is not in the bare-path namespace at all; it is in
    // its parent's.
    let nested_page = post_type == "page" && parent_id.is_some();
    let competing_types: Vec<&str> = if nested_page {
        vec!["page"]
    } else if BARE_PATH_TYPES.contains(&post_type) {
        BARE_PATH_TYPES.to_vec()
    } else {
        vec![post_type]
    };

    // A bare-path slug that something else already owns is reserved, because
    // the content given it would be unreachable at its own canonical URL.
    // `segment_claim` is the whole list — literal routes, year archives,
    // taxonomy rewrite bases, custom type slugs and custom archive bases —
    // rather than the two shapes this used to know about, which left a post
    // slugged `product` shadowed by a custom type's archive.
    //
    // Reserving is what keeps both features working. Falling back to content
    // when the archive or route "has nothing" would instead make `/2026` or
    // `/search` mean different things depending on what happens to exist.
    //
    // A nested page is exempt: nothing claims `/about/team` by claiming
    // `/team`, and reserving on its account would rename a page for a collision
    // that cannot happen. `guard_page_path` covers the nested paths that *are*
    // claimed.
    let shadowed_by_a_route = !nested_page
        && BARE_PATH_TYPES.contains(&post_type)
        && segment_claim(desired, None).is_some();
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
        // Siblings only, for a nested page — and for a top-level page or a
        // post, the bare-path namespace, which nested pages are not in.
        query = match parent_id {
            Some(parent) if nested_page => query.filter(posts::parent_id.eq(parent)),
            _ if BARE_PATH_TYPES.contains(&post_type) => {
                query.filter(posts::post_type.eq("post").or(posts::parent_id.is_null()))
            }
            _ => query,
        };
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
pub async fn depth_under(
    conn: &mut AsyncPgConnection,
    candidate_parent_id: i64,
) -> AutumnResult<usize> {
    let mut depth = 1usize;
    let mut cursor = Some(candidate_parent_id);
    while let Some(current) = cursor {
        if depth > MAX_PAGE_DEPTH + 2 {
            break;
        }
        cursor = posts::table
            .find(current)
            .select(posts::parent_id)
            .first::<Option<i64>>(&mut *conn)
            .await
            .optional()?
            .flatten();
        if cursor.is_some() {
            depth += 1;
        }
    }
    Ok(depth)
}

/// How many levels of descendants a page has below it.
///
/// `0` for a leaf. Walked level by level with one query per level rather than
/// row by row, and bounded by the same limit the resolver has, so a hierarchy
/// that a direct write left deeper than the bound terminates instead of
/// spinning.
pub async fn subtree_height(conn: &mut AsyncPgConnection, post_id: i64) -> AutumnResult<usize> {
    let mut height = 0usize;
    let mut level = vec![post_id];
    while !level.is_empty() {
        if height > MAX_PAGE_DEPTH + 2 {
            break;
        }
        let children: Vec<i64> = posts::table
            .filter(posts::parent_id.eq_any(&level))
            .select(posts::id)
            .load(&mut *conn)
            .await?;
        if children.is_empty() {
            break;
        }
        height += 1;
        level = children;
    }
    Ok(height)
}

/// One post by id, on a caller-supplied connection.
///
/// The repository has `find_by_id`, but it takes a connection of its own from
/// the pool; a caller already inside a `with_conn` needs this one.
pub async fn post_by_id(conn: &mut AsyncPgConnection, post_id: i64) -> AutumnResult<Option<Post>> {
    Ok(posts::table
        .find(post_id)
        .select(Post::as_select())
        .first(conn)
        .await
        .optional()?)
}

/// Refuse a page whose full path a framework route already serves.
///
/// Checked *after* the write, inside the caller's transaction, so it rolls the
/// change back — the same shape the hierarchy re-validation uses. The path is
/// only known once the slug is allocated and the parent is set, and those
/// happen in the same statement, so there is no earlier point that has both.
///
/// A bare slug is caught long before this by `ensure_unique_slug`; what this
/// adds is the nested case, which only a page can reach.
pub async fn guard_page_path(conn: &mut AsyncPgConnection, post_id: i64) -> AutumnResult<()> {
    for path in page_paths_under(conn, post_id).await? {
        guard_claimed_path(&path, "page")?;
    }
    Ok(())
}

/// Every canonical page path a hierarchy edit at `post_id` settles: the edited
/// page's own, and one for each of its descendants.
///
/// Separate from the guard because it is the half that was missing and the half
/// worth asserting on its own. A page's path is built from its ancestors, so
/// renaming or re-parenting one rewrites the canonical URL of everything under
/// it — and checking only the edited row let a rename hand a *child's* path to
/// the health probe while the parent's own path was fine. With
/// `health.path = "/section/status"`, renaming `/old` to `section` claims
/// nothing for the parent and gives `/section/status` to the probe, which then
/// shadows a child the listings and the sitemap keep advertising.
///
/// Bounded by `descendant_ids`, which is bounded by `MAX_PAGE_DEPTH`.
pub async fn page_paths_under(
    conn: &mut AsyncPgConnection,
    post_id: i64,
) -> AutumnResult<Vec<Vec<String>>> {
    let mut ids = vec![post_id];
    ids.extend(descendant_ids(conn, post_id).await?);
    let mut paths = Vec::new();
    for id in ids {
        if let Some(path) = page_path_of(conn, id).await? {
            paths.push(path);
        }
    }
    Ok(paths)
}

/// One nested page's canonical path, walking up to its root.
///
/// `None` for anything that is not a nested page: a top-level page and a
/// non-page type are addressed by other rules, checked elsewhere.
async fn page_path_of(
    conn: &mut AsyncPgConnection,
    post_id: i64,
) -> AutumnResult<Option<Vec<String>>> {
    let Some(post) = post_by_id(conn, post_id).await? else {
        return Ok(None);
    };
    if post.post_type != "page" || post.parent_id.is_none() {
        return Ok(None);
    }

    let mut segments = vec![post.slug.clone()];
    let mut cursor = post.parent_id;
    let mut seen = vec![post.id];
    while let Some(parent_id) = cursor {
        if segments.len() > MAX_PAGE_DEPTH || seen.contains(&parent_id) {
            break;
        }
        seen.push(parent_id);
        let Some(parent) = post_by_id(conn, parent_id).await? else {
            break;
        };
        segments.push(parent.slug.clone());
        cursor = parent.parent_id;
    }
    segments.reverse();

    Ok(Some(segments))
}

/// Refuse content whose URL a framework route already serves.
///
/// Pure, and separate from `guard_page_path`, because a page's ancestry is the
/// only path shape that needs a database walk to compute. A term archive is
/// `{rewrite_base}/{slug}` and a custom type's item is `{type}/{slug}` — both
/// nested, both able to collide with a configured probe, and both settled
/// without a query.
///
/// `guard_page_path` covered pages alone, which is exactly the shape this PR
/// keeps producing: the fix applied where the finding pointed and nowhere else.
pub fn guard_claimed_path(segments: &[String], what: &str) -> AutumnResult<()> {
    if is_claimed_page_path(segments) {
        return Err(AutumnError::unprocessable_msg(format!(
            "/{} is served by this site's health probe, so a {what} there would never be \
             reachable",
            segments.join("/")
        )));
    }
    Ok(())
}

/// Refuse a term whose archive URL a framework route already serves.
pub fn guard_term_path(taxonomy: &str, slug: &str) -> AutumnResult<()> {
    let Some(registered) = crate::content_types::find_taxonomy(taxonomy) else {
        return Ok(());
    };
    guard_claimed_path(
        &[registered.rewrite_base.to_owned(), slug.to_owned()],
        "term archive",
    )
}

/// Every descendant of a page, by id.
///
/// Level by level, one query per level, bounded by the depth the resolver
/// walks. The editor used to compute this from the full set of pages it had
/// already loaded — which stopped being an option once the parent picker became
/// a bounded window, since a descendant outside the window would then have been
/// offered as its own ancestor's parent.
pub async fn descendant_ids(
    conn: &mut AsyncPgConnection,
    post_id: i64,
) -> AutumnResult<std::collections::HashSet<i64>> {
    let mut found = std::collections::HashSet::new();
    let mut level = vec![post_id];
    let mut depth = 0usize;
    while !level.is_empty() && depth <= MAX_PAGE_DEPTH + 2 {
        let children: Vec<i64> = posts::table
            .filter(posts::parent_id.eq_any(&level))
            .select(posts::id)
            .load(&mut *conn)
            .await?;
        // A pre-existing cycle (only reachable by a direct write) would
        // otherwise revisit the same rows forever.
        let next: Vec<i64> = children
            .into_iter()
            .filter(|id| found.insert(*id))
            .collect();
        level = next;
        depth += 1;
    }
    Ok(found)
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
    conn: &mut AsyncPgConnection,
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
        .first(&mut *conn)
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
        && would_create_cycle(conn, post_id, candidate_parent_id).await?
    {
        return Err(AutumnError::unprocessable_msg(
            "A page cannot be placed under itself or one of its own children",
        ));
    }
    // The depth that matters is the *deepest descendant's*, not the moved
    // page's. Checking only where this row would land let a subtree be dragged
    // under a parent deep enough to push its own children past the bound: the
    // move validated, and then `page_ancestry` truncated those children's
    // canonical paths while `resolve_page_path` still walked down from a real
    // root — so each of them 404'd at the URL the site itself published for it.
    // A page being created has no descendants, so this reduces to the old check
    // on that path.
    let moved_height = match post_id {
        Some(post_id) => subtree_height(conn, post_id).await?,
        None => 0,
    };
    if depth_under(conn, candidate_parent_id).await? + moved_height >= MAX_PAGE_DEPTH {
        return Err(AutumnError::unprocessable_msg(format!(
            "Pages can be nested at most {MAX_PAGE_DEPTH} levels deep"
        )));
    }
    Ok(())
}

/// Re-parent a post. Used by the importer's ancestry pass.
///
/// Returns whether the link was applied. The importer resolves a parent by slug
/// against rows already on the site, which can name a trashed row, a row of
/// another type, or one already at `MAX_PAGE_DEPTH` — none of which the editor
/// would accept. Writing the link anyway produced a child whose generated
/// ancestry the resolver could not walk, so the imported page was unreachable
/// at its own canonical URL.
///
/// Invalid links are skipped rather than raised: an import that aborts part-way
/// leaves the site half-restored, which is worse than one page landing at the
/// top level. The caller reports the count.
pub async fn set_post_parent(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    parent_id: i64,
) -> AutumnResult<bool> {
    let post: Option<Post> = posts::table
        .find(post_id)
        .select(Post::as_select())
        .first(&mut *conn)
        .await
        .optional()?;
    let Some(post) = post else {
        return Ok(false);
    };

    // The lock and the transaction belong to *this function*, not to its
    // callers. The editor's re-parenting takes them and the importer's did not,
    // so an import making A a child of B while an editor made B a child of A
    // could commit a cycle — and both pages then resolve nowhere, because page
    // resolution walks down from a root. Owning them here is what stops the
    // next caller forgetting, which is how this gap appeared.
    // A refusal has to leave the transaction, not return from inside it. The
    // path guard runs *after* the `UPDATE` — it cannot run before, because the
    // path is only settled once the parent is stored — so returning `Ok(false)`
    // there committed the re-parent and then reported that the page had been
    // left where it was. On a restore that is a page moved onto a claimed path,
    // shadowed by the probe, while the report says it stayed at the top level.
    let outcome = conn
        .transaction(async move |conn| {
            lock_page_hierarchy(conn).await?;
            if validate_parent(conn, Some(post_id), &post.post_type, parent_id)
                .await
                .is_err()
            {
                return Err(ParentRefused::Declined);
            }
            diesel::update(posts::table.find(post_id))
                .set(posts::parent_id.eq(parent_id))
                .execute(conn)
                .await?;
            // Re-parenting is the other way a page's path changes — and the
            // descendants' paths with it, which is what `guard_page_path`
            // walks.
            if guard_page_path(conn, post_id).await.is_err() {
                return Err(ParentRefused::Declined);
            }
            Ok::<_, ParentRefused>(true)
        })
        .await;

    // Declining is not an error to the caller: the importer treats an unusable
    // link as "leave this page at the top level" and reports the count. It is
    // an error to the *transaction*, which is the point.
    match outcome {
        Ok(applied) => Ok(applied),
        Err(ParentRefused::Declined) => Ok(false),
        Err(ParentRefused::Failed(error)) => Err(error),
    }
}

/// Why a re-parent did not happen.
///
/// A refusal travels as an error so the transaction rolls back, and is
/// translated to `Ok(false)` outside it — see [`set_post_parent`].
enum ParentRefused {
    /// The link is not one the editor would accept, or it would put the page or
    /// one of its descendants on a claimed path.
    Declined,
    /// Something actually went wrong.
    Failed(AutumnError),
}

impl From<diesel::result::Error> for ParentRefused {
    fn from(error: diesel::result::Error) -> Self {
        Self::Failed(AutumnError::from(error))
    }
}

impl From<AutumnError> for ParentRefused {
    fn from(error: AutumnError) -> Self {
        Self::Failed(error)
    }
}

/// Delete a comment and rebuild the post's approved-comment counter — in one
/// transaction.
///
/// `comments.parent_id` cascades, so deleting a comment deletes its whole reply
/// subtree. Decrementing by one for the row named in the request therefore left
/// every approved descendant permanently included in the post's displayed
/// count. The counter is recomputed from ground truth rather than adjusted by a
/// delta, so it is right whatever the cascade removed.
pub async fn delete_comment(conn: &mut AsyncPgConnection, comment_id: i64) -> AutumnResult<()> {
    conn.transaction(async move |conn| {
        let comment: Comment = comments::table
            .find(comment_id)
            .select(Comment::as_select())
            .first(conn)
            .await
            .map_err(AutumnError::not_found)?;

        // The same lock `moderate_comment` takes first, in the same order: a
        // delete cascading over a subtree and an approval walking that
        // subtree's ancestors are the same race, and one lock ordering for
        // every path that changes a thread is what makes it total rather than
        // nearly total.
        posts::table
            .find(comment.post_id)
            .select(posts::id)
            .for_update()
            .first::<i64>(conn)
            .await
            .map_err(AutumnError::not_found)?;

        diesel::delete(comments::table.find(comment_id))
            .execute(conn)
            .await?;

        recount_post_comments(conn, comment.post_id).await?;
        Ok::<_, AutumnError>(())
    })
    .await
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

    // The vector a *reader without the password* is allowed to match against.
    //
    // `search_vector` covers title, excerpt and body. For a password-protected
    // post the body is exactly what the password withholds, so matching it here
    // turned search into an oracle: a caller could probe words and learn
    // whether they occur in protected content, without ever having the
    // password. Title and a hand-written excerpt stay searchable because both
    // are already public — the index renders them and the password form is
    // titled. A derived excerpt is not in play; `display_excerpt` returns empty
    // for a protected post with no hand-written one.
    const PUBLIC_VECTOR: &str = "(CASE WHEN password = '' THEN search_vector \
         ELSE setweight(to_tsvector('english', COALESCE(title, '')), 'A') \
              || setweight(to_tsvector('english', COALESCE(excerpt, '')), 'B') \
         END)";

    let match_predicate = format!(
        "status = 'publish' \
         AND {PUBLIC_VECTOR} @@ websearch_to_tsquery('english', $1) \
         AND post_type = ANY($2)"
    );
    let match_predicate = match_predicate.as_str();

    let total: i64 = diesel::sql_query(format!(
        "SELECT COUNT(*) AS count FROM posts WHERE {match_predicate}"
    ))
    .bind::<Text, _>(query)
    .bind::<Array<Text>, _>(public_types.to_vec())
    .get_result::<Total>(conn)
    .await?
    .count;

    // Ranked on the same restricted vector, not on `search_vector`: ordering
    // derived from body matches would leak through position what the predicate
    // refuses to leak through membership.
    let matched: Vec<i64> = diesel::sql_query(format!(
        "SELECT id FROM posts WHERE {match_predicate} \
         ORDER BY ts_rank({PUBLIC_VECTOR}, websearch_to_tsquery('english', $1)) DESC, \
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

/// Add a menu item, refusing one the menu could never show.
///
/// The screen that manages a menu and the navigation that renders it both read
/// the first `limit` items ordered by `(position, id)` — bounded so a menu
/// cannot make every page view unbounded. An unbounded *insert* behind a
/// bounded read is the gap: the item is accepted, appears nowhere, and has no
/// delete control, so the only way to reach it is to remove a visible item
/// first. Refusing is the honest answer, and it names the way out.
///
/// The count and the insert are one transaction, over the menu's items locked
/// `FOR UPDATE`, so two administrators filling the last slot cannot both see
/// room. With no items there is nothing to lock and nothing to race over.
pub async fn insert_menu_item(
    conn: &mut AsyncPgConnection,
    new: crate::models::NewMenuItem,
    limit: i64,
) -> AutumnResult<()> {
    // The model's declared rules, which this direct insert never runs.
    crate::hooks::validate_new_menu_item(&new)?;
    conn.transaction(async move |conn| {
        // The *menu* row, not its items. Locking the children serializes
        // nothing when there are none: concurrent transactions each lock zero
        // rows, each count zero, and each insert — so a burst of simultaneous
        // submissions can carry an empty menu straight past the bound, which is
        // the one case a capacity check exists for. The container row is always
        // there, so it is the thing to serialize on.
        let _locked: Option<i64> = menus::table
            .find(new.menu_id)
            .select(menus::id)
            .for_update()
            .first(conn)
            .await
            .optional()?;
        let existing: i64 = menu_items::table
            .filter(menu_items::menu_id.eq(new.menu_id))
            .count()
            .get_result(conn)
            .await?;
        if existing >= limit {
            return Err(AutumnError::unprocessable_msg(format!(
                "This menu already has {limit} items, which is as many as the \
                 navigation shows. Remove one before adding another."
            )));
        }
        diesel::insert_into(menu_items::table)
            .values(&new)
            .execute(conn)
            .await?;
        Ok::<_, AutumnError>(())
    })
    .await
}

/// The advisory-lock class the sidebar capacity check uses.
///
/// Two-int advisory locks live in a different space from the single-`bigint`
/// ones (`ADMIN_SET_LOCK_KEY`, `PAGE_HIERARCHY_LOCK_KEY`), so this cannot
/// collide with them however the second half hashes.
const WIDGET_SET_LOCK_CLASS: i32 = 7_717_260;

/// Add a sidebar widget, refusing one the sidebar could never show.
///
/// Same shape as [`insert_menu_item`]: the Appearance screen and every public
/// page read the first `limit` widgets of the sidebar, so an unbounded insert
/// behind that bounded read produces a widget that is invisible and
/// undeletable.
pub async fn insert_widget(
    conn: &mut AsyncPgConnection,
    new: crate::models::NewWidget,
    limit: i64,
) -> AutumnResult<()> {
    conn.transaction(async move |conn| {
        // A sidebar has no row of its own to lock, and locking the widgets
        // already in it serializes nothing while it is empty — see
        // `insert_menu_item`. An advisory lock keyed on the sidebar's name is
        // the stable thing here: it exists whether or not any widget does.
        //
        // Its own key space, so it orders against nothing else. Widgets are a
        // leaf: no path holds a widget lock and then reaches for a post, a term
        // or the hierarchy.
        diesel::sql_query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
            .bind::<diesel::sql_types::Integer, _>(WIDGET_SET_LOCK_CLASS)
            .bind::<diesel::sql_types::Text, _>(new.sidebar.clone())
            .execute(conn)
            .await?;
        let existing: i64 = widgets::table
            .filter(widgets::sidebar.eq(&new.sidebar))
            .count()
            .get_result(conn)
            .await?;
        if existing >= limit {
            return Err(AutumnError::unprocessable_msg(format!(
                "This sidebar already has {limit} widgets, which is as many as it \
                 shows. Remove one before adding another."
            )));
        }
        diesel::insert_into(widgets::table)
            .values(&new)
            .execute(conn)
            .await?;
        Ok::<_, AutumnError>(())
    })
    .await
}

/// The location index, which is an answer to give the administrator.
///
/// `menus.slug` is `UNIQUE` and `idx_menus_location` is a partial unique index,
/// so both raise `UniqueViolation` and only the constraint name tells them
/// apart. They want opposite handling: a lost race for a *location* is a real
/// conflict worth reporting, because resolving it silently would detach a menu
/// somebody else just attached. A lost race for a *slug* is not a conflict at
/// all — see [`MENU_SLUG_INDEX`].
const MENU_LOCATION_INDEX: &[(&str, &str, &str)] = &[(
    "idx_menus_location",
    "location",
    "Another menu was just assigned to that location; try again",
)];

/// The slug index, which is a signal to allocate again.
///
/// Duplicate menu names are supported, so a slug collision is never something
/// to tell the administrator about: it means another transaction took the
/// suffix this one had picked, and the answer is to pick the next one. Reported
/// as an error it claimed the *name* was unavailable — which is both wrong and
/// unactionable, since the name is fine and only the invisible slug collided.
const MENU_SLUG_INDEX: &[(&str, &str, &str)] = &[("menus_slug_key", "name", "")];

/// A free slug for a new menu.
///
/// `slugify` alone was the whole allocator, and `menus.slug` is `NOT NULL
/// UNIQUE`, so two menus named the same thing collided outright — an ordinary
/// thing for an administrator to want, reported as somebody else winning a race
/// for the location. Two names that differ only in punctuation do it too:
/// `Main menu` and `Main-Menu` both slugify to `main-menu`.
///
/// A suffix rather than a refusal: the slug is not a URL here — the theme
/// addresses a menu by `location` — so it only has to be unique, and making the
/// administrator invent a second name for it would be asking them to work
/// around a detail they cannot see.
async fn unique_menu_slug(conn: &mut AsyncPgConnection, name: &str) -> AutumnResult<String> {
    // `slugify` never returns an empty string — it falls back to a stable hash
    // token — so there is no empty case to guard here.
    let base = autumn_web::slugify(name);
    // One query for the whole family, not one per candidate.
    let taken: Vec<String> = menus::table
        .filter(
            menus::slug
                .eq(&base)
                .or(menus::slug.like(format!("{base}-%"))),
        )
        .select(menus::slug)
        .load(conn)
        .await?;
    if !taken.iter().any(|slug| slug == &base) {
        return Ok(base);
    }
    // Bounded: past this the name is being used as a counter, and the unique
    // index is still there to catch the race either way.
    for n in 2..=1000 {
        let candidate = format!("{base}-{n}");
        if !taken.iter().any(|slug| slug == &candidate) {
            return Ok(candidate);
        }
    }
    Err(AutumnError::unprocessable_msg(
        "Too many menus share that name; give this one a different one",
    ))
}

/// Assign a theme location to a new menu, clearing the previous holder — in one
/// transaction.
///
/// Only one menu can hold a location, so creating a replacement has to detach
/// the incumbent. Doing that as two statements means a failed insert (a
/// duplicate slug is the easy way to get one) leaves the site with *no* menu at
/// that location — the navigation simply disappears, from a request that
/// reported an error.
pub async fn replace_menu_at_location(
    conn: &mut AsyncPgConnection,
    name: &str,
    location: &str,
) -> AutumnResult<()> {
    // Allocate-then-insert is a read followed by a write, so two administrators
    // creating same-named menus at once can both read the same `taken` set and
    // both pick the same suffix. The unique index catches the second one, and
    // re-running the whole attempt is the answer: the loser now sees the
    // winner's committed row and takes the next suffix. Same shape as
    // `insert_post_with_unique_slug`, and for the same reason — the index is
    // the invariant and the allocator is an optimization over it.
    //
    // The retry is the whole transaction, not just the allocation, because the
    // location clearing and the insert have to stay atomic.
    for _ in 0..5 {
        let name = name.to_owned();
        let location = location.to_owned();
        let outcome = conn
            .transaction(async move |conn| {
                if !location.is_empty() {
                    // `FOR UPDATE` on the incumbent, so two administrators
                    // assigning the same location serialize here rather than
                    // each clearing what they saw and both inserting.
                    // `idx_menus_location` is the backstop — it makes the
                    // invariant the database's, which is what holds when there
                    // is no incumbent to lock and both inserts race.
                    let _: Vec<i64> = menus::table
                        .filter(menus::location.eq(&location))
                        .select(menus::id)
                        .for_update()
                        .load(conn)
                        .await?;
                    diesel::update(menus::table.filter(menus::location.eq(&location)))
                        .set(menus::location.eq(""))
                        .execute(conn)
                        .await?;
                }
                let slug = unique_menu_slug(conn, &name).await?;
                diesel::insert_into(menus::table)
                    .values((
                        menus::name.eq(&name),
                        menus::slug.eq(&slug),
                        menus::location.eq(&location),
                    ))
                    .execute(conn)
                    .await
                    .map_err(|error| {
                        // Only the location index becomes a message here. A slug
                        // collision is left as the database error it is, so the
                        // loop below can recognize it and allocate again.
                        let error = AutumnError::from(error);
                        match autumn_web::error::unique_violation_field(&error, MENU_LOCATION_INDEX)
                        {
                            Some((_, message)) => AutumnError::conflict_msg(message),
                            None => error,
                        }
                    })?;
                Ok::<_, AutumnError>(())
            })
            .await;
        match outcome {
            Ok(()) => return Ok(()),
            Err(error)
                if autumn_web::error::unique_violation_field(&error, MENU_SLUG_INDEX).is_some() =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    Err(AutumnError::conflict_msg(
        "Could not allocate a name for this menu; try again",
    ))
}

/// Terms of a taxonomy that have at least one published, publicly-routable
/// post, bounded.
///
/// Membership is derived from the posts themselves rather than read from
/// `terms.post_count`. The counter is maintained by `recount_term`, which
/// applies the *current* `public_type_slugs()` — so it is right whenever it
/// runs, and stale the moment the answer to "is this type public?" changes
/// without a write to touch it. `register_post_type` supports replacing a
/// registration, so a deployment can flip a type's visibility between restarts
/// and nothing recomputes the counters: the sitemap and the widgets would then
/// advertise an archive whose own listing is empty, or omit one that is full.
///
/// Correct by construction is worth a subquery here. The count is still what
/// orders the result — popularity is a heuristic and a stale one costs nothing
/// — and the limit still bounds the work, which is what the unauthenticated
/// sitemap needs.
pub async fn populated_terms(
    conn: &mut AsyncPgConnection,
    taxonomy: &str,
    limit: i64,
) -> AutumnResult<Vec<Term>> {
    Ok(terms::table
        .filter(terms::taxonomy.eq(taxonomy))
        .filter(diesel::dsl::exists(
            post_terms::table
                .inner_join(posts::table.on(posts::id.eq(post_terms::post_id)))
                .filter(post_terms::term_id.eq(terms::id))
                .filter(posts::status.eq("publish"))
                .filter(posts::post_type.eq_any(public_type_slugs())),
        ))
        .order((terms::post_count.desc(), terms::id.asc()))
        .limit(limit.max(0))
        .select(Term::as_select())
        .load(conn)
        .await?)
}

// ── Export ──────────────────────────────────────────────────────────────────

/// Every row an export writes, read from one connection.
///
/// The reads were repository calls, each taking its own pooled connection and
/// therefore its own snapshot: a page renamed while the export ran could be
/// written into the file under its old slug as a post and its new slug as some
/// other page's ancestor. The restore then cannot resolve that parent and files
/// the child at the top level — a backup that is silently wrong, which is worse
/// than one that fails. See [`export_snapshot`].
pub struct ExportRows {
    /// Every term, grouped by taxonomy in the registry's order.
    pub terms_by_taxonomy: Vec<(String, Vec<Term>)>,
    /// The posts to export: registered types, trash excluded, type by type.
    pub posts: Vec<Post>,
    /// Every post by id, *including* trash and unregistered types. A page's
    /// path is built through its ancestors, and an ancestor may be either.
    pub posts_by_id: std::collections::HashMap<i64, Post>,
    /// Author usernames by account id.
    pub usernames: std::collections::HashMap<i64, String>,
    /// The terms each post is filed under, by post id.
    pub terms_by_post: std::collections::HashMap<i64, Vec<Term>>,
    /// Every attachment, oldest first.
    pub attachments: Vec<crate::models::Attachment>,
    /// Retained revisions of the exported posts, by post id, oldest first.
    pub revisions_by_post: std::collections::HashMap<i64, Vec<Revision>>,
    /// Custom fields on the exported posts, by post id, excluding the
    /// importer's own private keys.
    pub meta_by_post: std::collections::HashMap<i64, Vec<(String, String)>>,
    /// Every comment on the exported posts, by post id, in creation order.
    ///
    /// All statuses, not just approved: a backup that restores a site without
    /// its moderation queue has thrown away work, and one that restores spam as
    /// discussion is worse.
    pub comments_by_post: std::collections::HashMap<i64, Vec<Comment>>,
    /// The same rows by id, for resolving featured images.
    pub attachments_by_id: std::collections::HashMap<i64, crate::models::Attachment>,
}

impl ExportRows {
    /// A page's ancestor slugs, outermost first, resolved in memory.
    ///
    /// The same walk and the same bound as [`crate::routes::site::Repos::page_ancestry`],
    /// against the snapshot rather than the live table — which is the whole
    /// point: a path assembled from rows read at different instants can name an
    /// ancestor by a slug the file also writes differently elsewhere.
    #[must_use]
    pub fn ancestry(&self, post: &Post) -> Vec<String> {
        let mut slugs = Vec::new();
        let mut cursor = post.parent_id;
        let mut seen = vec![post.id];
        while let Some(parent_id) = cursor {
            if slugs.len() >= MAX_PAGE_DEPTH || seen.contains(&parent_id) {
                break;
            }
            seen.push(parent_id);
            match self.posts_by_id.get(&parent_id) {
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
}

/// Read everything an export needs from one repeatable-read snapshot.
///
/// `REPEATABLE READ` rather than the default `READ COMMITTED`: every statement
/// in the transaction sees the database as of the first one, so no concurrent
/// rename, retitle or re-filing can land between two of these reads and make
/// the file internally inconsistent. `READ ONLY` says so to the server and lets
/// it skip the work a writable snapshot costs.
pub async fn export_snapshot(conn: &mut AsyncPgConnection) -> AutumnResult<ExportRows> {
    conn.build_transaction()
        .repeatable_read()
        .read_only()
        .run(async |conn| export_rows(conn).await)
        .await
}

/// The reads themselves. Separate from [`export_snapshot`] so the transaction
/// is one line and cannot accidentally gain a statement outside itself.
async fn export_rows(conn: &mut AsyncPgConnection) -> AutumnResult<ExportRows> {
    // Every taxonomy the *rows* mention, not only the registered ones. A plugin
    // that is disabled when the backup is taken leaves its terms in the table
    // and its registration absent, so a file built from the registry would omit
    // them — and re-enabling the plugin after a restore could not recover the
    // taxonomy it defined. A backup is of the database, not of the process.
    //
    // Registered first, in registry order, then the rest alphabetically, so the
    // file's shape stays deterministic either way.
    let mut taxonomies: Vec<String> = crate::content_types::all_taxonomies()
        .iter()
        .map(|taxonomy| taxonomy.slug.to_owned())
        .collect();
    let mut stored_taxonomies: Vec<String> = terms::table
        .select(terms::taxonomy)
        .distinct()
        .order(terms::taxonomy.asc())
        .load(conn)
        .await?;
    stored_taxonomies.retain(|slug| !taxonomies.contains(slug));
    taxonomies.extend(stored_taxonomies);

    let mut terms_by_taxonomy = Vec::new();
    for taxonomy in taxonomies {
        let rows: Vec<Term> = terms::table
            .filter(terms::taxonomy.eq(&taxonomy))
            .order(terms::id.asc())
            .select(Term::as_select())
            .load(conn)
            .await?;
        if rows.is_empty() {
            continue;
        }
        terms_by_taxonomy.push((taxonomy, rows));
    }

    // Every post in one query, trash and unregistered types included, because
    // an ancestor may be either and the path has to be built through it.
    let all_posts: Vec<Post> = posts::table
        .order(posts::id.asc())
        .select(Post::as_select())
        .load(conn)
        .await?;
    let posts_by_id: std::collections::HashMap<i64, Post> = all_posts
        .iter()
        .map(|post| (post.id, post.clone()))
        .collect();

    // Type by type, in registry order, so the file's shape does not depend on
    // insertion order. Trash is deliberately excluded: an export is a backup of
    // the site's content, and restoring somebody's deleted drafts into a fresh
    // install is a surprise, not a feature.
    // Every type the rows mention, for the reason the taxonomies are: a disabled
    // plugin's content is still the site's content, and a backup that omits it
    // cannot be restored into a site that re-enables the plugin.
    let mut post_types: Vec<String> = crate::content_types::all_post_types()
        .iter()
        .map(|post_type| post_type.slug.to_owned())
        .collect();
    let mut stored_types: Vec<String> = all_posts
        .iter()
        .filter(|post| post.status != "trash")
        .map(|post| post.post_type.clone())
        .collect();
    stored_types.sort_unstable();
    stored_types.dedup();
    stored_types.retain(|slug| !post_types.contains(slug));
    post_types.extend(stored_types);

    let mut posts = Vec::new();
    for post_type in post_types {
        posts.extend(
            all_posts
                .iter()
                .filter(|post| post.post_type == post_type && post.status != "trash")
                .cloned(),
        );
    }

    let post_ids: Vec<i64> = posts.iter().map(|post| post.id).collect();

    // One query for every exported post's comments, ordered so the tree can be
    // rebuilt from parents down.
    let comment_rows: Vec<Comment> = if post_ids.is_empty() {
        Vec::new()
    } else {
        comments::table
            .filter(comments::post_id.eq_any(&post_ids))
            .order((comments::created_at.asc(), comments::id.asc()))
            .select(Comment::as_select())
            .load(conn)
            .await?
    };
    let mut comments_by_post: std::collections::HashMap<i64, Vec<Comment>> =
        std::collections::HashMap::new();
    for comment in &comment_rows {
        comments_by_post
            .entry(comment.post_id)
            .or_default()
            .push(comment.clone());
    }

    // Retained revisions, oldest first — the order they have to be replayed in
    // for `restore_revision` to mean the same thing after a restore.
    let revision_rows: Vec<Revision> = if post_ids.is_empty() {
        Vec::new()
    } else {
        revisions::table
            .filter(revisions::post_id.eq_any(&post_ids))
            .order((revisions::created_at.asc(), revisions::id.asc()))
            .select(Revision::as_select())
            .load(conn)
            .await?
    };
    let mut revisions_by_post: std::collections::HashMap<i64, Vec<Revision>> =
        std::collections::HashMap::new();
    for revision in &revision_rows {
        revisions_by_post
            .entry(revision.post_id)
            .or_default()
            .push(revision.clone());
    }

    // Custom fields. The importer's own markers are deliberately excluded: they
    // record where a row came from *in this database*, so carrying them into a
    // file would make the next restore treat a fresh row as one it had already
    // finished — and skip its terms, status and ancestry forever.
    let meta_rows: Vec<(i64, String, String)> = if post_ids.is_empty() {
        Vec::new()
    } else {
        post_meta::table
            .filter(post_meta::post_id.eq_any(&post_ids))
            .filter(post_meta::meta_key.ne_all(INTERNAL_META_KEYS))
            .order((post_meta::post_id.asc(), post_meta::id.asc()))
            .select((
                post_meta::post_id,
                post_meta::meta_key,
                post_meta::meta_value,
            ))
            .load(conn)
            .await?
    };
    let mut meta_by_post: std::collections::HashMap<i64, Vec<(String, String)>> =
        std::collections::HashMap::new();
    for (post_id, key, value) in meta_rows {
        meta_by_post.entry(post_id).or_default().push((key, value));
    }

    let attachments: Vec<crate::models::Attachment> = attachments::table
        .order(attachments::id.asc())
        .select(crate::models::Attachment::as_select())
        .load(conn)
        .await?;
    let attachments_by_id = attachments
        .iter()
        .map(|attachment| (attachment.id, attachment.clone()))
        .collect();

    // Post authors, comment authors, revision editors *and* media uploaders.
    // Resolving only the first left every registered commenter exported as a
    // guest, and every uploader unnamed — so a restore handed each Author's
    // files to whoever ran the import.
    let mut author_ids: Vec<i64> = posts.iter().map(|post| post.author_id).collect();
    author_ids.extend(comment_rows.iter().filter_map(|comment| comment.author_id));
    author_ids.extend(
        revision_rows
            .iter()
            .filter_map(|revision| revision.author_id),
    );
    author_ids.extend(
        attachments
            .iter()
            .filter_map(|attachment| attachment.uploader_id),
    );
    author_ids.sort_unstable();
    author_ids.dedup();
    let usernames = users_by_ids(conn, &author_ids)
        .await?
        .into_iter()
        .map(|(id, user)| (id, user.username))
        .collect();

    // Two queries for every post's terms rather than two per post.
    let links: Vec<(i64, i64)> = if post_ids.is_empty() {
        Vec::new()
    } else {
        post_terms::table
            .filter(post_terms::post_id.eq_any(&post_ids))
            .order((post_terms::post_id.asc(), post_terms::term_id.asc()))
            .select((post_terms::post_id, post_terms::term_id))
            .load(conn)
            .await?
    };
    let linked_term_ids: Vec<i64> = links.iter().map(|(_, term_id)| *term_id).collect();
    let linked_terms = terms_by_ids(conn, &linked_term_ids).await?;
    let mut terms_by_post: std::collections::HashMap<i64, Vec<Term>> =
        std::collections::HashMap::new();
    for (post_id, term_id) in links {
        if let Some(term) = linked_terms.get(&term_id) {
            terms_by_post.entry(post_id).or_default().push(term.clone());
        }
    }

    Ok(ExportRows {
        terms_by_taxonomy,
        posts,
        posts_by_id,
        usernames,
        terms_by_post,
        attachments,
        attachments_by_id,
        revisions_by_post,
        meta_by_post,
        comments_by_post,
    })
}

/// One retained revision as an export file describes it.
pub struct ImportedRevision {
    /// The editor's **username**, if the snapshot recorded one.
    pub author_username: Option<String>,
    pub title: String,
    pub excerpt: String,
    pub body: String,
    pub status: String,
    pub summary: String,
    pub created_at: chrono::NaiveDateTime,
}

/// Restore a post's revision history.
///
/// Returns how many snapshots this call wrote.
///
/// Replaces whatever the row has rather than appending, and that is the whole
/// difficulty: the importer creates every post as a draft and transitions it,
/// and both of those record revisions of their own. So a post always has
/// history by the time this runs, and appending would interleave the file's
/// record with the restore's own bookkeeping — leaving an editor a "history"
/// whose newest entries describe the import rather than anything anybody wrote.
/// The file's history is the true one; the rows the restore made along the way
/// are an artefact of restoring.
///
/// Bounded by [`REVISION_LIMIT`], newest kept, exactly as an edit is.
pub async fn import_revisions(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    incoming: &[ImportedRevision],
) -> AutumnResult<usize> {
    if incoming.is_empty() {
        return Ok(0);
    }
    let usernames: Vec<String> = incoming
        .iter()
        .filter_map(|revision| revision.author_username.clone())
        .collect();
    let accounts: std::collections::HashMap<String, i64> = if usernames.is_empty() {
        std::collections::HashMap::new()
    } else {
        users::table
            .filter(users::username.eq_any(&usernames))
            .select((users::username, users::id))
            .load::<(String, i64)>(conn)
            .await?
            .into_iter()
            .collect()
    };

    // Newest `REVISION_LIMIT`, in the order they were written.
    let keep: Vec<ImportedRevision> = incoming
        .iter()
        .skip(
            incoming
                .len()
                .saturating_sub(usize::try_from(REVISION_LIMIT).unwrap_or(usize::MAX)),
        )
        .map(|revision| ImportedRevision {
            author_username: revision.author_username.clone(),
            title: revision.title.clone(),
            excerpt: revision.excerpt.clone(),
            body: revision.body.clone(),
            status: revision.status.clone(),
            summary: revision.summary.clone(),
            created_at: revision.created_at,
        })
        .collect();

    conn.transaction(async move |conn| {
        let locked: Option<i64> = posts::table
            .find(post_id)
            .select(posts::id)
            .for_update()
            .first(conn)
            .await
            .optional()?;
        if locked.is_none() {
            return Ok(0);
        }
        diesel::delete(revisions::table.filter(revisions::post_id.eq(post_id)))
            .execute(conn)
            .await?;
        for revision in &keep {
            diesel::insert_into(revisions::table)
                .values((
                    revisions::post_id.eq(post_id),
                    revisions::title.eq(&revision.title),
                    revisions::excerpt.eq(&revision.excerpt),
                    revisions::body.eq(&revision.body),
                    revisions::status.eq(&revision.status),
                    revisions::author_id.eq(revision
                        .author_username
                        .as_ref()
                        .and_then(|username| accounts.get(username).copied())),
                    revisions::summary.eq(&revision.summary),
                    // Explicit, like a comment's: a history whose timestamps all
                    // say "the moment of the restore" is not a history.
                    revisions::created_at.eq(revision.created_at),
                ))
                .execute(conn)
                .await?;
        }
        Ok::<_, AutumnError>(keep.len())
    })
    .await
}

/// The `post_meta` keys this application owns.
///
/// Everything else in `post_meta` is a plugin's custom field, which an export
/// carries and an import restores. These two do not travel: they record where a
/// row came from *in this database*, so a file carrying them would make the next
/// restore treat a fresh row as one it had already finished — and skip its
/// terms, status and ancestry forever.
pub const INTERNAL_META_KEYS: &[&str] = &[IMPORT_SOURCE_SLUG_KEY, IMPORT_COMPLETED_KEY];

/// Restore a post's custom fields.
///
/// Returns how many rows this call created. The importer's own keys are refused
/// whatever a file claims — see [`INTERNAL_META_KEYS`] — so a hand-edited or
/// hostile backup cannot mark its rows as already-imported and make every later
/// restore skip them.
///
/// Replaces rather than appends: `post_meta` has no uniqueness constraint, so
/// re-running an import would otherwise give a post a second copy of every
/// field, and a reader taking "the" value would get whichever came back first.
pub async fn import_post_meta(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    fields: &[(String, String)],
) -> AutumnResult<usize> {
    let fields: Vec<(String, String)> = fields
        .iter()
        .filter(|(key, _)| !INTERNAL_META_KEYS.contains(&key.as_str()))
        .filter(|(key, _)| !key.trim().is_empty() && key.len() <= MAX_META_KEY)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if fields.is_empty() {
        return Ok(0);
    }
    conn.transaction(async move |conn| {
        let keys: Vec<&str> = fields.iter().map(|(key, _)| key.as_str()).collect();
        diesel::delete(
            post_meta::table
                .filter(post_meta::post_id.eq(post_id))
                .filter(post_meta::meta_key.eq_any(&keys)),
        )
        .execute(conn)
        .await?;
        for (key, value) in &fields {
            diesel::insert_into(post_meta::table)
                .values((
                    post_meta::post_id.eq(post_id),
                    post_meta::meta_key.eq(key),
                    post_meta::meta_value.eq(value),
                ))
                .execute(conn)
                .await?;
        }
        Ok::<_, AutumnError>(fields.len())
    })
    .await
}

/// The longest custom-field key accepted from a file.
///
/// A key is an identifier a plugin looks rows up by, not prose; a file naming a
/// megabyte-long one is not a backup this site wrote.
pub const MAX_META_KEY: usize = 255;

/// The `post_meta` key under which the importer records the slug a post carried
/// in the file it came from.
///
/// The stored row's own slug is not that slug whenever the allocator had to
/// suffix it, so this is what makes a re-import idempotent. Underscore-prefixed
/// in WordPress's convention for meta the UI does not show.
pub const IMPORT_SOURCE_SLUG_KEY: &str = "_import_source_slug";

/// Record which file slug an imported row came from.
///
/// Takes a connection so it can join the transaction that finishes the rest of
/// the import for that post: written separately, a failure left an *unmarked*
/// draft, which the next run reads as unrelated local content and skips
/// forever. Marked-and-unfinished is recoverable; unmarked-and-unfinished is
/// not.
pub async fn record_import_source(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    source_slug: &str,
) -> AutumnResult<()> {
    diesel::insert_into(post_meta::table)
        .values((
            post_meta::post_id.eq(post_id),
            post_meta::meta_key.eq(IMPORT_SOURCE_SLUG_KEY),
            post_meta::meta_value.eq(source_slug),
        ))
        .execute(conn)
        .await?;
    Ok(())
}

/// The `post_meta` key marking an imported row as *finished*.
///
/// The source-slug marker alone says only "an import created this row", and it
/// is written before the row's terms, status and ancestry are, so it cannot
/// also mean "and it is done". Without a separate completion record every later
/// import of the same backup re-applied the file's terms and status — silently
/// undoing an editor who had since re-filed the post or moved it back to draft,
/// on a screen that promises existing items are left alone.
pub const IMPORT_COMPLETED_KEY: &str = "_import_completed";

/// Mark imported rows as fully restored — terms, status and ancestry.
///
/// Written once, at the very end of the run: the ancestry pass comes after the
/// per-post loop, so marking earlier would let a failure there leave a post
/// recorded as finished with its parent never set, and the retry that exists to
/// repair exactly that would skip it.
pub async fn mark_imports_complete(
    conn: &mut AsyncPgConnection,
    post_ids: &[i64],
) -> AutumnResult<()> {
    if post_ids.is_empty() {
        return Ok(());
    }
    let rows: Vec<_> = post_ids
        .iter()
        .map(|id| {
            (
                post_meta::post_id.eq(id),
                post_meta::meta_key.eq(IMPORT_COMPLETED_KEY),
                post_meta::meta_value.eq("1"),
            )
        })
        .collect();
    diesel::insert_into(post_meta::table)
        .values(rows)
        .on_conflict_do_nothing()
        .execute(conn)
        .await?;
    Ok(())
}

/// The imported rows a previous run finished.
pub async fn completed_import_ids(
    conn: &mut AsyncPgConnection,
) -> AutumnResult<std::collections::HashSet<i64>> {
    Ok(post_meta::table
        .filter(post_meta::meta_key.eq(IMPORT_COMPLETED_KEY))
        .select(post_meta::post_id)
        .load::<i64>(conn)
        .await?
        .into_iter()
        .collect())
}

/// Every `(post_type, source slug)` a previous import recorded, and the row it
/// produced.
///
/// The id matters as well as the membership: a retry has to be able to reach
/// the existing row to finish work an interrupted run left undone, not merely
/// know that it should skip it.
///
/// Joined to `posts` so a row deleted since the import it came from does not
/// keep its slug reserved — re-importing content the site no longer holds is a
/// restore, and should work.
pub async fn imported_source_slugs(
    conn: &mut AsyncPgConnection,
) -> AutumnResult<std::collections::HashMap<(String, String), i64>> {
    Ok(post_meta::table
        .inner_join(posts::table.on(posts::id.eq(post_meta::post_id)))
        .filter(post_meta::meta_key.eq(IMPORT_SOURCE_SLUG_KEY))
        .select((posts::post_type, post_meta::meta_value, posts::id))
        .load::<(String, String, i64)>(conn)
        .await?
        .into_iter()
        .map(|(post_type, slug, id)| ((post_type, slug), id))
        .collect())
}

/// What the content-administration screen is asking for.
#[derive(Debug, Clone, Copy)]
pub struct AdminPostQuery<'a> {
    pub post_type: &'a str,
    /// `None` means "every status except trash", which is what WordPress's
    /// unfiltered list shows.
    pub status: Option<&'a str>,
    pub search: Option<&'a str>,
    /// Set for a role without `EditOthersPosts`, so the restriction is a
    /// predicate rather than a filter applied to rows already loaded.
    pub author_id: Option<i64>,
}

/// One page of the content-administration list, with its total.
///
/// Every part of the question — the type, the status, the search, the
/// Contributor's own-content restriction, the order and the bound — is asked of
/// Postgres. Each branch previously loaded the complete matching rows, bodies
/// and all, filtered and sorted them in Rust and rendered every one of them;
/// Authors and Contributors grow this table continuously, so the primary
/// content-management screen was on course to become the least usable one on
/// the site.
pub async fn admin_posts_page(
    conn: &mut AsyncPgConnection,
    query: &AdminPostQuery<'_>,
    offset: i64,
    limit: i64,
) -> AutumnResult<(Vec<Post>, i64)> {
    use diesel::sql_types::{BigInt, Nullable, Text};

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

    // A status of `NULL` means the unfiltered list, which hides trash; a search
    // of `NULL` means no search. Expressed as bound parameters rather than by
    // concatenating SQL, so the predicate below is one constant string and the
    // untrusted search text never reaches the query text.
    //
    // `search_vector` is used unrestricted here, unlike `search_published`: this
    // screen is behind `EditPosts` and every row it can return is one the caller
    // may open and read in full, so there is nothing for a restricted vector to
    // withhold.
    const PREDICATE: &str = "post_type = $1          AND (CASE WHEN $2::text IS NULL THEN status <> 'trash' ELSE status = $2 END)          AND ($3::text IS NULL OR search_vector @@ websearch_to_tsquery('english', $3))          AND ($4::bigint IS NULL OR author_id = $4)";

    let total: i64 = diesel::sql_query(format!(
        "SELECT COUNT(*) AS count FROM posts WHERE {PREDICATE}"
    ))
    .bind::<Text, _>(query.post_type)
    .bind::<Nullable<Text>, _>(query.status)
    .bind::<Nullable<Text>, _>(query.search)
    .bind::<Nullable<BigInt>, _>(query.author_id)
    .get_result::<Total>(conn)
    .await?
    .count;

    let matched: Vec<i64> = diesel::sql_query(format!(
        "SELECT id FROM posts WHERE {PREDICATE}          ORDER BY updated_at DESC, id DESC LIMIT $5 OFFSET $6"
    ))
    .bind::<Text, _>(query.post_type)
    .bind::<Nullable<Text>, _>(query.status)
    .bind::<Nullable<Text>, _>(query.search)
    .bind::<Nullable<BigInt>, _>(query.author_id)
    .bind::<BigInt, _>(limit.max(0))
    .bind::<BigInt, _>(offset.max(0))
    .load::<MatchedId>(conn)
    .await?
    .into_iter()
    .map(|row| row.id)
    .collect();

    if matched.is_empty() {
        return Ok((Vec::new(), total));
    }

    let mut rows: Vec<Post> = posts::table
        .filter(posts::id.eq_any(&matched))
        .select(Post::as_select())
        .load(conn)
        .await?;
    rows.sort_by_key(|post| {
        matched
            .iter()
            .position(|id| *id == post.id)
            .unwrap_or(usize::MAX)
    });

    Ok((rows, total))
}

/// One page of the accounts list, ordered by username, bounded in SQL.
///
/// Open registration is the shipped default and the per-IP throttle bounds the
/// rate rather than the total, so this table grows without any single signup
/// being invalid — and the screen an administrator would use to clear a signup
/// flood renders one or two forms per row, which made it the first one to stop
/// working.
pub async fn users_page(
    conn: &mut AsyncPgConnection,
    offset: i64,
    limit: i64,
) -> AutumnResult<Vec<User>> {
    Ok(users::table
        .order((users::username.asc(), users::id.asc()))
        .offset(offset.max(0))
        .limit(limit.max(0))
        .select(User::as_select())
        .load(conn)
        .await?)
}

/// How many accounts the site holds, for the pager.
pub async fn user_count(conn: &mut AsyncPgConnection) -> AutumnResult<i64> {
    Ok(users::table.count().get_result(conn).await?)
}

/// The most recently updated live rows of a hierarchical type, for a parent
/// picker, with the total so the caller can say the list is a window.
///
/// The picker loaded every row of the type — full bodies included — and
/// rendered nearly all of them as `<option>`s, so paginating the content list
/// left the editor itself as the screen that grows without bound. Only the
/// columns a picker shows are selected.
pub async fn parent_candidates(
    conn: &mut AsyncPgConnection,
    post_type: &str,
    limit: i64,
) -> AutumnResult<(Vec<Post>, i64)> {
    let total: i64 = posts::table
        .filter(posts::post_type.eq(post_type))
        .filter(posts::status.ne("trash"))
        .count()
        .get_result(conn)
        .await?;
    let rows = posts::table
        .filter(posts::post_type.eq(post_type))
        .filter(posts::status.ne("trash"))
        .order((posts::updated_at.desc(), posts::id.desc()))
        .limit(limit.max(0))
        .select(Post::as_select())
        .load(conn)
        .await?;
    Ok((rows, total))
}

/// One page of a taxonomy's terms with the taxonomy's total, for the admin
/// screen's list and its pager.
pub async fn terms_page_with_total(
    conn: &mut AsyncPgConnection,
    taxonomy: &str,
    offset: i64,
    limit: i64,
) -> AutumnResult<(Vec<Term>, i64)> {
    let total: i64 = terms::table
        .filter(terms::taxonomy.eq(taxonomy))
        .count()
        .get_result(conn)
        .await?;
    let rows = terms_page(conn, taxonomy, offset, limit).await?;
    Ok((rows, total))
}

/// The posts named by `ids`, plus every ancestor needed to build their
/// permalinks, as one map.
///
/// A menu resolved item by item cost one query per entry and, for a page, one
/// more per ancestor — on every public page render, with the item count bounded
/// only by what an editor has added. This is instead one query per *level* of
/// the hierarchy, at most `MAX_PAGE_DEPTH` of them, whatever the menu's size.
///
/// The walk stops on a row already in the map, so a `parent_id` cycle (only
/// reachable by a direct write) terminates rather than looping.
pub async fn posts_with_ancestors(
    conn: &mut AsyncPgConnection,
    ids: &[i64],
) -> AutumnResult<std::collections::HashMap<i64, Post>> {
    let mut found: std::collections::HashMap<i64, Post> = std::collections::HashMap::new();
    let mut wanted: Vec<i64> = ids.to_vec();
    for _ in 0..=MAX_PAGE_DEPTH {
        wanted.retain(|id| !found.contains_key(id));
        if wanted.is_empty() {
            break;
        }
        let rows: Vec<Post> = posts::table
            .filter(posts::id.eq_any(&wanted))
            .select(Post::as_select())
            .load(&mut *conn)
            .await?;
        if rows.is_empty() {
            break;
        }
        wanted = rows.iter().filter_map(|post| post.parent_id).collect();
        for post in rows {
            found.insert(post.id, post);
        }
    }
    Ok(found)
}

/// Which of `ids` are terms in `taxonomy`, in one query.
///
/// The editor resolved a submitted id set one lookup at a time, so a crafted
/// save could turn a single bounded request into a query per id.
pub async fn term_ids_in_taxonomy(
    conn: &mut AsyncPgConnection,
    taxonomy: &str,
    ids: &[i64],
) -> AutumnResult<std::collections::HashSet<i64>> {
    if ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    Ok(terms::table
        .filter(terms::taxonomy.eq(taxonomy))
        .filter(terms::id.eq_any(ids))
        .select(terms::id)
        .load::<i64>(conn)
        .await?
        .into_iter()
        .collect())
}

/// The terms named by a set of ids, for resolving the parent names a page of
/// the term list refers to.
///
/// The list renders each term's parent name, and it used to find that parent in
/// the same in-memory set — which silently became wrong the moment the set was
/// a page rather than the whole taxonomy: a term whose parent sits on another
/// page would have rendered with no parent at all.
pub async fn terms_by_ids(
    conn: &mut AsyncPgConnection,
    ids: &[i64],
) -> AutumnResult<std::collections::HashMap<i64, Term>> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    Ok(terms::table
        .filter(terms::id.eq_any(ids))
        .select(Term::as_select())
        .load(conn)
        .await?
        .into_iter()
        .map(|term| (term.id, term))
        .collect())
}

/// The widgets placed in a sidebar, ordered and bounded in SQL.
///
/// A sidebar is chrome: it renders on *every* public page, and the widgets are
/// created through an ordinary admin form with no cap, so an unbounded read
/// here made the cost of every request a function of how many widgets somebody
/// had added. Past a couple of dozen it has stopped being a sidebar.
pub async fn sidebar_widgets(
    conn: &mut AsyncPgConnection,
    sidebar: &str,
    limit: i64,
) -> AutumnResult<Vec<crate::models::Widget>> {
    Ok(widgets::table
        .filter(widgets::sidebar.eq(sidebar))
        .order((widgets::position.asc(), widgets::id.asc()))
        .limit(limit.max(0))
        .select(crate::models::Widget::as_select())
        .load(conn)
        .await?)
}

/// One page of menus, oldest first, bounded in SQL.
pub async fn menus_page(
    conn: &mut AsyncPgConnection,
    offset: i64,
    limit: i64,
) -> AutumnResult<Vec<crate::models::Menu>> {
    Ok(menus::table
        .order((menus::name.asc(), menus::id.asc()))
        .offset(offset.max(0))
        .limit(limit.max(0))
        .select(crate::models::Menu::as_select())
        .load(conn)
        .await?)
}

/// How many menus the site holds, for the pager.
pub async fn menu_count(conn: &mut AsyncPgConnection) -> AutumnResult<i64> {
    Ok(menus::table.count().get_result(conn).await?)
}

/// The items of a whole page of menus, in one query, grouped by menu.
///
/// The Appearance screen issued one unbounded item query per menu, so the cost
/// of the screen was the number of menus times the size of each — and menus can
/// be created through the ordinary form with no deletion route, so that grows
/// and stays grown.
pub async fn menu_items_for(
    conn: &mut AsyncPgConnection,
    menu_ids: &[i64],
    per_menu: i64,
) -> AutumnResult<std::collections::HashMap<i64, Vec<crate::models::MenuItem>>> {
    if menu_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    // The per-menu bound is applied by Postgres, not after the rows arrive.
    // Grouping a full `load()` in Rust left the transfer and the memory
    // proportional to the largest menu — which is the one case the bound exists
    // for. A window function keeps it one round trip *and* one page of rows.
    use diesel::sql_types::{Array, BigInt};

    #[derive(diesel::QueryableByName)]
    struct RankedId {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }

    // The window function picks the ids; the rows themselves come back through
    // the DSL, which is what keeps `MenuItem` a plain `#[model]` rather than
    // needing a hand-written `QueryableByName` that would drift from it.
    let ids: Vec<i64> = diesel::sql_query(
        "SELECT id FROM (\
             SELECT id, ROW_NUMBER() OVER ( \
                 PARTITION BY menu_id ORDER BY position ASC, id ASC) AS rank \
             FROM menu_items WHERE menu_id = ANY($1)) ranked \
         WHERE rank <= $2",
    )
    .bind::<Array<BigInt>, _>(menu_ids.to_vec())
    .bind::<BigInt, _>(per_menu.max(0))
    .load::<RankedId>(conn)
    .await?
    .into_iter()
    .map(|row| row.id)
    .collect();
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }

    let rows: Vec<crate::models::MenuItem> = menu_items::table
        .filter(menu_items::id.eq_any(&ids))
        .order((
            menu_items::menu_id.asc(),
            menu_items::position.asc(),
            menu_items::id.asc(),
        ))
        .select(crate::models::MenuItem::as_select())
        .load(conn)
        .await?;

    let mut grouped: std::collections::HashMap<i64, Vec<crate::models::MenuItem>> =
        std::collections::HashMap::new();
    for row in rows {
        grouped.entry(row.menu_id).or_default().push(row);
    }
    Ok(grouped)
}

/// The accounts named by `ids`, in one query.
///
/// The public thread renderer looked each commenter up individually, so an
/// ordinary page view cost one round trip per distinct account in the thread.
pub async fn users_by_ids(
    conn: &mut AsyncPgConnection,
    ids: &[i64],
) -> AutumnResult<std::collections::HashMap<i64, User>> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    Ok(users::table
        .filter(users::id.eq_any(ids))
        .select(User::as_select())
        .load(conn)
        .await?
        .into_iter()
        .map(|user| (user.id, user))
        .collect())
}

/// The accounts that authored a page of posts, in one query.
///
/// The list screen looked each one up per row, so a fifty-row page cost fifty
/// round trips to render a column of names.
pub async fn authors_for_posts(
    conn: &mut AsyncPgConnection,
    rows: &[Post],
) -> AutumnResult<std::collections::HashMap<i64, User>> {
    let ids: Vec<i64> = rows.iter().map(|post| post.author_id).collect();
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    Ok(users::table
        .filter(users::id.eq_any(ids))
        .select(User::as_select())
        .load(conn)
        .await?
        .into_iter()
        .map(|user| (user.id, user))
        .collect())
}

/// Every index that means "this slug is taken", for the allocators' retry.
///
/// One list, because there are two allocators and a third caller reasoning
/// about the same question — and the sibling-page index added when nested page
/// slugs became per-parent was named in neither of them, so a lost race there
/// surfaced the raw constraint error instead of allocating `-2`.
pub const SLUG_COLLISION_INDEXES: &[(&str, &str, &str)] = &[
    (
        "idx_posts_bare_path_slug",
        "slug",
        "That URL is already taken",
    ),
    ("idx_posts_type_slug", "slug", "That URL is already taken"),
    ("idx_pages_parent_slug", "slug", "That URL is already taken"),
];

/// Insert a post with a free slug, on the caller's connection.
///
/// The `Repos` allocator does the same thing on a connection of its own, which
/// is what the editor and the API want. The importer needs this shape instead:
/// its insert has to commit in the *same* transaction as the
/// `_import_source_slug` marker, because a row written without one is the one
/// state a retry cannot recognise — the next run sees an ordinary local post
/// holding that slug and skips it forever, so its terms, status and ancestry
/// are never restored. A process killed between two statements is enough to
/// produce it.
///
/// Each attempt is a nested transaction, so a lost race rolls back to a
/// savepoint rather than poisoning the caller's transaction, and the retry can
/// allocate again.
pub async fn insert_post_with_unique_slug(
    conn: &mut AsyncPgConnection,
    new: crate::models::NewPost,
) -> AutumnResult<Post> {
    insert_with_unique_slug(conn, new, false).await
}

/// The same insert for a post a *file* describes.
///
/// Differs only in accepting an unregistered post type — see
/// [`crate::hooks::normalize_imported_post`]. A backup carries the content of a
/// plugin that was disabled when it was taken, and refusing it on the way back
/// in would mean such a backup could be produced and never restored.
pub async fn insert_imported_post_with_unique_slug(
    conn: &mut AsyncPgConnection,
    new: crate::models::NewPost,
) -> AutumnResult<Post> {
    insert_with_unique_slug(conn, new, true).await
}

async fn insert_with_unique_slug(
    conn: &mut AsyncPgConnection,
    new: crate::models::NewPost,
    imported: bool,
) -> AutumnResult<Post> {
    let mut new = new;
    if imported {
        crate::hooks::normalize_imported_post(&mut new)?;
    } else {
        crate::hooks::normalize_new_post(&mut new)?;
    }
    let desired = new.slug.clone();

    for _ in 0..5 {
        let attempt = new.clone();
        let desired = desired.clone();
        let outcome = conn
            .transaction(async move |conn| {
                let slug =
                    ensure_unique_slug(conn, &attempt.post_type, &desired, attempt.parent_id, None)
                        .await?;
                let row = crate::models::NewPost { slug, ..attempt };
                let saved: Post = diesel::insert_into(posts::table)
                    .values(&row)
                    .returning(Post::as_returning())
                    .get_result(conn)
                    .await?;
                guard_page_path(conn, saved.id).await?;
                // A custom type is addressed under its own prefix, so its items
                // mint a nested path too — `/product/widget` is as claimable as
                // `/about/team`, and needs no walk to work out.
                if !BARE_PATH_TYPES.contains(&saved.post_type.as_str()) {
                    guard_claimed_path(
                        &[saved.post_type.clone(), saved.slug.clone()],
                        &saved.post_type,
                    )?;
                }
                Ok::<_, AutumnError>(saved)
            })
            .await;
        match outcome {
            Ok(post) => return Ok(post),
            // Both slug indexes, for the reason the `Repos` allocator spells
            // out: either can be the one a lost race reports, and re-running
            // allocation is the right answer to both.
            Err(error)
                if autumn_web::error::unique_violation_field(&error, SLUG_COLLISION_INDEXES)
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

/// A term as an export file describes it.
///
/// The parent is a **slug**, not an id: ids mean nothing across installations,
/// which is the same rule the post ancestry and the term references follow.
#[derive(Debug, Clone)]
/// One comment as an export file describes it.
///
/// Nesting is expressed by embedding replies rather than by carrying ids: an id
/// means nothing in another database, and a parent column would need the same
/// two-pass resolution the terms need. A tree needs neither.
pub struct ImportedComment {
    /// The registered account's **username**, if the comment had one. Resolved
    /// against this site; an unknown one becomes a guest comment under the name
    /// the file carries, rather than being attributed to somebody else.
    pub author_username: Option<String>,
    pub author_name: String,
    pub author_email: String,
    pub author_url: String,
    pub body: String,
    pub status: String,
    pub created_at: chrono::NaiveDateTime,
    pub replies: Vec<ImportedComment>,
}

/// Restore a post's comments, with their nesting, authorship and moderation
/// states, in one transaction.
///
/// Returns how many comments this call created.
///
/// Skipped entirely when the post already has any comment. An import that says
/// it skips existing content must not append a second copy of a thread to a
/// post that already carries one — and unlike a post, a comment has no natural
/// key to dedupe on, so "this post already has a discussion" is the honest
/// guard. It is also what makes a re-run of a half-finished import safe.
pub async fn import_comments(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    incoming: &[ImportedComment],
) -> AutumnResult<usize> {
    if incoming.is_empty() {
        return Ok(0);
    }
    // Resolved before the transaction: one query for every username the file
    // mentions, rather than one per comment.
    let mut usernames: Vec<String> = Vec::new();
    collect_usernames(incoming, &mut usernames);
    usernames.sort_unstable();
    usernames.dedup();
    let accounts: std::collections::HashMap<String, i64> = if usernames.is_empty() {
        std::collections::HashMap::new()
    } else {
        users::table
            .filter(users::username.eq_any(&usernames))
            .select((users::username, users::id))
            .load::<(String, i64)>(conn)
            .await?
            .into_iter()
            .collect()
    };

    let incoming: Vec<ImportedComment> = incoming.iter().map(clone_imported).collect();
    conn.transaction(async move |conn| {
        // The post row first — the lock order every write here follows, and the
        // same lock the counter rebuild takes at the end.
        let locked: Option<Post> = posts::table
            .find(post_id)
            .select(Post::as_select())
            .for_update()
            .first(conn)
            .await
            .optional()?;
        if locked.is_none() {
            return Ok(0);
        }

        let existing: i64 = comments::table
            .filter(comments::post_id.eq(post_id))
            .count()
            .get_result(conn)
            .await?;
        if existing > 0 {
            return Ok(0);
        }

        let mut created = 0usize;
        let mut level: Vec<(Option<i64>, &ImportedComment)> =
            incoming.iter().map(|c| (None, c)).collect();
        for _ in 0..=MAX_COMMENT_DEPTH {
            if level.is_empty() {
                break;
            }
            let mut next: Vec<(Option<i64>, &ImportedComment)> = Vec::new();
            for (parent_id, comment) in level {
                let mut new = crate::models::NewComment {
                    post_id,
                    parent_id,
                    author_id: comment
                        .author_username
                        .as_ref()
                        .and_then(|username| accounts.get(username).copied()),
                    author_name: comment.author_name.clone(),
                    author_email: comment.author_email.clone(),
                    author_url: comment.author_url.clone(),
                    author_ip: String::new(),
                    body: comment.body.clone(),
                    // Clamped to a status the site recognises. An unknown one
                    // becomes `pending` rather than `approved`: the direction
                    // that shows a moderator something is the safe one.
                    status: if crate::hooks::COMMENT_STATUSES.contains(&comment.status.as_str()) {
                        comment.status.clone()
                    } else {
                        "pending".to_owned()
                    },
                };
                // The model's declared rules, which this direct insert never
                // runs. A file with an over-long or empty body is a file this
                // site would not have produced; skipping the row keeps the rest
                // of the restore rather than failing it.
                if crate::hooks::validate_comment(&mut new).is_err() {
                    continue;
                }
                // `created_at` explicitly, not the column default. A thread
                // restored with every timestamp set to the moment of the
                // restore has lost its chronology — and the renderer orders by
                // `(created_at, id)`, so it would also read back in a different
                // order than it was written in.
                let saved: Comment = diesel::insert_into(comments::table)
                    .values((
                        comments::post_id.eq(new.post_id),
                        comments::parent_id.eq(new.parent_id),
                        comments::author_id.eq(new.author_id),
                        comments::author_name.eq(&new.author_name),
                        comments::author_email.eq(&new.author_email),
                        comments::author_url.eq(&new.author_url),
                        comments::author_ip.eq(&new.author_ip),
                        comments::body.eq(&new.body),
                        comments::status.eq(&new.status),
                        comments::created_at.eq(comment.created_at),
                    ))
                    .returning(Comment::as_returning())
                    .get_result(conn)
                    .await?;
                created += 1;
                for reply in &comment.replies {
                    next.push((Some(saved.id), reply));
                }
            }
            level = next;
        }

        // Replies past the cap are dropped rather than flattened onto the root:
        // the renderer draws `MAX_COMMENT_DEPTH` levels, and a reply grafted
        // somewhere it does not belong is worse than one that is absent.

        recount_post_comments(conn, post_id).await?;
        Ok::<_, AutumnError>(created)
    })
    .await
}

fn collect_usernames(comments: &[ImportedComment], into: &mut Vec<String>) {
    for comment in comments {
        if let Some(username) = &comment.author_username {
            into.push(username.clone());
        }
        collect_usernames(&comment.replies, into);
    }
}

fn clone_imported(comment: &ImportedComment) -> ImportedComment {
    ImportedComment {
        author_username: comment.author_username.clone(),
        author_name: comment.author_name.clone(),
        author_email: comment.author_email.clone(),
        author_url: comment.author_url.clone(),
        body: comment.body.clone(),
        status: comment.status.clone(),
        created_at: comment.created_at,
        replies: comment.replies.iter().map(clone_imported).collect(),
    }
}

pub struct ImportedTerm {
    pub taxonomy: String,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub parent: Option<String>,
}

/// Restore a file's taxonomy — the rows and their ancestry — in one transaction.
///
/// Returns how many terms this call created.
///
/// Two passes are unavoidable: a child term can appear in the file before its
/// parent, so nothing can be linked until every row exists. What matters is
/// that both passes are one transaction. Run as separate statements, a failure
/// during the linking pass left the creations committed — and a retry then
/// found every one of those rows already present, so none joined the
/// "this run created it" set, and the branch that protects a locally-managed
/// hierarchy from being restructured by an import skipped the unfinished links
/// forever. The tree stayed flat while the retry reported success. All-or-
/// nothing makes the retry start from the same place the first run did.
///
/// The rule the second pass enforces is unchanged: only rows *this restore
/// created* are re-parented. A term the site already had is locally managed —
/// the first pass deliberately leaves its name and description alone, and
/// moving it under the file's parent would be the same contradiction.
pub async fn import_terms(
    conn: &mut AsyncPgConnection,
    incoming: &[ImportedTerm],
) -> AutumnResult<usize> {
    use crate::models::NewTerm;

    conn.transaction(async move |conn| {
        // Normalized once, up front, so the slug a row is stored under and the
        // slug a parent reference is resolved against come from the same
        // function. Resolving the raw file slug instead would miss any parent
        // whose slug `slugify` changed.
        let mut drafts: Vec<NewTerm> = Vec::with_capacity(incoming.len());
        for term in incoming {
            let mut draft = NewTerm {
                taxonomy: term.taxonomy.clone(),
                name: term.name.clone(),
                slug: term.slug.clone(),
                description: term.description.clone(),
                parent_id: None,
            };
            crate::hooks::normalize_imported_term(&mut draft)?;
            guard_term_path(&draft.taxonomy, &draft.slug)?;
            drafts.push(draft);
        }

        let mut created: std::collections::HashSet<i64> = std::collections::HashSet::new();
        for draft in &drafts {
            let existing: Option<Term> = terms::table
                .filter(terms::taxonomy.eq(&draft.taxonomy))
                .filter(terms::slug.eq(&draft.slug))
                .select(Term::as_select())
                .first(conn)
                .await
                .optional()?;
            if existing.is_none() {
                let row: Term = diesel::insert_into(terms::table)
                    .values(draft)
                    .returning(Term::as_returning())
                    .get_result(conn)
                    .await?;
                created.insert(row.id);
            }
        }

        for (term, draft) in incoming.iter().zip(&drafts) {
            let Some(parent_slug) = &term.parent else {
                continue;
            };
            // A *registered flat* taxonomy has no hierarchy to put a term in.
            // The hook says so on the create path; the direct `UPDATE` below
            // has to say it too, or the importer becomes the one way to give a
            // tag a parent.
            //
            // An *unregistered* taxonomy is a different answer: unknown, not
            // flat. Now that a backup carries the terms of a plugin that was
            // disabled when it was taken, restoring one while that plugin is
            // still disabled would read "not registered" as "has no hierarchy"
            // and drop every parent — and re-enabling the plugin afterwards
            // would find the taxonomy permanently flattened, because a second
            // import only re-parents rows *it* created. Only the file knows
            // what shape that taxonomy has, so the file is what to believe.
            let flat = crate::content_types::find_taxonomy(&draft.taxonomy)
                .is_some_and(|registered| !registered.hierarchical);
            if flat {
                continue;
            }
            let parent_slug = autumn_web::slugify(parent_slug);
            let child: Option<Term> = terms::table
                .filter(terms::taxonomy.eq(&draft.taxonomy))
                .filter(terms::slug.eq(&draft.slug))
                .select(Term::as_select())
                .first(conn)
                .await
                .optional()?;
            let parent: Option<Term> = terms::table
                .filter(terms::taxonomy.eq(&draft.taxonomy))
                .filter(terms::slug.eq(&parent_slug))
                .select(Term::as_select())
                .first(conn)
                .await
                .optional()?;
            let (Some(child), Some(parent)) = (child, parent) else {
                continue;
            };
            if !created.contains(&child.id) {
                continue;
            }
            if child.id != parent.id && child.parent_id != Some(parent.id) {
                diesel::update(terms::table.find(child.id))
                    .set(terms::parent_id.eq(parent.id))
                    .execute(conn)
                    .await?;
            }
        }

        Ok::<_, AutumnError>(created.len())
    })
    .await
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

/// The most approved comments one *page* of a rendered thread holds.
///
/// A public post's thread is served to anyone, and with guest comments enabled
/// anyone can also grow it. Without a bound, a post that has accumulated tens
/// of thousands of comments makes every single page view cost the whole set in
/// database and application memory — and the pending, spam and trashed rows a
/// moderation queue collects made it worse, because they were loaded and then
/// discarded in Rust.
///
/// [`THREAD_ROOTS_PER_PAGE`] bounds the roots and [`MAX_COMMENT_DEPTH`] bounds
/// the nesting, but neither bounds *breadth*: a single popular root can carry
/// any number of direct replies, so a page of fifty roots was unbounded again
/// by a different route. This is the bound on the total, and the renderer says
/// so when a page reaches it.
///
/// Twenty per root is generous enough that no real discussion reaches it.
pub const MAX_THREAD_COMMENTS: i64 = THREAD_ROOTS_PER_PAGE * 20;

/// One page of a moderation queue, newest first, ordered and bounded in SQL.
///
/// The generated finder loads every row of the status and sorts in memory. A
/// pending queue is exactly the thing an attacker can grow — guest comments are
/// on by default and the throttle bounds the rate, not the total — so the
/// screen needed to *clear* spam was the one that became unusable first.
pub async fn moderation_queue_page(
    conn: &mut AsyncPgConnection,
    status: &str,
    offset: i64,
    limit: i64,
) -> AutumnResult<Vec<Comment>> {
    Ok(comments::table
        .filter(comments::status.eq(status))
        .order((comments::created_at.desc(), comments::id.desc()))
        .offset(offset.max(0))
        .limit(limit.max(0))
        .select(Comment::as_select())
        .load(conn)
        .await?)
}

/// The posts these comments are on, in one query rather than one per comment.
pub async fn posts_for_comments(
    conn: &mut AsyncPgConnection,
    comments: &[Comment],
) -> AutumnResult<std::collections::HashMap<i64, Post>> {
    let ids: Vec<i64> = comments.iter().map(|comment| comment.post_id).collect();
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    Ok(posts::table
        .filter(posts::id.eq_any(ids))
        .select(Post::as_select())
        .load(conn)
        .await?
        .into_iter()
        .map(|post| (post.id, post))
        .collect())
}

/// A flat page of a post's approved comments, oldest first.
///
/// For the REST API, which pages explicitly and returns a list rather than a
/// tree — so a flat window is the right shape there and the caller can reach
/// every comment by asking for the next page. The HTML thread uses
/// `approved_thread_page` instead, because it renders a tree and a flat window
/// would detach replies from roots that fell outside it.
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

/// How many *top-level* comments one page of a thread holds.
///
/// Threads paginate by root, not by row. Taking a flat window of the oldest N
/// comments meant every comment past that window was unreachable for good:
/// there was no page to turn to, and a signed-in commenter was redirected to an
/// anchor that did not exist on the page they landed on. Taking the newest N
/// instead would have detached replies whose parents fell off the front, since
/// `assemble_thread` builds from roots down.
///
/// Paginating by root keeps both properties: every comment is on exactly one
/// page, and every reply is on the page its root is.
pub const THREAD_ROOTS_PER_PAGE: i64 = 50;

/// One page of a post's approved thread.
///
/// The rows are a window of roots plus their approved descendants, the total
/// number of roots so the caller can page, and whether the page hit
/// [`MAX_THREAD_COMMENTS`] so the caller can say so.
pub struct ThreadPage {
    pub comments: Vec<Comment>,
    pub total_roots: i64,
    /// Whether replies were left off because the page filled up.
    pub truncated: bool,
}

/// Load one page of a post's approved thread: a window of roots, plus the
/// approved descendants of those roots that fit in [`MAX_THREAD_COMMENTS`].
///
/// The descendant walk is level by level and bounded by `MAX_COMMENT_DEPTH`,
/// which the write path enforces — so it is a handful of queries whatever the
/// thread looks like. Each level is *also* bounded by what is left of the row
/// budget, because depth is not breadth: one root with fifty thousand direct
/// replies is five queries and fifty thousand rows.
///
/// Truncation happens at a level boundary and takes the oldest rows first, so a
/// comment is never included without its parent — the tree always assembles,
/// and what is dropped is always the tail of the newest level reached.
pub async fn approved_thread_page(
    conn: &mut AsyncPgConnection,
    post_id: i64,
    offset: i64,
    roots_per_page: i64,
) -> AutumnResult<ThreadPage> {
    let total_roots: i64 = comments::table
        .filter(comments::post_id.eq(post_id))
        .filter(comments::status.eq("approved"))
        .filter(comments::parent_id.is_null())
        .count()
        .get_result(conn)
        .await?;

    let mut collected: Vec<Comment> = comments::table
        .filter(comments::post_id.eq(post_id))
        .filter(comments::status.eq("approved"))
        .filter(comments::parent_id.is_null())
        .order((comments::created_at.asc(), comments::id.asc()))
        .offset(offset.max(0))
        .limit(roots_per_page.clamp(0, MAX_THREAD_COMMENTS))
        .select(Comment::as_select())
        .load(conn)
        .await?;

    let mut truncated = false;
    let mut frontier: Vec<i64> = collected.iter().map(|comment| comment.id).collect();
    for _ in 0..=MAX_COMMENT_DEPTH {
        if frontier.is_empty() {
            break;
        }
        let budget = MAX_THREAD_COMMENTS - i64::try_from(collected.len()).unwrap_or(i64::MAX);
        if budget <= 0 {
            truncated = true;
            break;
        }
        // One more than the budget, so a full page is distinguishable from a
        // page that exactly fits.
        let mut children: Vec<Comment> = comments::table
            .filter(comments::status.eq("approved"))
            .filter(comments::parent_id.eq_any(&frontier))
            .order((comments::created_at.asc(), comments::id.asc()))
            .limit(budget + 1)
            .select(Comment::as_select())
            .load(conn)
            .await?;
        if i64::try_from(children.len()).unwrap_or(i64::MAX) > budget {
            children.truncate(usize::try_from(budget).unwrap_or(0));
            truncated = true;
        }
        frontier = children.iter().map(|comment| comment.id).collect();
        collected.extend(children);
        if truncated {
            break;
        }
    }

    // `assemble_thread` expects the rows in creation order.
    collected.sort_by_key(|comment| (comment.created_at, comment.id));
    Ok(ThreadPage {
        comments: collected,
        total_roots,
        truncated,
    })
}

/// A page holds more comments than it holds roots, so a root always fits.
///
/// [`approved_comment_is_rendered`] leans on this: it can answer "yes" for a
/// root without asking the database, because the root window is loaded before
/// the budget applies to anything.
const _: () = assert!(THREAD_ROOTS_PER_PAGE <= MAX_THREAD_COMMENTS);

/// Whether a comment is actually rendered on the page it belongs to.
///
/// Belonging to a page and appearing on it are different questions once a page
/// is capped at [`MAX_THREAD_COMMENTS`]: on a thread past that cap, a newly
/// approved reply can belong to a page that has no room left for it. Promising
/// `#comment-<id>` then sends the browser to an anchor that is not there, which
/// is the same broken promise the root-page redirect was added to fix.
///
/// Two cheap answers come first, because the expensive one should be rare:
/// a root is always rendered (the const above), and no page truncates while the
/// whole post fits in one page's budget. Only past that does this replay the
/// renderer's own page load — the *same* function, so the two cannot disagree
/// about what is on the page.
pub async fn approved_comment_is_rendered(
    conn: &mut AsyncPgConnection,
    comment_id: i64,
    page: i64,
) -> AutumnResult<bool> {
    let Some(comment): Option<Comment> = comments::table
        .find(comment_id)
        .select(Comment::as_select())
        .first(conn)
        .await
        .optional()?
    else {
        return Ok(false);
    };
    if comment.parent_id.is_none() {
        return Ok(true);
    }
    if approved_comment_count(conn, comment.post_id).await? <= MAX_THREAD_COMMENTS {
        return Ok(true);
    }

    let page = approved_thread_page(
        conn,
        comment.post_id,
        (page - 1).max(0) * THREAD_ROOTS_PER_PAGE,
        THREAD_ROOTS_PER_PAGE,
    )
    .await?;
    Ok(page.comments.iter().any(|row| row.id == comment_id))
}

/// Which page of a post's approved thread a comment appears on, if any.
///
/// A comment is rendered on the page its *root* is on, so this walks up to the
/// root and counts the approved roots ordered before it. `None` means the
/// comment has no page: it was deleted underneath us, or its root is not
/// approved.
///
/// The post-a-comment redirect needs this. Sending the browser to the bare
/// permalink lands it on page one, where the `#comment-<id>` anchor it was
/// promised does not exist — the comment is real, approved and simply somewhere
/// else, which reads exactly like a comment that was silently dropped.
pub async fn approved_thread_page_of(
    conn: &mut AsyncPgConnection,
    comment_id: i64,
) -> AutumnResult<Option<i64>> {
    let Some(mut current): Option<Comment> = comments::table
        .find(comment_id)
        .select(Comment::as_select())
        .first(conn)
        .await
        .optional()?
    else {
        return Ok(None);
    };

    // Bounded like `reply_depth`, and for the same reason: a cycle introduced
    // by a bad import must terminate the walk rather than spin.
    let mut steps = 0_usize;
    while let Some(parent_id) = current.parent_id {
        steps += 1;
        if steps > MAX_COMMENT_DEPTH + 2 {
            return Ok(None);
        }
        let Some(parent): Option<Comment> = comments::table
            .find(parent_id)
            .select(Comment::as_select())
            .first(conn)
            .await
            .optional()?
        else {
            return Ok(None);
        };
        current = parent;
    }

    if current.status != "approved" {
        return Ok(None);
    }

    // The same ordering the page window uses, so the count and the offset can
    // never disagree about which page a root is on.
    let earlier: i64 = comments::table
        .filter(comments::post_id.eq(current.post_id))
        .filter(comments::status.eq("approved"))
        .filter(comments::parent_id.is_null())
        .filter(
            comments::created_at
                .lt(current.created_at)
                .or(comments::created_at
                    .eq(current.created_at)
                    .and(comments::id.lt(current.id))),
        )
        .count()
        .get_result(conn)
        .await?;

    Ok(Some(earlier / THREAD_ROOTS_PER_PAGE + 1))
}

/// Rebuild a post's approved-comment counter from ground truth, under its lock.
///
/// The lock is taken before the count for the same reason `recount_term` takes
/// one: two moderators approving different pending comments on one post each
/// count a set that excludes the other's uncommitted change, and both write the
/// same too-low number. Serializing the *snapshot* rather than only the write
/// is what makes the result right.
///
/// Every path that changes which comments are approved goes through here —
/// insertion, moderation and deletion — so there is one definition of the
/// counter rather than three.
async fn recount_post_comments(conn: &mut AsyncPgConnection, post_id: i64) -> AutumnResult<i64> {
    let _locked: Option<i64> = posts::table
        .find(post_id)
        .select(posts::id)
        .for_update()
        .first(conn)
        .await
        .optional()?;

    let approved: i64 = comments::table
        .filter(comments::post_id.eq(post_id))
        .filter(comments::status.eq("approved"))
        .count()
        .get_result(conn)
        .await?;
    diesel::update(posts::table.find(post_id))
        .set(posts::comment_count.eq(approved))
        .execute(conn)
        .await?;
    Ok(approved)
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

/// One page of the media library, newest first, ordered and bounded in SQL.
///
/// The generated `find_all` loads every attachment row and the screen sorted
/// the whole collection in memory. Every account with `UploadFiles` — which is
/// every Author — can grow that table indefinitely, so the screen an editor
/// uses to *manage* their uploads is the one that stops working first, without
/// a single invalid upload having happened.
pub async fn attachments_page(
    conn: &mut AsyncPgConnection,
    offset: i64,
    limit: i64,
) -> AutumnResult<Vec<crate::models::Attachment>> {
    Ok(attachments::table
        .order((attachments::created_at.desc(), attachments::id.desc()))
        .offset(offset.max(0))
        .limit(limit.max(0))
        .select(crate::models::Attachment::as_select())
        .load(conn)
        .await?)
}

/// One page of the images in the library, for a picker that can only use
/// images.
///
/// The featured-image select listed *every* attachment, so an editor could pick
/// a PDF or a CSV, the save would succeed, and the post would render no image
/// at all — the editor said yes and the site said nothing. Filtering in SQL
/// rather than in Rust keeps the page size meaningful: filtering afterwards
/// would show fewer than a page of images while claiming the page was full.
pub async fn images_page(
    conn: &mut AsyncPgConnection,
    offset: i64,
    limit: i64,
) -> AutumnResult<Vec<crate::models::Attachment>> {
    Ok(attachments::table
        .filter(attachments::mime_type.like("image/%"))
        .order((attachments::created_at.desc(), attachments::id.desc()))
        .offset(offset.max(0))
        .limit(limit.max(0))
        .select(crate::models::Attachment::as_select())
        .load(conn)
        .await?)
}

/// How many images the library holds.
pub async fn image_count(conn: &mut AsyncPgConnection) -> AutumnResult<i64> {
    Ok(attachments::table
        .filter(attachments::mime_type.like("image/%"))
        .count()
        .get_result(conn)
        .await?)
}

/// How many rows the media library holds, for its pager.
pub async fn attachment_count(conn: &mut AsyncPgConnection) -> AutumnResult<i64> {
    Ok(attachments::table.count().get_result(conn).await?)
}

/// One bounded batch of scheduled posts whose time has arrived, oldest first.
///
/// Bounded and ordered in SQL. The sweep loaded *every* due post — full bodies
/// and all — before publishing any of them, so a backlog (downtime, or many
/// authors scheduling the same slot) could exhaust the task's memory or run
/// past its next tick and delay every publication behind it. Oldest first so a
/// backlog drains in the order the schedules were meant to fire, rather than
/// starving the earliest posts behind newer ones.
pub async fn due_scheduled_posts(
    conn: &mut AsyncPgConnection,
    limit: i64,
) -> AutumnResult<Vec<Post>> {
    Ok(posts::table
        .filter(posts::status.eq("future"))
        .filter(posts::published_at.le(chrono::Utc::now().naive_utc()))
        .order((posts::published_at.asc(), posts::id.asc()))
        .limit(limit.max(0))
        .select(Post::as_select())
        .load(conn)
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

        // The claim is a locking *read* rather than a blind `UPDATE`, so the
        // row can be snapshotted in the state it is being moved out of. The
        // guard conditions live on this query: a second replica blocks on the
        // row lock, re-evaluates them after the first commits, and matches
        // nothing — which is the same mutual exclusion the conditional update
        // gave, with the pre-transition row in hand.
        let Some(scheduled) = posts::table
            .find(post_id)
            .filter(posts::status.eq("future"))
            .filter(posts::published_at.eq(observed_published_at))
            .filter(posts::published_at.le(now))
            .select(Post::as_select())
            .for_update()
            .first(conn)
            .await
            .optional()?
        else {
            return Ok(false);
        };

        // Snapshotted *before* the status changes, like every other transition:
        // a revision labelled `future → publish` that stores `status = publish`
        // is a record of the wrong moment, and the scheduled state — the one an
        // editor would want to look back at — is then absent from the history
        // entirely.
        if type_supports_revisions(&scheduled.post_type) {
            scheduled
                .record_revision(conn, &format!("Status: future → {new_status}"))
                .await?;
        }

        // The same bookkeeping `transition_status` does, because this *is* a
        // status transition — it only takes a different shape because the sweep
        // needs the claim to be conditional. `lock_version` moves with it, so an
        // editor holding a form rendered before the post went live cannot save
        // over the publication without the stale-edit check noticing.
        diesel::update(posts::table.find(post_id))
            .set((
                posts::status.eq(&new_status),
                posts::updated_at.eq(now),
                posts::lock_version.eq(posts::lock_version + 1),
            ))
            .execute(conn)
            .await?;

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
    use super::{
        Registration, SLUG_COLLISION_INDEXES, probe_paths, reads_as_date_archive, segment_claim,
    };

    /// The namespace answer covers every branch `permalinks::resolve` tries
    /// before it reaches bare post/page content — the whole point of having one
    /// function rather than three partial lists.
    #[test]
    fn segment_claim_names_every_kind_of_owner() {
        // A literal application route.
        assert!(segment_claim("search", None).is_some());
        assert!(segment_claim("feed", None).is_some());
        // A year archive.
        assert!(segment_claim("2026", None).is_some());
        // A built-in taxonomy's rewrite base.
        assert!(segment_claim("category", None).is_some());
        assert!(segment_claim("tag", None).is_some());
        // Nothing owns an ordinary word.
        assert!(segment_claim("about", None).is_none());
        // `post` and `page` share the bare path and claim no prefix, so they
        // must not reserve their own names against content.
        assert!(segment_claim("post", None).is_none());
        assert!(segment_claim("page", None).is_none());
        // A registration is never in conflict with itself…
        assert!(segment_claim("category", Some(Registration::Taxonomy("category"))).is_none());
        // …but an exclusion in one registry must not excuse the other. A
        // taxonomy called `category` does not get to take the `category`
        // taxonomy's base just because the slugs match across registries.
        assert!(segment_claim("category", Some(Registration::PostType("category"))).is_some());
    }

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
    /// A renamed probe still claims its segment.
    ///
    /// All four probe paths are configurable, so the four names in
    /// `RESERVED_PATHS` are a floor rather than the answer: an operator who
    /// sets `health.path = "/healthz"` gets a literal `/healthz` route, which
    /// beats the front controller's wildcard exactly as `/health` does.
    #[test]
    fn a_renamed_probe_path_is_still_claimed() {
        let mut health = autumn_web::config::HealthConfig::default();
        assert_eq!(
            probe_paths(&health),
            vec![
                vec!["health".to_owned()],
                vec!["live".to_owned()],
                vec!["ready".to_owned()],
                vec!["startup".to_owned()],
            ],
            "the defaults are what `RESERVED_PATHS` already carries"
        );

        "/healthz".clone_into(&mut health.path);
        let paths = probe_paths(&health);
        assert!(paths.contains(&vec!["healthz".to_owned()]));
        assert!(
            !paths.contains(&vec!["health".to_owned()]),
            "the renamed path replaces the default rather than adding to it"
        );

        // A nested probe is kept whole. A page is addressed by its full
        // ancestry, so `/internal/probe` is reachable as a root page
        // `internal` with a child `probe` — discarding it (which an earlier
        // version did) left exactly that collision open.
        "/internal/probe".clone_into(&mut health.path);
        let paths = probe_paths(&health);
        assert!(paths.contains(&vec!["internal".to_owned(), "probe".to_owned()]));
        assert!(
            !paths.contains(&vec!["internal".to_owned()]),
            "the prefix alone is not claimed — `/internal` serves nothing"
        );

        // Disabled probes mount nothing and claim nothing.
        health.enabled = false;
        assert!(probe_paths(&health).is_empty());
    }
    /// Every index that can report a slug collision is in the retry list.
    ///
    /// The allocators retry on a *named* index, so an index the list does not
    /// know surfaces its raw constraint error to the caller instead of
    /// allocating the next suffix. `idx_pages_parent_slug` was added when
    /// nested page slugs became per-parent and named in neither allocator —
    /// which is the shape this PR keeps producing, so the list is one constant
    /// now rather than two copies.
    #[test]
    fn every_slug_index_is_a_recognised_collision() {
        let migration = include_str!("../migrations/20260908005714_create_content_schema/up.sql");
        let named: Vec<&str> = SLUG_COLLISION_INDEXES
            .iter()
            .map(|(name, _, _)| *name)
            .collect();
        // Statement-wise, because the `ON posts` clause is usually on its own
        // line — and it is the clause that matters: this list is the *post*
        // allocators' retry set, so a unique slug index on `terms` or `menus`
        // belongs to their own handling. (The first version matched on the
        // index name alone and flagged `idx_terms_taxonomy_slug` immediately,
        // which is how the scope got settled.)
        for statement in migration.split(';') {
            let Some(at) = statement.find("CREATE UNIQUE INDEX ") else {
                continue;
            };
            let rest = &statement[at + "CREATE UNIQUE INDEX ".len()..];
            let words: Vec<&str> = rest.split_whitespace().collect();
            let Some(name) = words.first().copied() else {
                continue;
            };
            let on_posts = words
                .windows(2)
                .any(|pair| pair[0] == "ON" && pair[1].trim_start_matches('(') == "posts");
            if !on_posts || !name.contains("slug") {
                continue;
            }
            assert!(
                named.contains(&name),
                "`{name}` makes a post slug unique but the allocators would not retry on \
                 it; add it to `SLUG_COLLISION_INDEXES`"
            );
        }
    }
    /// A nested path is claimable whatever mints it, not only a page.
    ///
    /// `guard_page_path` covered pages alone, which is the shape this PR keeps
    /// producing: the fix applied where the finding pointed and nowhere else. A
    /// term archive is `{rewrite_base}/{slug}` and a custom type's item is
    /// `{type}/{slug}` — both nested, both able to collide with a configured
    /// probe.
    #[test]
    fn a_claimed_path_is_refused_whatever_mints_it() {
        let claimed = |segments: &[&str]| {
            super::guard_claimed_path(
                &segments.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
                "thing",
            )
            .is_err()
        };

        // Nothing is claimed until a configuration has been observed, which is
        // the store's whole design — so this asserts the *function*, against a
        // path the defaults do claim once seeded.
        // One observation per process — the store is a `OnceLock` by design, so
        // every assertion below shares this configuration. Two probe paths, so
        // both taxonomy rewrite bases are covered by the one seeding.
        super::observe_probe_paths(&{
            let mut config = autumn_web::config::AutumnConfig::default();
            "/category/status".clone_into(&mut config.health.path);
            "/tag/status".clone_into(&mut config.health.live_path);
            // A bare single-segment path too, which is the shape the seeder
            // mints. The store is a `OnceLock`, so this is the one place a
            // configuration can be observed in a test — an integration test
            // cannot set it deterministically, which is why the seeder's guard
            // is asserted here rather than through a seeded site.
            //
            // Deliberately not `/about`, even though that is the page the
            // seeder creates: every test in this process shares this one
            // observation, and `segment_claim_names_every_kind_of_owner`
            // asserts `about` is unclaimed. The property under test is that a
            // bare path a probe holds is refused, not that it is spelled
            // `about`.
            "/probe-only".clone_into(&mut config.health.ready_path);
            config
        });

        assert!(
            claimed(&["category", "status"]),
            "a term archive under a claimed path is unreachable, exactly as a page is"
        );
        assert!(
            !claimed(&["category", "news"]),
            "and an ordinary one is untouched"
        );
        assert!(
            !claimed(&["category"]),
            "the prefix alone serves nothing and is not claimed"
        );

        // Through `guard_term_path`, which is what the three term-creation
        // paths call — the term screen, the importer, and the post editor's tag
        // box. It resolves the taxonomy's *rewrite base*, so `post_tag` has to
        // become `/tag` rather than `/post_tag`.
        assert!(super::guard_term_path("category", "status").is_err());
        assert!(super::guard_term_path("post_tag", "status").is_err());
        assert!(
            super::guard_term_path("post_tag", "rust").is_ok(),
            "an ordinary tag is untouched"
        );
        assert!(
            super::guard_term_path("not_a_taxonomy", "status").is_ok(),
            "an unregistered taxonomy has no base to collide on"
        );

        // The bare single-segment shape the seeder mints. It went around this
        // guard with a direct insert, so a site configured with
        // `health.path = "/about"` published an About page the probe then
        // shadowed — the CMS advertising a URL it does not serve.
        assert!(
            claimed(&["probe-only"]),
            "a seeded page under a claimed bare path is unreachable too"
        );
        assert!(
            !claimed(&["about"]) && !claimed(&["hello-world"]),
            "and the seeded content beside it is untouched"
        );
    }
}
