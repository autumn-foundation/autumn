//! The *newly-built* build of the hot-upgrade example (issue #1674).
//!
//! Its live-state shape gained an `upgrades` counter, so it declares both
//! shapes and a [`state_migration!`] between them. The migration is total by
//! construction: drop the `upgrades` line below and this binary does not
//! compile, which is the guarantee the BEAM's `code_change/3` cannot give.
//!
//! See `docs/guide/hot-upgrades.md`.

use std::sync::Arc;

use autumn_web::prelude::*;
use autumn_web::state_migration;
use autumn_web::upgrade::{LiveState, LiveStateHandle};
use serde::{Deserialize, Serialize};

/// The shape `hot-upgrade-v1` snapshots. Kept in this build purely so the
/// migration below has something to migrate *from*.
#[derive(Debug, Serialize, Deserialize)]
struct StatsV1 {
    hits: u64,
    note: String,
}

/// Version 2 of the live state: same counter and note, plus a count of how
/// many in-place upgrades this state has survived.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Stats {
    hits: u64,
    note: String,
    upgrades: u64,
}

impl LiveState for StatsV1 {
    const VERSION: u32 = 1;
}

impl LiveState for Stats {
    const VERSION: u32 = 2;
}

state_migration! {
    from StatsV1 as old => Stats {
        hits: old.hits,
        note: old.note,
        upgrades: 1,
    }
}

fn line(stats: &Stats) -> String {
    format!(
        "v2 hits={} note={} upgrades={} pid={}\n",
        stats.hits,
        stats.note,
        stats.upgrades,
        std::process::id()
    )
}

fn stats(state: &AppState) -> AutumnResult<Arc<LiveStateHandle<Stats>>> {
    state
        .live_state::<Stats>()
        .ok_or_else(|| AutumnError::internal_server_error_msg("live state is not installed"))
}

#[get("/")]
async fn index(State(state): State<AppState>) -> AutumnResult<String> {
    Ok(stats(&state)?.read(line))
}

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
        // Adopt a v1 snapshot through the migration above; a cold start uses
        // the default. A snapshot this build can neither decode nor migrate
        // aborts the upgrade instead of starting with an empty counter.
        .with_live_state_from::<StatsV1, _>(Stats::default())
        .run()
        .await;
}
