//! The designated live-state block, exercised through the real request
//! pipeline (issue #1674).
//!
//! The *upgrade* itself — socket handoff, migration, cutover under load — is
//! proven end-to-end by `examples/hot-upgrade`'s `live_upgrade` test, which
//! drives two real binaries. What matters here is the half an app touches: a
//! handler reaching the block, and what the block does once an upgrade has
//! snapshotted it.

use autumn_web::test::TestApp;
use autumn_web::upgrade::{LiveState, LiveStateFrozen, LiveStateHandle};
use autumn_web::{AppState, get, routes};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stats {
    hits: u64,
    note: String,
}

impl LiveState for Stats {
    const VERSION: u32 = 1;
}

fn stats(state: &AppState) -> LiveStateHandle<Stats> {
    state
        .live_state::<Stats>()
        .expect("the designated live state is installed")
}

#[get("/read")]
async fn read(autumn_web::extract::State(state): autumn_web::extract::State<AppState>) -> String {
    stats(&state).read(|s| format!("hits={} note={}", s.hits, s.note))
}

#[get("/write")]
async fn write(autumn_web::extract::State(state): autumn_web::extract::State<AppState>) -> String {
    stats(&state)
        .write(|s| {
            s.hits += 1;
            format!("hits={}", s.hits)
        })
        .unwrap_or_else(|frozen: LiveStateFrozen| frozen.to_string())
}

#[get("/absent")]
async fn absent(autumn_web::extract::State(state): autumn_web::extract::State<AppState>) -> String {
    format!("{}", state.live_state::<Stats>().is_some())
}

#[tokio::test]
async fn a_handler_reads_and_writes_the_designated_live_state() {
    let client = TestApp::new()
        .routes(routes![read, write])
        .with_live_state(Stats {
            hits: 0,
            note: "seeded".to_owned(),
        })
        .build();

    client.get("/write").send().await.assert_ok();
    let response = client.get("/write").send().await;
    response.assert_ok();
    assert_eq!(response.text(), "hits=2");

    let response = client.get("/read").send().await;
    response.assert_ok();
    assert_eq!(response.text(), "hits=2 note=seeded");
}

#[tokio::test]
async fn a_snapshotted_block_refuses_writes_but_still_serves_reads() {
    let client = TestApp::new()
        .routes(routes![read, write])
        .with_live_state(Stats {
            hits: 7,
            note: "carried".to_owned(),
        })
        .build();

    // Stand in for the upgrade path: freeze the block exactly as `SIGUSR2`
    // does before handing it to a successor.
    let state = client.state();
    let handle = state
        .live_state::<Stats>()
        .expect("designating live state installs it");
    handle.freeze_for_test();

    // A write is refused rather than accepted and thrown away with the process.
    let response = client.get("/write").send().await;
    response.assert_ok();
    assert!(
        response.text().contains("frozen"),
        "expected a frozen-state refusal, got {:?}",
        response.text()
    );

    // Reads keep working: this process is still serving while it drains, and
    // the value the successor will adopt is the one it reports.
    let response = client.get("/read").send().await;
    response.assert_ok();
    assert_eq!(response.text(), "hits=7 note=carried");

    // An abandoned upgrade hands the state back.
    handle.unfreeze_for_test();
    let response = client.get("/write").send().await;
    response.assert_ok();
    assert_eq!(response.text(), "hits=8");
}

#[tokio::test]
async fn an_app_that_designates_nothing_has_no_live_state() {
    let client = TestApp::new().routes(routes![absent]).build();

    let response = client.get("/absent").send().await;
    response.assert_ok();
    assert_eq!(response.text(), "false");
}

/// A successor's adopted state becomes writable only once the handover is
/// irreversible — never before.
///
/// The window this closes is narrow and needs two processes to reach, so it is
/// pinned where a refactor would break it: `run()` must unfreeze *after* the
/// checks that can still abandon the upgrade, and *before* the signal that
/// releases the predecessor to drain. Ordered the other way, a write the
/// successor acknowledged could be discarded by an upgrade that then failed —
/// the exact loss the freeze exists to prevent.
#[test]
fn an_adopted_block_is_unfrozen_after_the_last_abort_point_and_before_the_predecessor_is_released()
{
    let source = include_str!("../../src/app.rs");
    let ready_path = source
        .split_once("state.probes().mark_startup_complete();")
        .expect("run() marks startup complete")
        .1
        .split_once("signal_serve_ready(")
        .expect("run() signals serve readiness")
        .0;

    let verify = ready_path
        .find("verify_handover_complete()")
        .expect("the handover is verified before the predecessor is released");
    let unfreeze = ready_path
        .find("unfreeze_adopted_live_state(&state)")
        .expect("the adopted live state is unfrozen before the predecessor is released");
    let release = ready_path
        .find("signal_upgrade_ready()")
        .expect("the predecessor is released");

    assert!(
        verify < unfreeze,
        "the adopted state must stay frozen until the handover is known to be complete"
    );
    assert!(
        unfreeze < release,
        "the successor must be writable before it releases the predecessor, or the first \
         write after the cutover lands on a process that refuses it"
    );
}
