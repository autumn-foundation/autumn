//! Issue #1674: there is no `..Default::default()` escape hatch in a state
//! migration — the macro grammar has no rule for a rest pattern, so "map the
//! fields I remembered and default the others" cannot be written at all.

use autumn_web::state_migration;
use autumn_web::upgrade::LiveState;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct StatsV1 {
    pub hits: u64,
}

#[derive(Default, Serialize, Deserialize)]
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
        ..Default::default()
    }
}

fn main() {}
