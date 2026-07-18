// ── v0.2 Feature: #[scheduled] macro ────────────────────────────
//
// Declares a background task that runs every hour alongside the
// HTTP server. Dependencies (AppState) are injected automatically,
// just like handler extractors.
//
// Errors are logged at WARN level and the task retries on the
// next scheduled interval.

use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use autumn_web::http::{Client, ClientError};
use autumn_web::prelude::*;

use crate::schema::bookmarks;

#[allow(clippy::cognitive_complexity)]
#[scheduled(every = "1h", name = "link-checker")]
pub async fn check_links(state: AppState) -> AutumnResult<()> {
    let pool = state
        .pool()
        .ok_or_else(|| AutumnError::service_unavailable_msg("No database pool"))?;

    let mut conn = pool.get().await.map_err(AutumnError::from)?;

    // Load all bookmarks currently marked alive
    let alive: Vec<(i64, String)> = bookmarks::table
        .filter(bookmarks::alive.eq(true))
        .select((bookmarks::id, bookmarks::url))
        .load(&mut conn)
        .await?;

    if alive.is_empty() {
        tracing::info!("link-checker: no alive bookmarks to check");
        return Ok(());
    }

    tracing::info!("link-checker: checking {} URLs", alive.len());

    let client = Client::from_state(&state);

    let mut dead_ids = Vec::new();
    for (id, url) in &alive {
        let reachable = match client.head(url).no_retry().send().await {
            Ok(r) => r.status().is_success() || r.status().is_redirection(),
            Err(ClientError::CircuitBreakerOpen) => {
                tracing::debug!("link-checker: circuit breaker open for {url}, skipping probe");
                continue;
            }
            Err(_) => false,
        };

        if !reachable {
            tracing::warn!("link-checker: dead link id={id} url={url}");
            dead_ids.push(*id);
        }
    }

    let dead_count = dead_ids.len();

    if !dead_ids.is_empty() {
        diesel::update(bookmarks::table.filter(bookmarks::id.eq_any(&dead_ids)))
            .set(bookmarks::alive.eq(false))
            .execute(&mut conn)
            .await?;
    }

    tracing::info!(
        "link-checker: done — {dead_count} dead of {} checked",
        alive.len()
    );
    Ok(())
}
