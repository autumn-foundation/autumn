//! Fixed-memory stream sketches for Autumn apps.
//!
//! Each module answers one question about an endless stream in memory chosen
//! up front:
//!
//! - [`heavy_hitters`]: which keys are most of the traffic?
//! - [`frequency`]: how often has *this* key been seen?
//! - [`distinct`]: how many distinct keys?
//! - [`quantiles`]: what are p50, p99 and p99.9?
//! - [`reservoir`]: a fair sample of `k` items.
//! - [`bloom`]: have we seen this key before?
//! - [`minhash`]: how alike are these two sets?
//!
//! Each has a shared form behind a mutex, to put in an `Arc` and record from
//! every handler. Sketches of one shape merge, so per-replica sketches add up
//! to the fleet's answer. The crate is std only and does not depend on
//! `autumn-web`.

pub mod bloom;
pub mod distinct;
pub mod frequency;
pub mod heavy_hitters;
pub mod minhash;
pub mod quantiles;
pub mod reservoir;
