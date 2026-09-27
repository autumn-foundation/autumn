//! Issue #1674: two live-state shapes that declare the same `VERSION` are
//! indistinguishable on the wire, so a migration between them could never run —
//! the old payload would be handed to the new shape's `Deserialize` instead,
//! which is the silent state loss the migration exists to prevent. Forgetting
//! the version bump is therefore a build failure, not a runtime surprise.

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

// The shape changed but the version did not.
impl LiveState for Stats {
    const VERSION: u32 = 1;
}

state_migration! {
    from StatsV1 as old => Stats {
        hits: old.hits,
        upgrades: 1,
    }
}

fn main() {}
