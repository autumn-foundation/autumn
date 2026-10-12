//! One seed of the nightly: the whole scenario and its checks.
//!
//! [`run`] makes a new sim runtime, runs [`body`] for one seed, and returns
//! the trace. [`body`] has these phases:
//!
//! 1. Sign-ups. The welcome mail goes through SMTP faults and retries with
//!    full jitter.
//! 2. Posts. Each post enqueues a rank job, an auto-mod job (LLM stub) and a
//!    webhook job (`SimNet`).
//! 3. The vote race. Votes race through `vote_core::cast` in a seeded order.
//!    Subscribers must never get an older score after a newer one.
//! 4. Policy and feed. The real `PostPolicy` and feed experiment.
//! 5. Time. The posts age in virtual time; the scheduled rank sweep runs.
//! 6. Replicas with skewed clocks race votes and rank posts.
//! 7. A crash at a seeded await, then a restart on the same store.
//! 8. Settle, then check the end state, the fault ledger and the logs.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::backoff::{ceiling_ms, full_jitter, full_jitter_ms, retry_after_wait};
use autumn_web::config::AutumnConfig;
use autumn_web::entropy::SeededEntropy;
use autumn_web::mail::Transport;
use autumn_web::sim::fault::FaultOutcome;
use autumn_web::sim::llm::{LlmError, SeededLlm};
use autumn_web::sim::trace::{FRAMEWORK_TARGET, Trace, capture_targets};
use autumn_web::sim::{
    Chaos, FaultPlan, MailFault, Replica, Sim, SimNet, SometimesRegistry, crash_at,
    sometimes_unsatisfied,
};
use autumn_web::test::{TestApp, TestClient};
use autumn_web::time::{
    AmbientClock, ambient_instant, ambient_monotonic, ambient_now, ambient_system_time,
    clock_unix_duration, clock_unix_secs,
};
use autumn_web::{always, sometimes};
use chrono::{NaiveDate, TimeDelta, TimeZone as _, Utc};
use futures::FutureExt as _;
use proptest::strategy::Strategy;

use reddit_clone::tasks::calculate_hot_rank;
use reddit_clone::vote_core::PublishOrder;

use super::app::{HOOKS_HOST, HotRow, POSTS_TOPIC, RedditPlugin, Shared, Worker, hooks_router};
use super::store::Store;

/// Which vote rule the apps use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// `vote_core::cast`, the rule of the real app.
    Correct,
    /// The planted bug: a new `PublishOrder` for each vote. The sweep must
    /// catch it.
    UnorderedPublish,
}

/// The trace records the framework, the app and this scenario.
const TRACE_TARGETS: &[&str] = &[FRAMEWORK_TARGET, "reddit_clone", "sim_nightly"];

/// How many trace lines a failure shows.
const FAILURE_TRACE_LINES: usize = 60;

/// Replica clocks can be this far off. It is more than the 2 hours in the rank
/// formula, so a replica can rank a post that is "from the future".
const MAX_REPLICA_OFFSET: Duration = Duration::from_secs(3 * 3600);

/// The interval of the scheduled rank sweep.
const SWEEP_EVERY: Duration = Duration::from_secs(15 * 60);

/// A seed that takes more virtual time than this has hung: all its tasks wait
/// and no timer can wake them. One seed uses about 2 virtual days.
const LIVENESS_BUDGET: Duration = Duration::from_secs(30 * 24 * 3600);

/// Run one seed on a new sim runtime.
///
/// # Errors
///
/// Returns the panic message and the last trace lines when a check fails.
pub fn run(seed: u64, variant: Variant) -> Result<Trace, String> {
    let runtime = autumn_web::sim::runtime().map_err(|error| format!("sim runtime: {error}"))?;
    let (outcome, trace) = runtime.block_on(async move {
        let mut sim = Sim::from_seed(seed);
        sim.anchor();
        capture_targets(TRACE_TARGETS, async {
            // The paused clock jumps to the budget only when no task can run.
            match tokio::time::timeout(
                LIVENESS_BUDGET,
                AssertUnwindSafe(body(&mut sim, variant)).catch_unwind(),
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(_) => Err(Box::new(format!(
                    "seed {seed} hung: no task could run for {} virtual days",
                    LIVENESS_BUDGET.as_secs() / 86_400
                )) as Box<dyn std::any::Any + Send>),
            }
        })
        .await
    });
    match outcome {
        Ok(()) => Ok(trace),
        Err(panic) => {
            let reason = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "the scenario panicked".to_owned());
            let tail = trace.len().saturating_sub(FAILURE_TRACE_LINES);
            Err(format!(
                "{reason}\nthe last trace lines:\n{}",
                trace.lines()[tail..].join("\n")
            ))
        }
    }
}

/// The workload of one seed, drawn from the seed.
#[derive(Debug)]
struct Plan {
    users: i64,
    posts: usize,
    /// `(user index, post index, upvote)`.
    votes: Vec<(u8, u8, bool)>,
    /// For each replica vote: `true` sends it to `web-a`.
    routing: Vec<bool>,
    actors: Vec<String>,
    drop_rate: f64,
    max_latency_ms: u64,
    partition_first: bool,
    fanout_fail_one_in: u64,
    duplicate_rate: f64,
    smtp_rate: f64,
    skew_ms: u64,
    clock_b: (bool, Duration, i64),
    step_b_secs: i64,
}

impl Plan {
    fn draw(sim: &mut Sim) -> Self {
        let users = 2 + i64::try_from(sim.rng().next_u64() % 4).expect("small");
        let posts = 1 + usize::try_from(sim.rng().next_u64() % 3).expect("small");
        let max_user = u8::try_from(users).expect("small");
        let max_post = u8::try_from(posts).expect("small");
        let votes = sim.gen_ops_with(proptest::collection::vec(
            (0..max_user, 0..max_post, proptest::bool::ANY),
            4..=16,
        ));
        let routing: Vec<bool> = sim.gen_ops();
        let mut secret = [0_u8; 8];
        sim.rng().fill_bytes(&mut secret);
        let actors = vec![
            format!("anon-{}", sim.rng().uuid_v4()),
            format!("anon-{}", sim.rng().uuid_v7(1_577_836_800_000)),
            format!("anon-{}", sim.rng().derive_uuid(secret)),
        ];
        let draw = |sim: &mut Sim, n: u64| sim.rng().next_u64() % n;
        #[allow(clippy::cast_precision_loss)] // Small values.
        let rate = |sim: &mut Sim, max_percent: u64| draw(sim, max_percent + 1) as f64 / 100.0;
        Self {
            users,
            posts,
            votes,
            routing,
            actors,
            drop_rate: rate(sim, 30),
            max_latency_ms: 1 + draw(sim, 200),
            partition_first: draw(sim, 2) == 0,
            fanout_fail_one_in: 4 + draw(sim, 13),
            duplicate_rate: rate(sim, 30),
            smtp_rate: rate(sim, 20),
            skew_ms: draw(sim, 500),
            clock_b: (
                draw(sim, 2) == 0,
                Duration::from_secs(draw(sim, 4 * 3600)),
                i64::try_from(draw(sim, 1001)).expect("small") - 500,
            ),
            step_b_secs: i64::try_from(draw(sim, 601)).expect("small") - 300,
        }
    }
}

/// The whole scenario for the seed of `sim`.
pub async fn body(sim: &mut Sim, variant: Variant) {
    let seed = sim.seed();
    let plan = Plan::draw(sim);
    tracing::info!(seed, ?plan, "plan");

    let llm = if seed.is_multiple_of(2) {
        SeededLlm::builder(seed)
    } else {
        SeededLlm::from_entropy(sim.seeded_entropy().as_ref())
    };
    let shared = Arc::new(Shared {
        store: Arc::new(Store::default()),
        llm: llm
            .default_model("mod-model")
            .canned_response("moderate", "approve")
            .fault_at(0, LlmError::RateLimited)
            .fault_probability(0.2, LlmError::ServiceUnavailable)
            .latency_up_to(Duration::from_millis(500))
            .build(),
        experiments: reddit_clone::experiments::setup(),
        variant,
        fanout_fail_one_in: plan.fanout_fail_one_in,
    });

    let net = SimNet::new()
        .host(HOOKS_HOST, hooks_router(Arc::clone(&shared.store)))
        .latency(
            Duration::from_millis(1),
            Duration::from_millis(plan.max_latency_ms),
        )
        .drop_rate(plan.drop_rate);
    sim.net(net.clone());
    // `db_transient_errors` has no effect here: the app has no database pool.
    sim.chaos(
        Chaos::default()
            .db_transient_errors(0.1)
            .job_duplicate_delivery(plan.duplicate_rate)
            .clock_skew(Duration::from_millis(plan.skew_ms))
            .smtp_transient_errors(plan.smtp_rate)
            .smtp_faults([(2, MailFault::Fail), (3, MailFault::Timeout)]),
    );
    // Wide: a debug build on a slow runner must not trip it.
    sim.strict_wall_clock()
        .strict_wall_clock_budget(Duration::from_secs(120));

    let faults = fault_plan(seed);
    sim.build(main_app(&shared, faults.clone()));
    always!(sim.try_client().is_some(), "build mounts the main app");

    signups(sim, &shared, &plan).await;
    if plan.partition_first {
        net.partition(HOOKS_HOST);
    }
    let posts = submit_posts(sim, &plan).await;
    vote_race(sim, &shared, &posts, &plan).await;
    check_publish_order(&payloads(sim.client()), &shared, "main", true);
    policy_and_feed(sim, &shared, &posts, &plan).await;
    age_posts(sim).await;
    net.heal(HOOKS_HOST);
    check_fault_ledger(sim, &faults).await;
    replicas(sim, &shared, &posts, &plan).await;
    // The job runtime is process-wide: it belongs to the app mounted last.
    // `crash_and_recover` mounts the main app last again, so its job worker
    // runs the jobs that `settle` waits for.
    crash_and_recover(sim, &shared, &faults, &plan).await;
    settle(sim, &shared).await;
    check_end_state(sim, &shared, &net);

    tracing::info!(
        unsatisfied = sometimes_unsatisfied().len(),
        "seed done; open sometimes! labels are for the sweep"
    );
}

/// The faults that the plan names. The database faults stay unfired, because
/// the app has no database pool; the check reads them from `unfired`.
fn fault_plan(seed: u64) -> FaultPlan {
    let plan = FaultPlan::from_seed(seed)
        .fail_job("rank_post", 1)
        .fail_job_execution(3)
        .random_job_execution_faults(2, 4..=12)
        .fail_db_checkout(1)
        .fail_db_checkout_on("primary", 2)
        .random_db_checkout_faults(1, 3..=6)
        .only_between(Duration::ZERO, Duration::from_secs(6 * 3600));
    always!(plan.seed() == seed, "a fault plan keeps its seed");
    always!(plan.is_active(), "a plan with faults is active");
    // A head line, one line per fault, and the window line.
    always!(
        plan.describe().lines().count() == plan.planned().len() + 2,
        "describe prints one line per planned fault"
    );
    plan
}

/// The config of the main app: one job worker, all 5xx reported (both are
/// `FaultPlan` rules), and a mail transport.
fn config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.jobs.workers = 1;
    config.reporting.sample_rate = 1.0;
    config.mail.transport = Transport::Log;
    config.mail.from = Some("noreply@reddit-clone.test".to_owned());
    config
}

fn main_app(shared: &Arc<Shared>, faults: FaultPlan) -> TestApp {
    TestApp::new()
        .plugin(RedditPlugin {
            shared: Arc::clone(shared),
            main: true,
        })
        .config(config())
        .record_broadcasts()
        .with_fault_plan(faults)
}

fn replica_app(shared: &Arc<Shared>, entropy_seed: u64) -> TestApp {
    TestApp::new()
        .plugin(RedditPlugin {
            shared: Arc::clone(shared),
            main: false,
        })
        .record_broadcasts()
        .with_entropy(SeededEntropy::new(entropy_seed))
}

// ── Phases ───────────────────────────────────────────────────────────────────

async fn signups(sim: &Sim, shared: &Shared, plan: &Plan) {
    // Its own stream: `sim.seeded_entropy()` gives the same numbers as the
    // entropy of the first app mount.
    let jitter = SeededEntropy::shared(sim.seed() ^ 0x5167_A11E);
    for user in 1..=plan.users {
        let mut attempt = 0;
        loop {
            let body = sim.client().post(&format!("/signup/{user}")).send().await;
            body.assert_ok();
            if body.text() == "sent" {
                break;
            }
            attempt += 1;
            always!(
                attempt < 32,
                "the welcome mail of user {user} failed 32 times"
            );
            let backoff_ms = full_jitter_ms(jitter.as_ref(), 100, 10_000, attempt);
            always!(
                backoff_ms <= ceiling_ms(100, 10_000, attempt),
                "full jitter stays under the ceiling"
            );
            let backoff = full_jitter(
                jitter.as_ref(),
                Duration::from_millis(100),
                Duration::from_secs(10),
                attempt,
            );
            let wait = retry_after_wait(Duration::from_millis(backoff_ms), backoff);
            always!(wait >= backoff, "a Retry-After wait keeps the backoff");
            sim.advance(wait).await;
        }
        sometimes!(attempt > 0, "a welcome mail failed and a retry sent it");
    }
    let sent = shared
        .store
        .with(|t| t.users.values().filter(|u| u.welcome_sent).count());
    always!(
        sent == usize::try_from(plan.users).expect("small"),
        "each user got a welcome mail"
    );
}

async fn submit_posts(sim: &Sim, plan: &Plan) -> Vec<i64> {
    let mut posts = Vec::with_capacity(plan.posts);
    for index in 0..plan.posts {
        let author = 1 + i64::try_from(index).expect("small") % plan.users;
        let response = sim
            .client()
            .post(&format!("/posts/new/{author}"))
            .send()
            .await;
        response.assert_ok();
        posts.push(
            response
                .text()
                .parse()
                .expect("the submit body is the post id"),
        );
    }
    posts
}

fn vote_uri(posts: &[i64], (user, post, up): (u8, u8, bool)) -> String {
    format!(
        "/posts/{}/vote/{}/{}",
        posts[usize::from(post)],
        i64::from(user) + 1,
        if up { 1 } else { -1 }
    )
}

async fn vote_race(sim: &Sim, shared: &Shared, posts: &[i64], plan: &Plan) {
    let client = sim.client();
    let requests = plan
        .votes
        .iter()
        .map(|vote| client.post(&vote_uri(posts, *vote)).send())
        .collect();
    let responses = sim.interleave(requests).await;
    for (vote, response) in plan.votes.iter().zip(responses) {
        response.assert_ok();
        let text = response.text();
        let (score, value) = text
            .split_once(' ')
            .expect("the vote body is `score value`");
        let score: i64 = score.parse().expect("a score");
        sometimes!(value == "0", "a repeated vote toggled off");
        let post = posts[usize::from(vote.1)];
        let known = shared.store.with(|t| {
            t.history
                .iter()
                .any(|((p, _), s)| *p == post && *s == score)
        });
        always!(
            known,
            "vote response score {score} for post {post} was never committed"
        );
    }
    shared.store.check().unwrap_or_else(|rule| panic!("{rule}"));
}

/// The vote payloads that `client` broadcast, in order.
fn payloads(client: &TestClient) -> Vec<String> {
    client
        .broadcasts_on(POSTS_TOPIC)
        .into_iter()
        .map(|broadcast| broadcast.payload)
        .collect()
}

/// Check the publish log of one process. `complete`: all votes went through
/// this app. If no fan-out failed, the last publish of each post must have its
/// current version.
fn check_publish_order(log: &[String], shared: &Shared, app: &str, complete: bool) {
    let mut last: BTreeMap<i64, u64> = BTreeMap::new();
    for payload in log {
        let mut parts = payload
            .split(' ')
            .map(|p| p.parse::<i64>().expect("a number"));
        let (post, version, score) = (
            parts.next().expect("post"),
            u64::try_from(parts.next().expect("version")).expect("a version"),
            parts.next().expect("score"),
        );
        always!(
            shared.store.score_at(post, version) == Some(score),
            "{app} published score {score} as version {version} of post {post}, \
             but that version had another score"
        );
        if let Some(previous) = last.insert(post, version) {
            always!(
                version >= previous,
                "{app} published an older score after a newer one: post {post} went from \
                 version {previous} to version {version}"
            );
            sometimes!(
                version > previous + 1,
                "two votes raced, so one publish skipped a version"
            );
        }
    }
    let failures = shared.store.with(|t| t.fanout_failures.clone());
    sometimes!(
        !failures.is_empty(),
        "a vote fan-out failed and the viewer got the committed score"
    );
    if complete {
        for (post, published) in &last {
            if failures.get(post).copied().unwrap_or(0) == 0 {
                let (version, _) = shared.store.load(*post).expect("a post");
                always!(
                    *published == version,
                    "{app}: the last publish of post {post} is version {published}, \
                     but the post is at version {version}"
                );
            }
        }
    }
}

async fn policy_and_feed(sim: &mut Sim, shared: &Shared, posts: &[i64], plan: &Plan) {
    for post in posts {
        let users = u64::try_from(plan.users).expect("positive");
        let actor = 1 + i64::try_from(sim.rng().next_u64() % users).expect("small");
        let admin = sim.rng().next_u64().is_multiple_of(2);
        let author = shared.store.with(|t| t.posts[post].author_id);
        let response = sim
            .client()
            .put(&format!("/posts/{post}/edit/{actor}/{admin}"))
            .send()
            .await;
        response.assert_ok();
        let allowed = response.text() == "allowed";
        always!(
            allowed == (admin || actor == author),
            "PostPolicy let user {actor} (admin: {admin}) edit post {post} of user {author}: {allowed}"
        );
        sometimes!(!allowed, "a user who is not the author was denied an edit");
        sometimes!(
            admin && actor != author,
            "an admin edited the post of another user"
        );
    }
    for actor in &plan.actors {
        let first = feed(sim.client(), actor).await;
        let again = feed(sim.client(), actor).await;
        always!(
            first == again,
            "the feed variant of {actor} changed: {first} then {again}"
        );
        sometimes!(first == "compact", "an actor got the compact feed");
        sometimes!(first == "card", "an actor got the card feed");
    }
}

async fn feed(client: &TestClient, actor: &str) -> String {
    let response = client.get(&format!("/feed/{actor}")).send().await;
    response.assert_ok();
    response.text()
}

/// Let the posts age. Check the front page and the ambient clock seams at
/// each step.
async fn age_posts(sim: &Sim) {
    sim.run_to_idle().await;
    let mut previous = hot(sim.client(), "main").await;
    for step in 1..=3_u32 {
        let now = ambient_now();
        let mono = ambient_monotonic();
        let instant = ambient_instant();
        let system = ambient_system_time();
        let unix = clock_unix_duration(&AmbientClock);
        sim.advance(SWEEP_EVERY).await;
        always!(ambient_now() - now == TimeDelta::from_std(SWEEP_EVERY).expect("small"));
        always!(ambient_monotonic().saturating_duration_since(mono) == SWEEP_EVERY);
        always!(ambient_instant().duration_since(instant) == SWEEP_EVERY);
        always!(ambient_system_time().duration_since(system).ok() == Some(SWEEP_EVERY));
        always!(clock_unix_duration(&AmbientClock) - unix == SWEEP_EVERY);
        always!(clock_unix_secs(&AmbientClock) >= unix.as_secs());
        sim.run_to_idle().await;
        let current = hot(sim.client(), "main").await;
        for row in &current {
            let Some(before) = previous.iter().find(|r| r.id == row.id) else {
                continue;
            };
            if row.score == before.score && row.score > 0 {
                always!(
                    row.rank < before.rank,
                    "post {} did not decay at step {step}: {} then {}",
                    row.id,
                    before.rank,
                    row.rank
                );
            }
        }
        previous = current;
    }
    let tomorrow = NaiveDate::from_ymd_opt(2020, 1, 2).expect("a date");
    sim.advance_to(&Utc.from_utc_datetime(&tomorrow.and_hms_opt(9, 0, 0).expect("a time")))
        .await;
    let new_york_winter = chrono::FixedOffset::west_opt(5 * 3600).expect("an offset");
    sim.advance_to_local(
        tomorrow.and_hms_opt(18, 0, 0).expect("a time"),
        &new_york_winter,
    )
    .await;
    if let Err(stall) = sim.try_run_to_idle().await {
        panic!(
            "the app did not settle: seed {}, {} drain steps: {stall}",
            stall.seed, stall.steps
        );
    }
    hot(sim.client(), "main").await;
}

/// Read the front page of `client` and check the rank rules.
async fn hot(client: &TestClient, app: &str) -> Vec<HotRow> {
    let response = client.get("/hot").send().await;
    response.assert_ok();
    let rows: Vec<HotRow> = serde_json::from_str(&response.text()).expect("front page JSON");
    for row in &rows {
        always!(
            row.rank.is_finite(),
            "{app} ranked post {} (score {}, created {}) as {}",
            row.id,
            row.score,
            row.created_at,
            row.rank
        );
        always!(
            row.rank.signum() == (row.score as f64).signum() || row.score == 0,
            "{app}: the rank {} of post {} has the wrong sign for score {}",
            row.rank,
            row.id,
            row.score
        );
    }
    for (a, b) in rows.iter().zip(rows.iter().skip(1)) {
        always!(
            a.rank >= b.rank,
            "{app}: the front page is not in rank order"
        );
        if a.score == b.score && a.score > 0 {
            always!(
                a.created_at >= b.created_at || a.rank == b.rank,
                "{app}: an older post ranks above a newer post with the same score"
            );
        }
    }
    rows
}

async fn check_fault_ledger(sim: &Sim, faults: &FaultPlan) {
    let client = sim.client();
    let outcome = client.fault_outcome().await;
    let ledger = client.fault_ledger().expect("a plan is attached");
    always!(
        ledger.outcome() == outcome,
        "the ledger and fault_outcome agree"
    );
    let json = outcome.to_json_string();
    let back = FaultOutcome::from_json_str(&json).expect("the outcome JSON parses");
    always!(
        back == outcome,
        "the fault outcome survives a JSON round trip"
    );
    always!(back.fingerprint() == outcome.fingerprint());
    always!(
        outcome.final_state.db_checkouts == 0,
        "the app has no database pool"
    );
    always!(
        outcome.fired.len() + outcome.suppressed.len() + outcome.unfired.len()
            == faults.planned().len(),
        "each planned fault fired, was suppressed, or stayed unfired"
    );
    sometimes!(
        outcome.fired.len() >= 2,
        "the fault plan failed two job executions and the retries ran"
    );
}

async fn crash_and_recover(sim: &mut Sim, shared: &Arc<Shared>, faults: &FaultPlan, plan: &Plan) {
    let schedule = sim.crash_schedule();
    let point = sim
        .crash_point()
        .expect("the crash schedule is never empty");
    always!(
        schedule.first() == Some(&point),
        "crash_point is the first point"
    );
    always!(!schedule.points().is_empty());

    let users = u64::try_from(plan.users).expect("positive");
    let author = 1 + i64::try_from(sim.rng().next_u64() % users).expect("small");
    let uri = format!("/posts/new/{author}");
    let outcome = crash_at(point.await_index, sim.client().post(&uri).send()).await;
    if outcome.is_crashed() {
        sometimes!(true, "a submit crashed at a seeded await");
        sim.crash_and_restart(main_app(shared, faults.clone()));
    } else {
        sometimes!(true, "a submit finished before its seeded crash point");
        outcome.completed().expect("not crashed").assert_ok();
        sim.kill();
        always!(sim.try_client().is_none(), "kill drops the app");
        sim.restart(main_app(shared, faults.clone()));
    }
    always!(sim.try_client().is_some(), "the restart mounts the app");
}

async fn replicas(sim: &mut Sim, shared: &Arc<Shared>, posts: &[i64], plan: &Plan) {
    let seed = sim.seed();
    let a = Replica::named("web-a").seeded_clock(MAX_REPLICA_OFFSET, 500);
    always!(a.name() == "web-a");
    let (ahead, offset, drift) = plan.clock_b;
    let b = if ahead {
        Replica::named("web-b").clock_ahead(offset)
    } else {
        Replica::named("web-b").clock_behind(offset)
    }
    .clock_drift_ppm(drift);
    sim.mount_replica(a, replica_app(shared, seed ^ 0xA));
    sim.mount_replica(b, replica_app(shared, seed ^ 0xB));
    always!(sim.replica_names() == ["web-a", "web-b"]);
    // A new post, so that a replica with a slow clock ranks a post from its
    // future.
    let fresh = sim.client().post("/posts/new/1").send().await;
    fresh.assert_ok();
    for name in ["web-a", "web-b"] {
        let clock = sim.replica_clock(name).expect("a mounted replica");
        tracing::info!(name, offset = %clock.offset(), drift = clock.drift_ppm(), "replica clock");
    }

    // The replicas take the votes through `Sim::interleave`. A vote worker,
    // a third process, casts the same votes in tasks from `Sim::spawn`.
    let worker = Arc::new(Worker {
        shared: Arc::clone(shared),
        order: PublishOrder::new(),
        entropy: SeededEntropy::new(seed ^ 0x3),
        published: std::sync::Mutex::default(),
    });
    let handles: Vec<_> = plan
        .votes
        .iter()
        .map(|&(user, post, up)| {
            let worker = Arc::clone(&worker);
            let post_id = posts[usize::from(post)];
            sim.spawn(async move {
                worker
                    .cast(i64::from(user) + 1, post_id, if up { 1 } else { -1 })
                    .await
            })
        })
        .collect();
    let requests = plan
        .votes
        .iter()
        .enumerate()
        .map(|(i, vote)| {
            let to_a = plan.routing.get(i).copied().unwrap_or(i % 2 == 0);
            sim.replica(if to_a { "web-a" } else { "web-b" })
                .post(&vote_uri(posts, *vote))
                .send()
        })
        .collect();
    for response in sim.interleave(requests).await {
        response.assert_ok();
    }
    for handle in handles {
        handle
            .await
            .expect("the worker task ran")
            .expect("the worker vote");
    }
    shared.store.check().unwrap_or_else(|rule| panic!("{rule}"));
    for name in ["web-a", "web-b"] {
        check_publish_order(&payloads(sim.replica(name)), shared, name, false);
        hot(sim.replica(name), name).await;
    }
    let worker_log = worker
        .published
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    check_publish_order(&worker_log, shared, "the vote worker", false);

    sim.step_replica_clock("web-b", TimeDelta::seconds(plan.step_b_secs));
    sim.run_for(Duration::from_secs(30)).await;
    hot(sim.replica("web-b"), "web-b").await;
    for actor in &plan.actors {
        let main = feed(sim.client(), actor).await;
        let other = feed(sim.replica("web-a"), actor).await;
        always!(
            main == other,
            "the feed variant of {actor} differs between apps"
        );
    }

    sim.kill_replica("web-b");
    always!(
        sim.try_replica("web-b").is_none(),
        "kill_replica drops the app"
    );
    sim.restart_replica("web-b", replica_app(shared, seed ^ 0xBB));
    always!(
        sim.try_replica("web-b").is_some(),
        "restart_replica mounts it again"
    );
    hot(sim.replica("web-b"), "web-b").await;
}

/// Let 90 minutes pass one timer event at a time, so that each job retry
/// runs at its backoff and the rank sweep runs six times. Use `run_for`, not
/// `advance`: `advance` jumps to the end and fires each due timer one time
/// only.
async fn settle(sim: &Sim, shared: &Shared) {
    let before = shared.store.with(|t| t.sweep_runs);
    sim.run_for(6 * SWEEP_EVERY).await;
    sim.run_to_idle().await;
    // Each sweep starts new job chains. With fewer sweeps, a correct app can
    // fail the end checks by chance (about 1e-9 per seed with 2 sweeps).
    let sweeps = shared.store.with(|t| t.sweep_runs) - before;
    always!(
        sweeps >= 5,
        "the rank sweep ran {sweeps} time(s) while the app settled"
    );
}

fn check_end_state(sim: &Sim, shared: &Shared, net: &SimNet) {
    shared.store.check().unwrap_or_else(|rule| panic!("{rule}"));
    let received = shared.store.with(|t| t.hooks_received.clone());
    for post in shared.store.posts() {
        always!(post.ranked, "post {} was never ranked", post.id);
        always!(
            post.verdict.as_deref() == Some("approve"),
            "post {} has the auto-mod verdict {:?}",
            post.id,
            post.verdict
        );
        always!(
            post.hook_delivered,
            "the webhook of post {} was never delivered",
            post.id
        );
        always!(
            received.get(&post.id).copied().unwrap_or(0) >= 1,
            "the receiver never got post {}",
            post.id
        );
        sometimes!(
            post.rank_runs > 1 || post.verdict_writes > 1,
            "a job ran twice and stayed idempotent"
        );
    }
    sometimes!(
        received.values().any(|count| *count > 1),
        "the receiver got one webhook twice"
    );
    let events = net.events();
    sometimes!(
        events
            .iter()
            .any(|e| e.fault != autumn_web::sim::net::NetFault::None),
        "SimNet dropped or cut a webhook attempt and a retry delivered it"
    );
    let calls = shared.llm.calls();
    always!(
        calls.windows(2).all(|pair| pair[0].seq < pair[1].seq),
        "the LLM call log is in call order"
    );
    sometimes!(
        calls.iter().any(|call| call.error.is_some()),
        "an LLM call failed and the job retried"
    );
    let registry = SometimesRegistry::new();
    always!(
        registry.observed().is_empty()
            && registry.satisfied().is_empty()
            && registry.unsatisfied().is_empty(),
        "a new registry is empty"
    );
    always!(sim.replica_names().len() == 2);
}

// ── The single-seed reachability check and the proptest lane ─────────────────

/// Two score bands that land on both sides of the front-page line after a day,
/// at every seed. A test can call `assert_all_sometimes_satisfied` after it.
pub async fn rank_bands(sim: &mut Sim) {
    let hot_score = 400 + i64::try_from(sim.rng().next_u64() % 500).expect("small");
    let cold_score = 10 + i64::try_from(sim.rng().next_u64() % 190).expect("small");
    let created = ambient_now().naive_utc();
    sim.advance(Duration::from_secs(24 * 3600)).await;
    let now = ambient_now().naive_utc();
    for score in [hot_score, cold_score] {
        let rank = calculate_hot_rank(score, created, now);
        sometimes!(
            rank >= 2.0,
            "a high-scoring post was still on the front page after a day"
        );
        sometimes!(
            rank < 2.0,
            "a low-scoring post left the front page within a day"
        );
    }
}

/// Generated posts: `(score, age in minutes)`.
pub fn rank_case_strategy() -> impl Strategy<Value = Vec<(i64, u32)>> {
    proptest::collection::vec((-5_000_i64..5_000, 0_u32..(60 * 24 * 30)), 1..12)
}

/// Check the real rank formula on generated posts.
pub fn check_rank_cases(_sim: &mut Sim, cases: &[(i64, u32)]) {
    let now = chrono::NaiveDateTime::default() + TimeDelta::days(60);
    for &(score, age) in cases {
        let created = now - TimeDelta::minutes(i64::from(age));
        let rank = calculate_hot_rank(score, created, now);
        always!(
            rank.is_finite(),
            "rank of score {score} at age {age} min is {rank}"
        );
        always!(rank.signum() == (score as f64).signum() || score == 0);
        let older = calculate_hot_rank(score, created - TimeDelta::hours(1), now);
        if score > 0 {
            always!(
                older < rank,
                "an older post with score {score} did not rank lower"
            );
        }
        let higher = calculate_hot_rank(score + 1, created, now);
        always!(
            higher > rank,
            "score {} did not rank above score {score}",
            score + 1
        );
        sometimes!(score > 0, "a generated post had a positive score");
    }
}
