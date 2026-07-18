// ── v0.2 Feature: #[scheduled] macro ─────────────────────────────────────
//
// Declares a background task that runs every hour alongside the
// HTTP server. Dependencies (AppState) are injected automatically,
// just like handler extractors.
//
// Errors are logged at WARN level and the task retries on the
// next scheduled interval.

use autumn_web::http::{Client, ClientError};
use autumn_web::prelude::*;
use reqwest::StatusCode;

use crate::repositories::BookmarkRepository;

fn response_is_reachable(status: StatusCode) -> bool {
    status.is_success() || status.is_redirection()
}

fn head_requires_get_fallback(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
    )
}

fn probe_outcome(head: Result<StatusCode, ()>, get: Option<Result<StatusCode, ()>>) -> bool {
    match head {
        Ok(status) if response_is_reachable(status) => true,
        Ok(status) if head_requires_get_fallback(status) => {
            get.is_some_and(|fallback| fallback.is_ok_and(response_is_reachable))
        }
        _ => false,
    }
}

async fn probe_reachable(client: &Client, url: &str) -> bool {
    let head_result = client.head(url).no_retry().send().await;
    let head = match head_result {
        Err(ClientError::CircuitBreakerOpen) => return true, // inconclusive — don't mark dead
        other => other.map(|r| r.status()),
    };
    match head {
        Ok(status) if response_is_reachable(status) => true,
        Ok(status) if head_requires_get_fallback(status) => {
            let get_result = client.get(url).no_retry().send().await;
            let get = match get_result {
                Err(ClientError::CircuitBreakerOpen) => return true, // inconclusive
                other => other.map(|r| r.status()),
            };
            probe_outcome(Ok(status), Some(get.map_err(|_| ())))
        }
        Ok(status) => probe_outcome(Ok(status), None),
        Err(_) => false,
    }
}

async fn process_shard(
    repo: &BookmarkRepository,
    client: &Client,
    shard: u32,
) -> AutumnResult<(u32, u32)> {
    let shard_alive = repo.find_alive_in_shard(shard).await?;

    if shard_alive.is_empty() {
        return Ok((0, 0));
    }
    let shard_checked_count =
        u32::try_from(shard_alive.len()).expect("shard bookmark count must fit in u32");

    tracing::info!(shard, count = shard_alive.len(), "link-checker owns shard");

    let mut dead_count = 0u32;
    for (id, url) in shard_alive {
        let reachable = probe_reachable(client, &url).await;

        if !reachable {
            tracing::warn!("link-checker: dead link id={id} url={url}");
            if repo.mark_dead(id).await? {
                dead_count += 1;
            }
        }
    }

    Ok((shard_checked_count, dead_count))
}

#[scheduled(every = "1h", name = "link-checker")]
pub async fn check_links(state: AppState) -> AutumnResult<()> {
    let repo = BookmarkRepository;
    let client = Client::from_state(&state);

    let mut dead_count = 0u32;
    let mut checked_count = 0u32;
    let mut owned_shards = 0u32;

    for shard in BookmarkRepository::shard_ids() {
        // `try_with` runs the section on exactly one replica: it skips the shard
        // (returns `None`) when another replica holds the lock, and releases the
        // lock automatically when `process_shard` finishes — on normal return,
        // an early `?`, or a panic (the guard closes its session as it unwinds,
        // so no lock leaks). This replaces the hand-rolled `pg_try_advisory_lock`
        // / `pg_advisory_unlock` dance the example used to carry.
        let outcome = BookmarkRepository::shard_lock(shard)?
            .try_with(|| process_shard(&repo, &client, shard))
            .await?;

        let Some(shard_result) = outcome else {
            tracing::debug!(shard, "link-checker shard already owned by another replica");
            continue;
        };

        let (shard_checked_count, shard_dead_count) = shard_result?;
        owned_shards += 1;
        dead_count += shard_dead_count;
        checked_count += shard_checked_count;
    }

    tracing::info!(
        owned_shards,
        dead_count,
        checked = checked_count,
        "link-checker done"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{head_requires_get_fallback, probe_outcome, response_is_reachable};
    use reqwest::StatusCode;

    #[test]
    fn reachable_statuses_match_link_checker_expectation() {
        assert!(response_is_reachable(StatusCode::OK));
        assert!(response_is_reachable(StatusCode::MOVED_PERMANENTLY));
        assert!(!response_is_reachable(StatusCode::NOT_FOUND));
    }

    #[test]
    fn head_fallback_is_limited_to_head_unsupported_statuses() {
        assert!(head_requires_get_fallback(StatusCode::METHOD_NOT_ALLOWED));
        assert!(head_requires_get_fallback(StatusCode::NOT_IMPLEMENTED));
        assert!(!head_requires_get_fallback(StatusCode::NOT_FOUND));
        assert!(!head_requires_get_fallback(StatusCode::FORBIDDEN));
    }

    #[test]
    fn successful_head_probe_marks_link_reachable() {
        assert!(probe_outcome(Ok(StatusCode::OK), None));
    }

    #[test]
    fn head_405_falls_back_to_get_before_marking_dead() {
        assert!(probe_outcome(
            Ok(StatusCode::METHOD_NOT_ALLOWED),
            Some(Ok(StatusCode::OK))
        ));
        assert!(!probe_outcome(
            Ok(StatusCode::METHOD_NOT_ALLOWED),
            Some(Ok(StatusCode::NOT_FOUND))
        ));
    }

    #[test]
    fn hard_head_failures_do_not_trigger_fallback() {
        assert!(!probe_outcome(Ok(StatusCode::NOT_FOUND), None));
        assert!(!probe_outcome(Err(()), None));
    }
}
