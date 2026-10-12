//! The vote rule, with no database in it.
//!
//! [`cast`] records a vote and publishes the new score to live subscribers.
//! The guarantee: subscribers never get an older score after a newer one.
//!
//! [`crate::routes::votes`] uses it with Postgres. The DST nightly
//! (`tests/sim_nightly.rs`) uses it with an in-memory store, and races many
//! votes against it for each seed.

use std::future::Future;

use autumn_web::AutumnResult;

/// The state of one vote after a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteState {
    /// The score of the post.
    pub aggregate: i64,
    /// The vote of the viewer: `Some(1)`, `Some(-1)`, or `None` after a
    /// toggle-off.
    pub value: Option<i16>,
}

/// The storage and fan-out that [`cast`] uses.
pub trait VoteBackend {
    /// Toggle, flip or insert the vote of `user_id` on `post_id`, and commit.
    fn react(
        &self,
        user_id: i64,
        post_id: i64,
        value: i16,
    ) -> impl Future<Output = AutumnResult<VoteState>> + Send;

    /// Load the score of `post_id` after the commit, publish it to live
    /// subscribers, and return it.
    fn reload_and_publish(&self, post_id: i64) -> impl Future<Output = AutumnResult<i64>> + Send;
}

/// Makes the publish order equal to the load order.
///
/// The row lock of `react` ends at commit. Without this lock, two votes can
/// load in one order and publish in the other, so the older score arrives
/// last. Each load comes after its own commit, so with this lock the
/// published score never goes back.
///
/// The lock is per process. Two replicas each keep their own order.
#[derive(Debug, Default)]
pub struct PublishOrder(tokio::sync::Mutex<()>);

impl PublishOrder {
    /// A new, unlocked order.
    #[must_use]
    pub const fn new() -> Self {
        Self(tokio::sync::Mutex::const_new(()))
    }

    /// Run `load_and_publish` while this process holds the lock.
    pub async fn run<F: Future>(&self, load_and_publish: F) -> F::Output {
        let _in_load_order = self.0.lock().await;
        load_and_publish.await
    }
}

/// Record a vote and publish the new score.
///
/// The fan-out is best effort: the vote is already committed. A failed
/// fan-out does not fail the request, because a retried toggle undoes the
/// vote. The viewer then gets the score of their own commit.
///
/// # Errors
///
/// Returns the error of [`VoteBackend::react`].
pub async fn cast<B: VoteBackend + Sync>(
    backend: &B,
    order: &PublishOrder,
    user_id: i64,
    post_id: i64,
    value: i16,
) -> AutumnResult<VoteState> {
    let committed = backend.react(user_id, post_id, value).await?;
    let aggregate = match order.run(backend.reload_and_publish(post_id)).await {
        Ok(score) => score,
        Err(error) => {
            tracing::warn!(
                target: "reddit_clone::routes::votes",
                post_id,
                error = %error,
                "vote committed but SSE fan-out failed"
            );
            committed.aggregate
        }
    };
    Ok(VoteState {
        aggregate,
        value: committed.value,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use autumn_web::AutumnError;

    use super::*;

    /// One post. Each vote adds `value`. A fan-out fails when `fail` is set.
    #[derive(Default)]
    struct Fake {
        score: Mutex<i64>,
        published: Mutex<Vec<i64>>,
        fail: bool,
    }

    impl VoteBackend for Fake {
        async fn react(&self, _user: i64, _post: i64, value: i16) -> AutumnResult<VoteState> {
            let mut score = self.score.lock().unwrap();
            *score += i64::from(value);
            Ok(VoteState {
                aggregate: *score,
                value: Some(value),
            })
        }

        async fn reload_and_publish(&self, _post: i64) -> AutumnResult<i64> {
            if self.fail {
                return Err(AutumnError::service_unavailable_msg("fan-out down"));
            }
            let score = *self.score.lock().unwrap();
            self.published.lock().unwrap().push(score);
            Ok(score)
        }
    }

    /// The sweep races `cast`, not the route. This check ties the route to
    /// `cast` with one order per process. A new `PublishOrder` per request
    /// compiles, but it orders nothing.
    #[test]
    fn the_vote_route_uses_cast_with_the_process_order() {
        let route = include_str!("routes/votes.rs");
        assert!(route.contains("crate::vote_core::cast(&backend, &BROADCAST_ORDER,"));
        assert!(route.contains("static BROADCAST_ORDER: crate::vote_core::PublishOrder"));
        assert_eq!(
            route.matches("PublishOrder::new()").count(),
            1,
            "only the static"
        );
    }

    #[tokio::test]
    async fn cast_returns_the_published_score() {
        let fake = Fake::default();
        let state = cast(&fake, &PublishOrder::new(), 1, 1, 1).await.unwrap();
        assert_eq!(
            state,
            VoteState {
                aggregate: 1,
                value: Some(1)
            }
        );
        assert_eq!(*fake.published.lock().unwrap(), vec![1]);
    }

    #[tokio::test]
    async fn a_failed_fan_out_returns_the_committed_score() {
        let fake = Fake {
            fail: true,
            ..Fake::default()
        };
        let state = cast(&fake, &PublishOrder::new(), 1, 1, -1).await.unwrap();
        assert_eq!(state.aggregate, -1);
        assert!(fake.published.lock().unwrap().is_empty());
    }
}
