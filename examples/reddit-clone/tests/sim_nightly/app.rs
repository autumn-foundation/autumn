//! The app that each seed mounts.
//!
//! The routes, jobs and the scheduled task wrap real reddit-clone code over
//! the in-memory [`Store`]:
//!
//! - the vote rule `reddit_clone::vote_core::cast`,
//! - the rank formula `reddit_clone::tasks::calculate_hot_rank`,
//! - the post policy `reddit_clone::policies::PostPolicy`,
//! - the feed experiment `reddit_clone::experiments::setup`,
//! - the summary text `reddit_clone::seo::summarize`.
//!
//! Each step that is a database round trip in the real app yields a seeded
//! number of times. Thus the races of the real app can occur here, and the
//! seed selects which race occurs.

use std::sync::Arc;

use autumn_web::app::AppBuilder;
use autumn_web::authorization::{Policy, PolicyContext, PolicyRegistry};
use autumn_web::entropy::Rng;
use autumn_web::experiments::ExperimentService;
use autumn_web::extract::Path;
use autumn_web::http_client::Client;
use autumn_web::job::JobClient;
use autumn_web::mail::{Mail, Mailer};
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::session::Session;
use autumn_web::sim::llm::{LlmClient, LlmRequest, SeededLlm};
use autumn_web::time::Clock;
use serde::{Deserialize, Serialize};

use reddit_clone::policies::PostPolicy;
use reddit_clone::tasks::calculate_hot_rank;
use reddit_clone::vote_core::{self, PublishOrder, VoteBackend, VoteState};

use super::scenario::Variant;
use super::store::{PostRow, Store};

/// The broadcast topic of vote updates.
pub const POSTS_TOPIC: &str = "posts";

/// The host name of the webhook receiver on `SimNet`.
pub const HOOKS_HOST: &str = "hooks";

/// The jobs that each new post needs.
pub const POST_JOBS: [&str; 3] = ["rank_post", "auto_mod", "notify_hooks"];

/// The state that all apps of one seed share: the database and the services
/// with their own store.
pub struct Shared {
    pub store: Arc<Store>,
    pub llm: SeededLlm,
    pub experiments: ExperimentService,
    pub variant: Variant,
    /// A vote fan-out fails for one draw in this many.
    pub fanout_fail_one_in: u64,
}

/// The state of one app (one process).
pub struct Reddit {
    pub shared: Arc<Shared>,
    /// Per process, as the static in `routes::votes` is.
    pub order: PublishOrder,
}

/// Mounts the routes, and for the main app the jobs and the task.
pub struct RedditPlugin {
    pub shared: Arc<Shared>,
    pub main: bool,
}

impl Plugin for RedditPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        let reddit = Reddit {
            shared: self.shared,
            order: PublishOrder::new(),
        };
        let app = app
            .routes(routes![vote, hot, feed, edit])
            .state_initializer(move |state| state.insert_extension(reddit));
        if self.main {
            app.routes(routes![submit, signup])
                .jobs(jobs![rank_post, auto_mod, notify_hooks])
                .tasks(tasks![rank_sweep])
        } else {
            app
        }
    }
}

fn reddit(state: &AppState) -> Arc<Reddit> {
    state
        .extension::<Reddit>()
        .expect("RedditPlugin inserts the Reddit state")
}

/// Yield a seeded number of times (0 to 2), as a database round trip does.
async fn round_trip(state: &AppState) {
    for _ in 0..state.entropy().next_u64() % 3 {
        tokio::task::yield_now().await;
    }
}

/// The vote backend over the store.
struct SimVotes<'a> {
    reddit: &'a Reddit,
    state: &'a AppState,
}

impl VoteBackend for SimVotes<'_> {
    async fn react(&self, user_id: i64, post_id: i64, value: i16) -> AutumnResult<VoteState> {
        round_trip(self.state).await;
        let (voted, version) = self
            .reddit
            .shared
            .store
            .react(user_id, post_id, value)
            .ok_or_else(|| AutumnError::not_found_msg("no such post"))?;
        tracing::debug!(post_id, user_id, version, "vote committed");
        Ok(voted)
    }

    async fn reload_and_publish(&self, post_id: i64) -> AutumnResult<i64> {
        round_trip(self.state).await;
        let (version, score) = self
            .reddit
            .shared
            .store
            .load(post_id)
            .ok_or_else(|| AutumnError::not_found_msg("no such post"))?;
        // The real handler loads two more rows and renders a fragment here.
        round_trip(self.state).await;
        if self
            .state
            .entropy()
            .next_u64()
            .is_multiple_of(self.reddit.shared.fanout_fail_one_in)
        {
            self.reddit
                .shared
                .store
                .with(|t| *t.fanout_failures.entry(post_id).or_default() += 1);
            return Err(AutumnError::service_unavailable_msg("fan-out is down"));
        }
        let _ = self
            .state
            .broadcast()
            .publish(POSTS_TOPIC, format!("{post_id} {version} {score}"));
        Ok(score)
    }
}

/// Cast with the vote rule of `variant`. The planted bug is the realistic
/// regression: a new `PublishOrder` for each request, which orders nothing.
async fn cast_with<B: VoteBackend + Sync>(
    variant: Variant,
    backend: &B,
    order: &PublishOrder,
    user_id: i64,
    post_id: i64,
    value: i16,
) -> AutumnResult<VoteState> {
    match variant {
        Variant::Correct => vote_core::cast(backend, order, user_id, post_id, value).await,
        Variant::UnorderedPublish => {
            vote_core::cast(backend, &PublishOrder::new(), user_id, post_id, value).await
        }
    }
}

/// A vote worker outside the HTTP apps: a third process that writes votes
/// to the shared store. `Sim::spawn` runs it as a task.
pub struct Worker {
    pub shared: Arc<Shared>,
    pub order: PublishOrder,
    pub entropy: autumn_web::entropy::SeededEntropy,
    /// The payloads this worker published, in order.
    pub published: std::sync::Mutex<Vec<String>>,
}

impl Worker {
    async fn round_trip(&self) {
        use autumn_web::entropy::Entropy as _;
        for _ in 0..self.entropy.next_u64() % 3 {
            tokio::task::yield_now().await;
        }
    }

    /// Cast one vote with the rule of the variant.
    pub async fn cast(&self, user_id: i64, post_id: i64, value: i16) -> AutumnResult<VoteState> {
        cast_with(
            self.shared.variant,
            self,
            &self.order,
            user_id,
            post_id,
            value,
        )
        .await
    }
}

impl VoteBackend for Worker {
    async fn react(&self, user_id: i64, post_id: i64, value: i16) -> AutumnResult<VoteState> {
        self.round_trip().await;
        let (voted, _) = self
            .shared
            .store
            .react(user_id, post_id, value)
            .ok_or_else(|| AutumnError::not_found_msg("no such post"))?;
        Ok(voted)
    }

    async fn reload_and_publish(&self, post_id: i64) -> AutumnResult<i64> {
        self.round_trip().await;
        let (version, score) = self
            .shared
            .store
            .load(post_id)
            .ok_or_else(|| AutumnError::not_found_msg("no such post"))?;
        self.round_trip().await;
        self.published
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("{post_id} {version} {score}"));
        Ok(score)
    }
}

/// Cast a vote. The body is `score value`, with value `0` after a
/// toggle-off.
#[post("/posts/{post_id}/vote/{user_id}/{value}")]
async fn vote(
    Path((post_id, user_id, value)): Path<(i64, i64, i16)>,
    State(state): State<AppState>,
) -> AutumnResult<String> {
    let reddit = reddit(&state);
    let backend = SimVotes {
        reddit: &reddit,
        state: &state,
    };
    let voted = cast_with(
        reddit.shared.variant,
        &backend,
        &reddit.order,
        user_id,
        post_id,
        value,
    )
    .await?;
    Ok(format!("{} {}", voted.aggregate, voted.value.unwrap_or(0)))
}

/// One front page row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HotRow {
    pub id: i64,
    pub score: i64,
    pub created_at: chrono::NaiveDateTime,
    pub rank: f64,
}

/// The front page, ranked at the clock of this app.
#[get("/hot")]
async fn hot(clock: Clock, State(state): State<AppState>) -> Json<Vec<HotRow>> {
    let now = clock.now().naive_utc();
    let mut rows: Vec<HotRow> = reddit(&state)
        .shared
        .store
        .posts()
        .into_iter()
        .map(|post| HotRow {
            id: post.id,
            score: post.score,
            created_at: post.created_at,
            rank: calculate_hot_rank(post.score, post.created_at, now),
        })
        .collect();
    rows.sort_by(|a, b| b.rank.total_cmp(&a.rank).then(a.id.cmp(&b.id)));
    Json(rows)
}

/// The feed layout variant of `actor`.
#[get("/feed/{actor}")]
async fn feed(Path(actor): Path<String>, State(state): State<AppState>) -> AutumnResult<String> {
    reddit(&state)
        .shared
        .experiments
        .assign("feed_layout", &actor)
        .map_err(|error| AutumnError::internal_server_error_msg(error.to_string()))
}

/// Can `actor_id` edit `post_id`? The body is `allowed` or `denied`.
#[put("/posts/{post_id}/edit/{actor_id}/{admin}")]
async fn edit(
    Path((post_id, actor_id, admin)): Path<(i64, i64, bool)>,
    State(state): State<AppState>,
) -> AutumnResult<&'static str> {
    let row = reddit(&state)
        .shared
        .store
        .with(|t| t.posts.get(&post_id).cloned())
        .ok_or_else(|| AutumnError::not_found_msg("no such post"))?;
    let ctx = PolicyContext {
        session: Session::new_for_test(String::new(), std::collections::HashMap::new()),
        user_id: Some(actor_id.to_string()),
        roles: if admin {
            vec!["admin".to_owned()]
        } else {
            Vec::new()
        },
        scopes: Vec::new(),
        pool: None,
        policy_registry: PolicyRegistry::default(),
    };
    let allowed = PostPolicy.can_update(&ctx, &model(&row)).await;
    Ok(if allowed { "allowed" } else { "denied" })
}

/// The `Post` model of reddit-clone for a store row.
fn model(row: &PostRow) -> reddit_clone::models::Post {
    reddit_clone::models::Post {
        id: row.id,
        title: row.title.clone(),
        slug: row.slug.clone(),
        body: String::new(),
        url: None,
        author_id: row.author_id,
        subreddit_id: 1,
        score: row.score,
        hot_rank: row.hot_rank,
        comment_count: 0,
        created_at: row.created_at,
        updated_at: row.created_at,
    }
}

/// Submit a post. The body is the new post id.
///
/// In the real app the jobs go into the database in the post transaction.
/// Here the local job queue holds them, so a crash can lose them. The
/// scheduled `rank_sweep` then enqueues them again, as an outbox relay does.
#[post("/posts/new/{author_id}")]
async fn submit(
    Path(author_id): Path<i64>,
    clock: Clock,
    rng: Rng,
    State(state): State<AppState>,
) -> AutumnResult<String> {
    let shared = Arc::clone(&reddit(&state).shared);
    let title = format!("A post by user {author_id}: what do you think?");
    let token = rng.uuid_v4().simple().to_string();
    let slug = format!("post-by-{author_id}-{}", &token[..8]);
    round_trip(&state).await;
    let id = shared
        .store
        .create_post(author_id, &title, &slug, clock.now().naive_utc());
    let jobs = state
        .extension::<JobClient>()
        .expect("the local job runtime installs a JobClient");
    for job in POST_JOBS {
        jobs.enqueue(job, serde_json::json!({ "post_id": id }))
            .await?;
    }
    Ok(id.to_string())
}

/// Sign up and send the welcome mail. The body is `sent` or `failed`.
#[post("/signup/{user_id}")]
async fn signup(
    Path(user_id): Path<i64>,
    mailer: Mailer,
    State(state): State<AppState>,
) -> AutumnResult<&'static str> {
    let mail = Mail::builder()
        .to(format!("user{user_id}@example.com"))
        .subject("Welcome to reddit-clone")
        .text("Thanks for joining.")
        .build()
        .map_err(|error| AutumnError::internal_server_error_msg(error.to_string()))?;
    let sent = mailer.send(mail).await.is_ok();
    reddit(&state).shared.store.with(|t| {
        let user = t.users.entry(user_id).or_default();
        if sent {
            user.welcome_sent = true;
        } else {
            user.welcome_failures += 1;
        }
    });
    Ok(if sent { "sent" } else { "failed" })
}

/// The payload of the post jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostArgs {
    pub post_id: i64,
}

/// Rank one post with the real formula.
#[job(name = "rank_post", max_attempts = 8, backoff_ms = 1000)]
async fn rank_post(state: AppState, args: PostArgs) -> AutumnResult<()> {
    let now = state.clock().now().naive_utc();
    reddit(&state).shared.store.with(|t| {
        if let Some(post) = t.posts.get_mut(&args.post_id) {
            post.hot_rank = calculate_hot_rank(post.score, post.created_at, now);
            post.rank_runs += 1;
            post.ranked = true;
        }
    });
    tracing::debug!(post_id = args.post_id, "rank_post: ranked");
    Ok(())
}

/// Ask the LLM stub for a verdict. A fault fails the job, and the job retries.
#[job(name = "auto_mod", max_attempts = 8, backoff_ms = 1000)]
async fn auto_mod(state: AppState, args: PostArgs) -> AutumnResult<()> {
    let shared = Arc::clone(&reddit(&state).shared);
    let Some(title) = shared
        .store
        .with(|t| t.posts.get(&args.post_id).map(|p| p.title.clone()))
    else {
        return Ok(());
    };
    let summary = reddit_clone::seo::summarize(&title, 24).unwrap_or_default();
    let reply = shared
        .llm
        .complete(LlmRequest::new("mod-model", format!("moderate: {summary}")))
        .await
        .map_err(|error| {
            tracing::debug!(post_id = args.post_id, %error, "auto_mod: the LLM failed");
            AutumnError::service_unavailable_msg(error.to_string())
        })?;
    tracing::debug!(post_id = args.post_id, verdict = %reply.text, "auto_mod: verdict");
    shared.store.with(|t| {
        if let Some(post) = t.posts.get_mut(&args.post_id) {
            post.verdict_writes += 1;
            post.verdict.get_or_insert(reply.text);
        }
    });
    Ok(())
}

/// Send the new-post webhook over `SimNet`. A drop or a partition fails the
/// job, and the job retries.
#[job(name = "notify_hooks", max_attempts = 8, backoff_ms = 1000)]
async fn notify_hooks(state: AppState, args: PostArgs) -> AutumnResult<()> {
    let response = Client::from_state(&state)
        .post(format!("http://{HOOKS_HOST}/new-post/{}", args.post_id))
        .send()
        .await
        .map_err(|error| {
            tracing::debug!(post_id = args.post_id, %error, "notify_hooks: the call failed");
            AutumnError::service_unavailable_msg(error.to_string())
        })?;
    if !response.status().is_success() {
        return Err(AutumnError::service_unavailable_msg(
            "the hook receiver refused",
        ));
    }
    reddit(&state).shared.store.with(|t| {
        if let Some(post) = t.posts.get_mut(&args.post_id) {
            post.hook_delivered = true;
        }
    });
    Ok(())
}

/// Every 15 minutes: rank all posts again (as `tasks::recalculate_hot_ranks`
/// does in SQL), and enqueue the jobs that a crash lost.
#[scheduled(every = "15m", name = "rank-sweep")]
async fn rank_sweep(state: AppState) -> AutumnResult<()> {
    let now = state.clock().now().naive_utc();
    let lost: Vec<(i64, &'static str)> = reddit(&state).shared.store.with(|t| {
        t.sweep_runs += 1;
        let mut lost = Vec::new();
        for post in t.posts.values_mut() {
            post.hot_rank = calculate_hot_rank(post.score, post.created_at, now);
            if !post.ranked {
                lost.push((post.id, "rank_post"));
            }
            if post.verdict.is_none() {
                lost.push((post.id, "auto_mod"));
            }
            if !post.hook_delivered {
                lost.push((post.id, "notify_hooks"));
            }
        }
        lost
    });
    let jobs = state
        .extension::<JobClient>()
        .expect("the local job runtime installs a JobClient");
    tracing::debug!(lost = lost.len(), "rank_sweep: enqueue the lost jobs");
    for (post_id, job) in lost {
        jobs.enqueue(job, serde_json::json!({ "post_id": post_id }))
            .await?;
    }
    Ok(())
}

/// The webhook receiver, a `SimNet` host. It counts each delivery.
pub fn hooks_router(store: Arc<Store>) -> axum::Router {
    axum::Router::new().route(
        "/new-post/{post_id}",
        axum::routing::post(
            move |axum::extract::Path(post_id): axum::extract::Path<i64>| {
                let store = Arc::clone(&store);
                async move {
                    store.with(|t| *t.hooks_received.entry(post_id).or_default() += 1);
                    "ok"
                }
            },
        ),
    )
}
