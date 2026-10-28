//! The in-memory store of the nightly: the database of all apps of one seed.
//!
//! Each seed makes a new store. All apps of a seed (the main app, the
//! replicas, a restarted app) share it, as processes share a database. It
//! lives longer than a crashed app, so it acts as the durable state.
//!
//! The vote write keeps the `#[votable]` contract of `Post`: toggle, flip or
//! insert, then the score is the sum of the votes. Each commit adds one to the
//! post version, and the history keeps the score of each version, so a check
//! can tell an older score from a newer one.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use chrono::NaiveDateTime;

use reddit_clone::vote_core::VoteState;

/// One post row.
#[derive(Debug, Clone, PartialEq)]
pub struct PostRow {
    pub id: i64,
    pub author_id: i64,
    pub title: String,
    pub slug: String,
    pub created_at: NaiveDateTime,
    pub score: i64,
    pub version: u64,
    pub hot_rank: f64,
    /// How many times the rank job ran for this post.
    pub rank_runs: u32,
    /// The rank job finished at least once.
    pub ranked: bool,
    /// The auto-mod verdict. Set once.
    pub verdict: Option<String>,
    /// How many times the verdict write was tried. More than one means a
    /// duplicate or a retry, and the write must stay idempotent.
    pub verdict_writes: u32,
    /// The webhook receiver has this post.
    pub hook_delivered: bool,
}

/// One user row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserRow {
    pub welcome_sent: bool,
    pub welcome_failures: u32,
}

/// The tables.
#[derive(Debug, Default)]
pub struct Tables {
    pub next_post_id: i64,
    pub posts: BTreeMap<i64, PostRow>,
    /// `(post_id, user_id) -> value`.
    pub votes: BTreeMap<(i64, i64), i16>,
    /// `(post_id, version) -> score`.
    pub history: BTreeMap<(i64, u64), i64>,
    pub users: BTreeMap<i64, UserRow>,
    /// `post_id -> deliveries` at the webhook receiver.
    pub hooks_received: BTreeMap<i64, u32>,
    /// `post_id -> failed vote fan-outs`.
    pub fanout_failures: BTreeMap<i64, u32>,
    /// How many times the scheduled rank sweep ran.
    pub sweep_runs: u32,
}

/// The store. Each method locks it for one short, synchronous step.
#[derive(Debug, Default)]
pub struct Store(Mutex<Tables>);

impl Store {
    /// Run `step` with the tables locked.
    pub fn with<R>(&self, step: impl FnOnce(&mut Tables) -> R) -> R {
        step(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Insert a post at version 0 and return its id.
    pub fn create_post(&self, author_id: i64, title: &str, slug: &str, at: NaiveDateTime) -> i64 {
        self.with(|t| {
            t.next_post_id += 1;
            let id = t.next_post_id;
            t.posts.insert(
                id,
                PostRow {
                    id,
                    author_id,
                    title: title.to_owned(),
                    slug: slug.to_owned(),
                    created_at: at,
                    score: 0,
                    version: 0,
                    hot_rank: 0.0,
                    rank_runs: 0,
                    ranked: false,
                    verdict: None,
                    verdict_writes: 0,
                    hook_delivered: false,
                },
            );
            t.history.insert((id, 0), 0);
            id
        })
    }

    /// Toggle, flip or insert one vote, and commit. Returns the state and the
    /// new version, or `None` for a missing post.
    pub fn react(&self, user_id: i64, post_id: i64, value: i16) -> Option<(VoteState, u64)> {
        self.with(|t| {
            if !t.posts.contains_key(&post_id) {
                return None;
            }
            let key = (post_id, user_id);
            let new_value = match t.votes.get(&key) {
                Some(current) if *current == value => None,
                _ => Some(value),
            };
            match new_value {
                Some(v) => t.votes.insert(key, v),
                None => t.votes.remove(&key),
            };
            let score = t
                .votes
                .range((post_id, i64::MIN)..=(post_id, i64::MAX))
                .map(|(_, v)| i64::from(*v))
                .sum();
            let post = t.posts.get_mut(&post_id).expect("checked above");
            post.score = score;
            post.version += 1;
            let version = post.version;
            t.history.insert((post_id, version), score);
            Some((
                VoteState {
                    aggregate: score,
                    value: new_value,
                },
                version,
            ))
        })
    }

    /// The current `(version, score)` of a post.
    pub fn load(&self, post_id: i64) -> Option<(u64, i64)> {
        self.with(|t| t.posts.get(&post_id).map(|p| (p.version, p.score)))
    }

    /// The score of a post at `version`.
    pub fn score_at(&self, post_id: i64, version: u64) -> Option<i64> {
        self.with(|t| t.history.get(&(post_id, version)).copied())
    }

    /// A copy of all posts, in id order.
    pub fn posts(&self) -> Vec<PostRow> {
        self.with(|t| t.posts.values().cloned().collect())
    }

    /// Check the table rules. Returns the first broken rule.
    pub fn check(&self) -> Result<(), String> {
        self.with(|t| {
            for post in t.posts.values() {
                let sum: i64 = t
                    .votes
                    .range((post.id, i64::MIN)..=(post.id, i64::MAX))
                    .map(|(_, v)| i64::from(*v))
                    .sum();
                if sum != post.score {
                    return Err(format!(
                        "post {} has score {} but its votes sum to {sum}",
                        post.id, post.score
                    ));
                }
            }
            if let Some(((post, user), value)) =
                t.votes.iter().find(|(_, v)| !matches!(**v, 1 | -1))
            {
                return Err(format!(
                    "vote of user {user} on post {post} has value {value}"
                ));
            }
            Ok(())
        })
    }
}
