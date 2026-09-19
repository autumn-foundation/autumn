//! Issue #1674: a catch-all `_` arm would silently map every variant somebody
//! adds later. The migration grammar takes variant *names*, so it cannot be
//! written — the guarantee holds even against a developer who wants to opt out.

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
            _ => Mode::Slow { level: 0 },
        }
    }
}

fn main() {}
