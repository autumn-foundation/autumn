//! The *running* build of the hot-upgrade example (issue #1674).
//!
//! Start it, put a value in its live state, then upgrade it in place to
//! `hot-upgrade-v2` — a binary whose live-state shape has an extra field —
//! without dropping a connection or the value:
//!
//! ```console
//! $ AUTUMN_UPGRADE_BINARY=target/debug/hot-upgrade-v2 cargo run -p hot-upgrade --bin hot-upgrade-v1
//! $ curl localhost:3000/note/hello    # v1 hits=1 note=hello upgrades=0
//! $ kill -USR2 $(pgrep -f hot-upgrade-v1)
//! $ curl localhost:3000/              # v2 hits=1 note=hello upgrades=1
//! ```
//!
//! See `docs/guide/hot-upgrades.md`.

use std::sync::Arc;

use autumn_web::prelude::*;
use autumn_web::upgrade::{LiveState, LiveStateHandle};
use serde::{Deserialize, Serialize};

/// The block of in-memory state this app designates to survive an upgrade.
///
/// Version 1: a request counter and an operator-set note.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Stats {
    hits: u64,
    note: String,
}

impl LiveState for Stats {
    const VERSION: u32 = 1;
}

/// Render the state the way both builds do, so a client can watch the shape
/// (and the version that served it) change across the cutover.
fn line(stats: &Stats) -> String {
    format!(
        "v1 hits={} note={} upgrades=0 pid={}\n",
        stats.hits,
        stats.note,
        std::process::id()
    )
}

fn stats(state: &AppState) -> AutumnResult<Arc<LiveStateHandle<Stats>>> {
    state
        .live_state::<Stats>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("live state is not installed"))
}

/// Read the live state. Reads keep working while the process drains.
#[get("/")]
async fn index(State(state): State<AppState>) -> AutumnResult<String> {
    Ok(stats(&state)?.read(line))
}

/// Count a hit. Refused with `503` once the state has been snapshotted for an
/// upgrade — the retry lands on the successor rather than being lost here.
#[get("/bump")]
async fn bump(State(state): State<AppState>) -> AutumnResult<String> {
    let handle = stats(&state)?;
    handle
        .write(|s| {
            s.hits += 1;
            line(s)
        })
        .map_err(|frozen| AutumnError::service_unavailable_msg(frozen.to_string()))
}

/// Put a value in the live state — the value the upgrade must carry across.
#[get("/note/{value}")]
async fn set_note(State(state): State<AppState>, value: Path<String>) -> AutumnResult<String> {
    let handle = stats(&state)?;
    handle
        .write(|s| {
            s.hits += 1;
            s.note = value.to_string();
            line(s)
        })
        .map_err(|frozen| AutumnError::service_unavailable_msg(frozen.to_string()))
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![index, bump, set_note])
        .with_live_state(Stats::default())
        .run()
        .await;
}
