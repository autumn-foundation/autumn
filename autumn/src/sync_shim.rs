//! The sync primitives of the modules that loom and Shuttle check.
//!
//! In a normal build these are the `std::sync` types. Under
//! `RUSTFLAGS="--cfg loom_models"` they are loom's, and under
//! `RUSTFLAGS="--cfg shuttle_models"` they are Shuttle's. Thus the model
//! checks in `tests/loom_models.rs` and `tests/shuttle_models.rs` run the
//! shipped code, not a copy of it: each lock and atomic in it is a point where
//! the checker can switch threads.
//!
//! A module uses this shim only when every lock and atomic in it is created
//! at run time. A `static` would be created outside the checker, and the
//! loom and Shuttle types panic there.

#[cfg(all(loom_models, not(shuttle_models)))]
pub use loom::sync::{
    Mutex, MutexGuard,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
#[cfg(shuttle_models)]
pub use shuttle::sync::{
    Mutex, MutexGuard,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
#[cfg(not(any(loom_models, shuttle_models)))]
pub use std::sync::{
    Mutex, MutexGuard,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
