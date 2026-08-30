//! Issue #1674: an enum-shaped state migration must map every variant of the
//! old shape. A missed variant is a non-exhaustive `match` — a build failure —
//! and a catch-all `_` arm is not expressible: the grammar takes variant
//! *names*, not patterns.

use autumn_web::state_migration;
use autumn_web::upgrade::LiveState;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub enum ModeV1 {
    Fast,
    Slow(u8),
}

#[derive(Serialize, Deserialize)]
pub enum Mode {
    Fast,
    Slow { level: u8 },
}

impl LiveState for ModeV1 {
    const VERSION: u32 = 1;
}

impl LiveState for Mode {
    const VERSION: u32 = 2;
}

state_migration! {
    from ModeV1 as old => Mode {
        match old {
            Fast => Mode::Fast,
            // `Slow` is never mapped.
        }
    }
}

fn main() {}
