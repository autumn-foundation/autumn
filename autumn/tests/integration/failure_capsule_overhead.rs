//! Measured cost of leaving failure capture switched on (issue #1598, AC5).
//!
//! Capture is a hot-path feature: with `[failure_capture] enabled = true` every
//! request — including the overwhelming majority that succeed — pays for a
//! capture scope, a request-head snapshot, a streaming body tee, the
//! `SET autumn.capsule_request` marker merged into the checkout batch, and the
//! wire tee on the pooled connection. Only *failing* requests pay for
//! redaction and the capsule write.
//!
//! This is a benchmark, not an assertion of a performance contract: it drives
//! two routes — one that touches nothing, one that runs a bound `SELECT` —
//! with capture off and on, prints p50/p95/mean and the deltas, and only fails
//! if the overhead is so large it has to be a bug (a per-request round trip
//! added, a body buffered that should not be, a lock held across the handler).
//! Two routes because the database one's number is confounded: with capture
//! armed the checkout marker rides in a single simple-query batch where the
//! uncaptured path issues its `SET` through the extended protocol, so a
//! database-route comparison measures the tee *minus* that saving. The no-DB
//! route is what isolates the request-layer cost.
//!
//! The published numbers in `docs/guide/failure-capsules.md` come from running
//! this.
//!
//! Run it directly:
//!
//! ```text
//! cargo test -p autumn-web --features test-support --test integration_tests \
//!     -- --ignored --nocapture capture_overhead
//! ```

use std::time::{Duration, Instant};

use autumn_web::AutumnError;
use autumn_web::config::AutumnConfig;
use autumn_web::db::Db;
use autumn_web::test::{TestApp, TestDb};
use autumn_web::{get, routes};
use diesel_async::RunQueryDsl as _;

/// Requests measured per phase *per round*, after that round's warm-up.
const SAMPLES: usize = 1_000;
/// How many times the two phases alternate. Alternating cancels the drift a
/// single off-then-on ordering would charge entirely to capture.
const ROUNDS: usize = 2;
/// Requests thrown away first, so pool growth and prepared-statement caching
/// are not charged to the first phase measured.
const WARMUP: usize = 200;
/// Size of the recording pool. Matches `TestDb`'s own pool, which the baseline
/// phase uses, so neither phase measures a different amount of connection
/// contention.
const POOL_SIZE: usize = 5;

/// Ceiling the measured p95 ratio must stay under.
///
/// Deliberately generous: this runs on shared virtualized CI hardware where a
/// sub-millisecond baseline is easily doubled by a neighbour. It exists to
/// catch a structural regression — an extra database round trip per request,
/// or a body buffered when it should have been streamed — not to police a few
/// microseconds.
const MAX_P95_RATIO: f64 = 3.0;

/// Below this, the baseline is small enough that scheduler noise dominates the
/// ratio and only the absolute number is meaningful.
const RATIO_FLOOR: Duration = Duration::from_micros(200);

#[derive(diesel::QueryableByName)]
struct Label {
    #[diesel(sql_type = diesel::sql_types::Text)]
    label: String,
}

/// One bound `SELECT` through the pooled connection, then a 200 — the shape
/// almost every request takes, and the one capture's *whole* cost lands on.
#[get("/bench")]
async fn bench(mut db: Db) -> Result<String, AutumnError> {
    let row: Label = diesel::sql_query("SELECT $1::text AS label")
        .bind::<diesel::sql_types::Text, _>("bench")
        .get_result(&mut db)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!("query failed: {error}"))
        })?;
    Ok(row.label)
}

/// No database at all, so the measurement isolates what the *request* layer of
/// capture costs: the scope, the registry entry, the head snapshot and the body
/// tap. Measured separately because the database route's number is
/// confounded — see the comment on the checkout marker below.
#[get("/plain")]
async fn plain() -> &'static str {
    "ok"
}

/// Latency summary of one phase, in microseconds.
struct Summary {
    p50: Duration,
    p95: Duration,
    mean: Duration,
}

impl Summary {
    fn of(mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();
        let len = samples.len().max(1);
        let total: Duration = samples.iter().sum();
        Self {
            p50: percentile(&samples, 50),
            p95: percentile(&samples, 95),
            mean: total / u32::try_from(len).unwrap_or(u32::MAX),
        }
    }
}

fn percentile(sorted: &[Duration], percentile: usize) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let index = sorted.len().saturating_mul(percentile) / 100;
    sorted[index.min(sorted.len() - 1)]
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

/// Print one phase pair as a table on stderr.
fn report(label: &str, off: &Summary, on: &Summary) {
    eprintln!(
        "\nfailure-capsule capture overhead — {label}\n  ({} requests/phase over {ROUNDS} \
         interleaved rounds)",
        SAMPLES * ROUNDS
    );
    eprintln!("  phase            p50 (us)   p95 (us)  mean (us)");
    eprintln!(
        "  capture off     {:9.1}  {:9.1}  {:9.1}",
        micros(off.p50),
        micros(off.p95),
        micros(off.mean)
    );
    eprintln!(
        "  capture on      {:9.1}  {:9.1}  {:9.1}",
        micros(on.p50),
        micros(on.p95),
        micros(on.mean)
    );
    eprintln!(
        "  delta           {:+9.1}  {:+9.1}  {:+9.1}",
        micros(on.p50) - micros(off.p50),
        micros(on.p95) - micros(off.p95),
        micros(on.mean) - micros(off.mean)
    );
    eprintln!(
        "  ratio           {:9.2}x {:9.2}x {:9.2}x",
        micros(on.p50) / micros(off.p50).max(1.0),
        micros(on.p95) / micros(off.p95).max(1.0),
        micros(on.mean) / micros(off.mean).max(1.0)
    );
}

/// Drive `SAMPLES` requests through a freshly built app and time each one.
async fn measure(
    config: AutumnConfig,
    pool: diesel_async::pooled_connection::deadpool::Pool<autumn_web::db::RuntimeConnection>,
    path: &str,
) -> Vec<Duration> {
    let client = TestApp::new()
        .config(config)
        .routes(routes![bench, plain])
        .with_db(pool)
        .build();

    for _ in 0..WARMUP {
        client.get(path).send().await.assert_status(200);
    }

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        let response = client.get(path).send().await;
        samples.push(started.elapsed());
        response.assert_status(200);
    }
    samples
}

fn base_config() -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    // The access log would dominate a microsecond-scale measurement and is not
    // what is being compared.
    config.log.access_log = false;
    config
}

#[tokio::test]
#[ignore = "benchmark: prints capture overhead; requires Docker (testcontainers)"]
async fn capture_overhead_on_a_successful_query_request() {
    let db = TestDb::shared().await;
    let url = db.url().to_owned();
    let dir = tempfile::tempdir().expect("tempdir");

    // Baseline: capture off, an ordinary pool against the same database —
    // exactly what an app that never sets `[failure_capture]` runs.
    let baseline_pool = db.pool();

    // Capture on: recording pool, marker on every checkout, scope per request.
    let mut on_config = base_config();
    on_config.failure_capture.enabled = true;
    on_config.failure_capture.dir = dir.path().to_string_lossy().into_owned();
    let on_pool = autumn_web::capsule::build_recording_pool(
        &url,
        POOL_SIZE,
        Duration::from_secs(10),
        "primary",
    )
    .expect("recording pool builds");

    // Both routes are measured, and the phases alternate rather than running
    // one after the other: a benchmark on shared hardware drifts, and a single
    // off-then-on ordering charges every bit of that drift to capture (or, if
    // the machine warms up, credits capture with it).
    let mut plain_off = Vec::with_capacity(SAMPLES * ROUNDS);
    let mut plain_on = Vec::with_capacity(SAMPLES * ROUNDS);
    let mut db_off = Vec::with_capacity(SAMPLES * ROUNDS);
    let mut db_on = Vec::with_capacity(SAMPLES * ROUNDS);
    for _ in 0..ROUNDS {
        plain_off.extend(measure(base_config(), baseline_pool.clone(), "/plain").await);
        plain_on.extend(measure(on_config.clone(), on_pool.clone(), "/plain").await);
        db_off.extend(measure(base_config(), baseline_pool.clone(), "/bench").await);
        db_on.extend(measure(on_config.clone(), on_pool.clone(), "/bench").await);
    }

    report(
        "no database (request-layer capture cost only)",
        &Summary::of(plain_off),
        &Summary::of(plain_on),
    );
    // Expect this one to come out *near zero or negative*, and not because
    // teeing a connection is free: with capture armed, `Db::checkout` sends
    // `SET statement_timeout` and the attribution marker as one simple-query
    // batch, where the uncaptured path issues the `SET` through the extended
    // protocol. Merging the marker into that round trip buys back about as much
    // as the tee costs. The honest reading is "capture does not measurably
    // change a database-touching request", not "capture makes your app faster".
    let off = Summary::of(db_off);
    let on = Summary::of(db_on);
    report("one bound SELECT through the pool", &off, &on);

    let written = std::fs::read_dir(dir.path()).map_or(0, std::iter::Iterator::count);
    assert_eq!(
        written, 0,
        "successful requests must leave no capsule behind — capture that writes on the \
         happy path is not measuring what this benchmark claims to measure"
    );

    let ratio = micros(on.p95) / micros(off.p95).max(1.0);
    assert!(
        ratio < MAX_P95_RATIO || off.p95 < RATIO_FLOOR,
        "capture multiplied p95 by {ratio:.2}x ({:.1}us -> {:.1}us); a factor that large \
         means capture added structural work per request, not overhead",
        micros(off.p95),
        micros(on.p95)
    );
}
