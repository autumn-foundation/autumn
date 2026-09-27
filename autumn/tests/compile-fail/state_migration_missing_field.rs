//! Issue #1674: an in-place upgrade's old→new state migration must be total.
//! Omitting a field mapping is a `cargo build` failure, not a field that
//! silently arrives at its `Default` after the upgrade.

use autumn_web::state_migration;
use autumn_web::upgrade::LiveState;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct StatsV1 {
    pub hits: u64,
}

#[derive(Serialize, Deserialize)]
pub struct Stats {
    pub hits: u64,
    pub upgrades: u64,
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
        // `upgrades` is never mapped.
    }
}

fn main() {}
