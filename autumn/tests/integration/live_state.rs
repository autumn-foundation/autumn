//! The designated live-state block, exercised through the real request
//! pipeline (issue #1674).
//!
//! The *upgrade* itself — socket handoff, migration, cutover under load — is
//! proven end-to-end by `examples/hot-upgrade`'s `live_upgrade` test, which
//! drives two real binaries. What matters here is the half an app touches: a
//! handler reaching the block, and what the block does once an upgrade has
//! snapshotted it.

use autumn_web::test::TestApp;
use autumn_web::upgrade::{LiveState, LiveStateFrozen, LiveStateHandle, LiveStateRegistry};
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

fn stats(state: &AppState) -> std::sync::Arc<LiveStateHandle<Stats>> {
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

    // Stand in for the upgrade path: freeze and snapshot the block exactly as
    // `SIGUSR2` does before handing it to a successor.
    let state = client.state();
    let registry = state
        .extension::<LiveStateRegistry>()
        .expect("designating live state registers it for handover");
    let envelope = registry.freeze_and_snapshot(1).expect("snapshots");
    assert_eq!(envelope.version, 1);

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
    registry.unfreeze();
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
