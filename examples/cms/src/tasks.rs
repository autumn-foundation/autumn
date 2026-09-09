//! Scheduled work — the equivalent of wp-cron, without wp-cron's defect.
//!
//! WordPress's scheduler is driven by visitor traffic: `wp-cron.php` fires on
//! page loads, so a site with no visitors never publishes its scheduled posts,
//! and a busy site fires the scheduler on requests that are then slower for it.
//! `#[scheduled]` is a real timer owned by the process, so a post scheduled for
//! 3am publishes at 3am on a site nobody visits at 3am.

use autumn_web::prelude::*;

use crate::plugins::{Action, do_action};

/// How many scheduled posts one sweep publishes.
///
/// Generous enough that an ordinary site never notices — a backlog this large
/// means something was down — and small enough that the task's memory and
/// runtime are bounded by the constant rather than by the backlog.
pub const PUBLISH_BATCH: i64 = 100;

/// Publish posts whose scheduled time has arrived.
///
/// Runs every minute: a scheduled post should go live within a minute of its
/// stated time, and the query is an index-backed lookup on
/// `(status, published_at)` that finds nothing almost every time it runs.
#[scheduled(every = "1m", name = "publish-scheduled-posts")]
pub async fn publish_scheduled(state: AppState) -> AutumnResult<()> {
    let pool = state
        .pool()
        .ok_or_else(|| AutumnError::service_unavailable_msg("No database pool"))?;
    let mut conn = pool.get().await.map_err(AutumnError::from)?;

    // One bounded batch per tick — see `content::due_scheduled_posts`. The
    // remainder is drained by the following runs, which is what a sweep that
    // fires every minute is for.
    let due = crate::content::due_scheduled_posts(&mut conn, PUBLISH_BATCH).await?;

    if due.is_empty() {
        return Ok(());
    }

    for post in &due {
        // Drive the state machine rather than writing `status = 'publish'`
        // directly: the `future -> publish` edge carries the `can_publish`
        // guard, so a scheduled post whose title was emptied after it was
        // scheduled is left alone instead of going live broken.
        let new_status = match post.transition_status_to("publish") {
            Ok(status) => status,
            Err(error) => {
                autumn_web::reexports::tracing::warn!(
                    post_id = post.id,
                    %error,
                    "scheduled post could not be published; leaving it scheduled"
                );
                continue;
            }
        };

        // Guarded so two replicas racing this sweep cannot both claim the same
        // post: the second `UPDATE` matches no rows. The framework also offers
        // `#[scheduled(cluster = ...)]` leader election, but a conditional
        // update is cheaper and needs no coordination.
        //
        // The guard re-checks `published_at` as well as the status, and against
        // the value observed by the SELECT. Guarding on status alone would let
        // an editor who reschedules a post to a later date — between the query
        // and this update, leaving it `future` — have it published early
        // anyway, which is the one thing scheduling is for.
        // The status change and the term recount commit together.
        //
        // Returning the recount error was not enough — the previous attempt at
        // this. Once the row is `publish`, no later sweep selects it (the query
        // above filters `status = 'future'`), so there is nothing left to retry
        // and the counts stay wrong permanently while the task reports failure.
        // Only atomicity actually fixes it: if the recount fails, the
        // publication rolls back with it and the next sweep sees the post as
        // `future` again and tries the whole thing afresh.
        //
        // The `UPDATE` is guarded on the status and on the `published_at` the
        // query observed, so two replicas racing this sweep cannot both claim
        // the post, and an editor who reschedules in between prevents it.
        let updated =
            crate::content::publish_due_post(&mut conn, post.id, post.published_at, &new_status)
                .await?;

        if updated {
            autumn_web::reexports::tracing::info!(
                post_id = post.id,
                slug = %post.slug,
                "published scheduled post"
            );
            do_action(Action::PostTransitioned, post.id);
        }
    }

    Ok(())
}
