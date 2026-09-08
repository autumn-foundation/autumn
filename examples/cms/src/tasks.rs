//! Scheduled work — the equivalent of wp-cron, without wp-cron's defect.
//!
//! WordPress's scheduler is driven by visitor traffic: `wp-cron.php` fires on
//! page loads, so a site with no visitors never publishes its scheduled posts,
//! and a busy site fires the scheduler on requests that are then slower for it.
//! `#[scheduled]` is a real timer owned by the process, so a post scheduled for
//! 3am publishes at 3am on a site nobody visits at 3am.

use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::Post;
use crate::plugins::{Action, do_action};
use crate::schema::posts;

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

    let now = chrono::Utc::now().naive_utc();
    let due: Vec<Post> = posts::table
        .filter(posts::status.eq("future"))
        .filter(posts::published_at.le(now))
        .select(Post::as_select())
        .load(&mut conn)
        .await?;

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
        let updated = diesel::update(
            posts::table
                .find(post.id)
                .filter(posts::status.eq("future"))
                .filter(posts::published_at.eq(post.published_at))
                .filter(posts::published_at.le(now)),
        )
        .set((
            posts::status.eq(&new_status),
            posts::updated_at.eq(chrono::Utc::now().naive_utc()),
        ))
        .execute(&mut conn)
        .await?;

        if updated > 0 {
            // The post was `future` when its terms were last counted, so every
            // term it is filed under excluded it. This is the moment it became
            // public, and the guarded `UPDATE` above is deliberately not
            // `transition_status` (which recounts), so recount here or every
            // affected archive shows a count one short of what it lists.
            if let Err(error) =
                crate::content::recount_terms_for_post_public(&mut conn, post.id).await
            {
                autumn_web::reexports::tracing::warn!(
                    post_id = post.id,
                    %error,
                    "published a scheduled post but could not rebuild its term counts"
                );
            }
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
