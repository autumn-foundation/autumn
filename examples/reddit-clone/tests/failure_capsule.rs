//! Failure-capsule capture for reddit-clone (0.7.0's record → `autumn replay`
//! subsystem — see `docs/guide/failure-capsules.md`).
//!
//! Two things are proved here, and one artifact is produced:
//!
//! 1. The committed `capsules` profile really does arm capture. It is a
//!    separate profile on purpose — capture is a deliberate decision, not a
//!    dev default — so a profile that silently stopped enabling it would make
//!    the README's walkthrough produce nothing at all.
//! 2. A failing request under that configuration leaves exactly one capsule,
//!    carrying the request, the outcome, and enough shape for `autumn replay`
//!    to have something to re-run. A 404 leaves none: capsules are written for
//!    failures, not for every request.
//! 3. The capsule the test captures is the same artifact committed at
//!    `capsules/dev-trigger-error.json`, which the README walks through. Run
//!    with `UPDATE_CAPSULE_FIXTURE=1` to re-record it after changing the route.
//!
//! The route under test — `/dev/trigger-error` — touches no database, so this
//! runs with no Docker and no Postgres. That is also what makes its capsule a
//! good committed example: it has no recorded database tape and no masked
//! credentials, so it carries nothing that a repository should not hold.
//!
//! # Running
//!
//! ```text
//! cargo test -p reddit-clone --test failure_capsule
//! UPDATE_CAPSULE_FIXTURE=1 cargo test -p reddit-clone --test failure_capsule
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use autumn_web::capsule::{Capsule, CapsuleBody, CapsuleOutcome};
use autumn_web::config::{AutumnConfig, MockEnv};
use autumn_web::routes;
use autumn_web::test::TestApp;

/// The capsule committed alongside the example, relative to the crate root.
const FIXTURE: &str = "capsules/dev-trigger-error.json";

/// The route the fixture was recorded from.
const RECORDED_ROUTE: &str = "/dev/trigger-error";

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE)
}

/// Load the committed `capsules` profile, the one the README tells you to run.
fn capsules_profile() -> AutumnConfig {
    let env = MockEnv::new()
        .with("AUTUMN_PROFILE", "capsules")
        .with("AUTUMN_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"));
    AutumnConfig::load_with_env(&env).expect("the capsules profile should load")
}

/// Every capsule file in `dir`, oldest first.
fn capsule_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
}

/// Capsules are written on a detached task, so the file lands shortly after
/// the response does.
async fn await_capsules(dir: &Path, expected: usize) -> Vec<PathBuf> {
    for _ in 0..100 {
        let paths = capsule_paths(dir);
        if paths.len() >= expected {
            return paths;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    capsule_paths(dir)
}

/// The `capsules` profile is what arms the whole feature: the capture layer,
/// the recording database pool and the recording clock. With
/// `enabled = false` none of it is installed and there is nothing to pay for —
/// which is also why the default profiles leave it off.
#[test]
fn capsules_profile_arms_capture_and_the_others_do_not() {
    let config = capsules_profile();
    assert!(
        config.failure_capture.enabled,
        "AUTUMN_PROFILE=capsules must turn capture on — the README's walkthrough \
         produces no capsule otherwise"
    );
    assert_eq!(config.failure_capture.dir, "tmp/autumn-capsules");
    assert_eq!(
        config.database.auto_migrate,
        Some(true),
        "`capsules` is a custom profile, so migrations do not auto-apply by \
         convention; without the explicit override the README's walkthrough \
         would need a dev boot first just to create the schema"
    );

    for profile in ["dev", "redis"] {
        let env = MockEnv::new()
            .with("AUTUMN_PROFILE", profile)
            .with("AUTUMN_MANIFEST_DIR", env!("CARGO_MANIFEST_DIR"));
        let other = AutumnConfig::load_with_env(&env)
            .unwrap_or_else(|error| panic!("the {profile} profile should load: {error}"));
        assert!(
            !other.failure_capture.enabled,
            "capture must stay opt-in; the {profile} profile turned it on"
        );
    }
}

/// The `capsules` profile also has to widen redaction past the built-in list.
///
/// Filter keys are matched by **equality** after normalization, never by
/// prefix — so `authorization` and `cookie` are covered out of the box while
/// this app's `Stripe-Signature` intake header is not, and would otherwise be
/// written into a capsule verbatim.
#[test]
fn capsules_profile_masks_this_apps_prefixed_secret_headers() {
    let filters = capsules_profile().log.filter_parameters;
    for header in ["stripe-signature", "x-api-key", "x-auth-token"] {
        assert!(
            filters.iter().any(|key| key == header),
            "the capsules profile must add `{header}` to [log] filter_parameters — \
             nothing in the default list matches a prefixed header name"
        );
    }
}

/// A 500 leaves exactly one capsule; a 404 leaves none.
///
/// This is the recording half of the record → replay loop the README walks
/// through, and (with `UPDATE_CAPSULE_FIXTURE=1`) the thing that produces the
/// committed fixture.
#[tokio::test]
async fn a_failing_request_records_one_replayable_capsule() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Boot with the committed profile's capture settings, redirected at a
    // tempdir so the test never writes into the repo's own `tmp/`.
    let mut config = capsules_profile();
    config.failure_capture.dir = dir.path().to_string_lossy().into_owned();
    // The profile carries a dev database URL this Docker-free test has no
    // Postgres for, and `/dev/trigger-error` needs none.
    config.database.primary_url = None;
    config.database.url = None;
    config.security.csrf.enabled = false;

    let client = TestApp::new()
        .config(config)
        .routes(routes![
            reddit_clone::routes::errors::trigger_error,
            reddit_clone::routes::errors::trigger_404,
        ])
        .build();

    // A 4xx writes nothing: capsules are for failures, and a client error is
    // not one. Send it first so a leaked capsule would be caught below.
    client
        .get("/dev/trigger-404")
        .send()
        .await
        .assert_status(404);

    client.get(RECORDED_ROUTE).send().await.assert_status(500);

    let paths = await_capsules(dir.path(), 1).await;
    assert_eq!(paths.len(), 1, "a 500 must leave exactly one capsule");

    // Capsules are written on a detached task, so `await_capsules` returns the
    // moment the first file lands. Settle before claiming the 404 wrote
    // nothing: without this, a 404 that *did* leak a capsule would still leave
    // a count of one here simply because its write had not finished yet.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let settled = capsule_paths(dir.path());
    assert_eq!(
        settled.len(),
        1,
        "a 4xx must write no capsule; found {settled:?}"
    );

    let json = std::fs::read_to_string(&paths[0]).expect("the capsule file is readable");
    let capsule = Capsule::from_json(&json).expect("the capsule parses");

    assert_eq!(capsule.request.method, "GET");
    assert_eq!(capsule.request.uri, RECORDED_ROUTE);
    assert_eq!(capsule.request.route.as_deref(), Some(RECORDED_ROUTE));
    match &capsule.outcome {
        CapsuleOutcome::Status { code, message, .. } => {
            assert_eq!(*code, 500);
            assert!(
                !message.is_empty(),
                "the capsule must carry the failure's own message, not a placeholder"
            );
        }
        other => panic!("a 500 must record a Status outcome, got {other:?}"),
    }
    assert!(
        !capsule.truncated,
        "a truncated capsule is refused by replay with exit code 2, so the \
         committed fixture must not be one: {:?}",
        capsule.notes
    );

    if std::env::var_os("UPDATE_CAPSULE_FIXTURE").is_some() {
        let path = fixture_path();
        std::fs::create_dir_all(path.parent().expect("fixture lives in a directory"))
            .expect("fixture directory is creatable");
        std::fs::write(&path, &json).expect("fixture is writable");
        eprintln!("re-recorded {}", path.display());
    }
}

/// The committed fixture is a real capsule, not a hand-written sketch: it
/// parses through the same `Capsule::from_json` the replay CLI uses, and
/// describes the request the README says it does.
#[test]
fn the_committed_fixture_is_a_real_replayable_capsule() {
    let path = fixture_path();
    let json = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "missing capsule fixture at {} ({error}) — re-record it with \
             UPDATE_CAPSULE_FIXTURE=1 cargo test -p reddit-clone --test failure_capsule",
            path.display()
        )
    });
    let capsule = Capsule::from_json(&json)
        .unwrap_or_else(|error| panic!("the committed fixture must parse: {error}"));

    assert_eq!(capsule.request.method, "GET");
    assert_eq!(capsule.request.uri, RECORDED_ROUTE);
    assert!(
        matches!(capsule.outcome, CapsuleOutcome::Status { code: 500, .. }),
        "the fixture records the 500 the README walks through, got {:?}",
        capsule.outcome
    );
    assert!(
        !capsule.truncated,
        "replay refuses a truncated capsule outright, so a truncated fixture \
         would make the README's walkthrough print `REFUSED`"
    );
    assert!(
        matches!(capsule.request.body, CapsuleBody::Absent),
        "the fixture is a GET with no body; a capsule whose body went \
         unrecorded (`Skipped`) is refused by replay, got {:?}",
        capsule.request.body
    );
}
