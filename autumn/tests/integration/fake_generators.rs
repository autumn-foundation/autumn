//! Tests for the deterministic fake-data generators in `autumn_web::fake`.

use autumn_web::fake;

/// Draw a mixed sequence of values, exercising most generators, so determinism
/// covers the full API surface (each draw advances the shared RNG).
fn sample_sequence() -> Vec<String> {
    let mut out = Vec::new();
    for _ in 0..25 {
        out.push(fake::name());
        out.push(fake::email());
        out.push(fake::username());
        out.push(fake::sentence());
        out.push(fake::paragraph());
        out.push(fake::url());
        out.push(fake::word());
        out.push(fake::boolean().to_string());
        out.push(fake::int_range(1, 10).to_string());
        out.push(fake::decimal().to_string());
        out.push(fake::decimal_f64().to_string());
        out.push(fake::recent_datetime().to_rfc3339());
        out.push(fake::uuid().to_string());
    }
    out
}

#[test]
fn reseed_makes_output_deterministic() {
    let _guard = fake::test_serial_guard();
    fake::reseed(42);
    let first = sample_sequence();
    fake::reseed(42);
    let second = sample_sequence();
    assert_eq!(
        first, second,
        "same seed must reproduce identical sequences"
    );
}

#[test]
fn different_seeds_diverge() {
    let _guard = fake::test_serial_guard();
    fake::reseed(1);
    let a = sample_sequence();
    fake::reseed(2);
    let b = sample_sequence();
    assert_ne!(a, b, "different seeds should produce different sequences");
}

/// Regression test for the determinism bug where the RNG was thread-local:
/// under the multi-thread runtime a seeded run migrated threads, each fresh
/// thread re-seeded itself independently from the same seed, and the sequence
/// restarted — breaking reproducibility and producing duplicate rows.
///
/// With a single process-global seeded RNG, `reseed(42)` followed by draws
/// spread across many concurrently-spawned tasks yields one shared sequence, so
/// the *multiset* of all drawn values is identical run-to-run no matter how the
/// tasks are scheduled. On the old thread-local design the spawned tasks would
/// each draw from their own thread's RNG (seeded from OS entropy, since
/// `AUTUMN_FAKE_SEED` is unset here), so the two runs diverge and this fails.
///
/// The per-draw unit is [`fake::uuid`], which consumes exactly one *atomic*
/// 16-byte pull from the RNG under a single lock. That atomicity is what makes
/// the multiset deterministic under interleaving: whichever task wins the lock,
/// the set of 16-byte blocks consumed is always the first `TASKS * PER_TASK`
/// blocks of the seeded stream. (A helper like `sentence()` that makes a
/// *variable number* of separate locked draws would let concurrent tasks
/// assemble strings from non-contiguous stream values, so its multiset is
/// legitimately non-deterministic under interleaving — not a defect in the
/// shared RNG.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// The serial guard is intentionally held across the `.await`s below to keep the
// whole run isolated from other reseed-based tests; the spawned tasks never take
// this lock, so there is no deadlock risk.
#[allow(clippy::await_holding_lock)]
async fn seeded_rng_is_deterministic_across_threads_and_tasks() {
    // Reseed once, then draw `TASKS * PER_TASK` UUIDs spread across tasks that
    // yield between draws to encourage migration across worker threads. Returns
    // the sorted multiset of everything drawn this run. (Defined before any
    // statement to satisfy `clippy::items_after_statements`.)
    async fn run() -> Vec<String> {
        const TASKS: usize = 8;
        const PER_TASK: usize = 64;

        fake::reseed(42);
        let mut handles = Vec::with_capacity(TASKS);
        for _ in 0..TASKS {
            handles.push(tokio::spawn(async {
                let mut drawn = Vec::with_capacity(PER_TASK);
                for _ in 0..PER_TASK {
                    // Yield so the task can be resumed on a different worker
                    // thread — the exact scenario that broke the thread-local
                    // design.
                    tokio::task::yield_now().await;
                    drawn.push(fake::uuid().to_string());
                }
                drawn
            }));
        }

        let mut all = Vec::with_capacity(TASKS * PER_TASK);
        for handle in handles {
            all.extend(handle.await.expect("fake draw task panicked"));
        }
        all.sort();
        all
    }

    let _guard = fake::test_serial_guard();
    let first = run().await;
    let second = run().await;

    assert_eq!(first.len(), 8 * 64, "should draw every value");
    // (a) No duplicates — distinct rows even across threads (#1343 AC3).
    let distinct: std::collections::HashSet<&String> = first.iter().collect();
    assert_eq!(
        distinct.len(),
        first.len(),
        "a seeded multi-threaded batch must have no duplicate values"
    );
    // (b) Reproducible run-to-run — one shared sequence regardless of scheduling
    // (#1343 AC5). This is what fails on the old thread-local design.
    assert_eq!(
        first, second,
        "a seeded run must draw one reproducible sequence across all threads/tasks"
    );
}

#[test]
fn name_has_high_cardinality() {
    let _guard = fake::test_serial_guard();
    fake::reseed(7);
    let mut names = std::collections::HashSet::new();
    for _ in 0..200 {
        names.insert(fake::name());
    }
    assert!(
        names.len() > 100,
        "expected many distinct names, got {}",
        names.len()
    );
}

#[test]
fn sentence_has_high_cardinality() {
    let _guard = fake::test_serial_guard();
    fake::reseed(7);
    let mut sentences = std::collections::HashSet::new();
    for _ in 0..200 {
        sentences.insert(fake::sentence());
    }
    assert!(
        sentences.len() > 150,
        "expected many distinct sentences, got {}",
        sentences.len()
    );
}

#[test]
fn int_range_stays_within_bounds() {
    let _guard = fake::test_serial_guard();
    fake::reseed(7);
    for _ in 0..1000 {
        let v = fake::int_range(1, 10);
        assert!((1..=10).contains(&v), "int_range out of bounds: {v}");
    }
    // Degenerate range collapses to lo.
    assert_eq!(fake::int_range(5, 5), 5);
    assert_eq!(fake::int_range(9, 3), 9);
}

#[test]
fn email_contains_exactly_one_at() {
    let _guard = fake::test_serial_guard();
    fake::reseed(7);
    for _ in 0..100 {
        let e = fake::email();
        assert_eq!(e.matches('@').count(), 1, "bad email: {e}");
    }
}

#[test]
fn url_is_https() {
    let _guard = fake::test_serial_guard();
    fake::reseed(7);
    for _ in 0..100 {
        let u = fake::url();
        assert!(u.starts_with("https://"), "bad url: {u}");
    }
}

#[test]
fn username_is_lowercase_and_spaceless() {
    let _guard = fake::test_serial_guard();
    fake::reseed(7);
    for _ in 0..100 {
        let u = fake::username();
        assert!(!u.contains(' '), "username has space: {u}");
        assert_eq!(u, u.to_lowercase(), "username not lowercase: {u}");
    }
}

#[test]
fn uuids_are_distinct_and_v4() {
    let _guard = fake::test_serial_guard();
    fake::reseed(7);
    let mut seen = std::collections::HashSet::new();
    for _ in 0..500 {
        let id = fake::uuid();
        assert_eq!(id.get_version_num(), 4, "uuid not v4: {id}");
        assert!(seen.insert(id), "duplicate uuid: {id}");
    }
}

#[test]
fn words_zero_is_empty() {
    let _guard = fake::test_serial_guard();
    fake::reseed(7);
    assert_eq!(fake::words(0), "");
    assert!(!fake::words(3).is_empty());
}
