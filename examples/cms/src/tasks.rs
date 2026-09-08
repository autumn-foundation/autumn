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

        // Guarded on `status` so two replicas racing this sweep cannot both
        // claim the same post: the second `UPDATE` matches no rows. The
        // framework also offers `#[scheduled(cluster = ...)]` leader election
        // for this, but a conditional update is cheaper and needs no
        // coordination.
        let updated = diesel::update(
            posts::table
                .find(post.id)
                .filter(posts::status.eq("future")),
        )
        .set((
            posts::status.eq(&new_status),
            posts::updated_at.eq(chrono::Utc::now().naive_utc()),
        ))
        .execute(&mut conn)
        .await?;

        if updated > 0 {
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
