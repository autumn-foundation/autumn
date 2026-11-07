//! The fence check before each write to a replica.
//!
//! Core owns every write to the replica namespace of a fleet database. Each
//! write goes through a [`FencedDestination`], which runs [`ShipFence::check`]
//! before each `put`. So no engine can skip the check, in any ship mode.
//!
//! ```text
//! put ─► stopped?                     yes ─► Rejected
//!     ─► a newer epoch this host saw? yes ─► Fenced (no list)
//!     ─► list <root>/epochs/
//!        a newer epoch marker?        yes ─► tell ownership, Fenced
//!     ─► inner.put
//! ```
//!
//! A writer with no epoch (no ownership) has no epoch fence. Reads and
//! deletes are not fenced: a purge and a prune must still work.

// autumn-panic-gate: durability-critical module — production code path must be
// panic-free. See CONTRIBUTING.md "Request-path panic gate". Justify exceptions
// with #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use super::destination::{DestinationError, ReplicaDestination};
use super::segment;

/// Returns `true` when the writes must stop.
pub type StopWhen = Arc<dyn Fn() -> bool + Send + Sync>;

/// Returns the newest epoch this host saw for the database.
pub type KnownEpoch = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Called with the newer epoch the fence found at the destination.
pub type OnFenced = Arc<dyn Fn(u64) + Send + Sync>;

/// The fence of one database's replica.
#[derive(Clone)]
pub struct ShipFence {
    /// Lists the epoch markers.
    destination: Arc<dyn ReplicaDestination>,
    /// The replica prefix of the database.
    root: String,
    /// The epoch the database opened under. `None` without ownership.
    epoch: Option<u64>,
    stop: Option<StopWhen>,
    known: Option<KnownEpoch>,
    on_fenced: Option<OnFenced>,
}

impl fmt::Debug for ShipFence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShipFence")
            .field("root", &self.root)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

impl ShipFence {
    /// The fence of the replica under `root`, for a writer of `epoch`.
    /// `destination` lists the epoch markers.
    #[must_use]
    pub fn new(
        destination: Arc<dyn ReplicaDestination>,
        root: impl Into<String>,
        epoch: Option<u64>,
    ) -> Self {
        Self {
            destination,
            root: root.into(),
            epoch,
            stop: None,
            known: None,
            on_fenced: None,
        }
    }

    /// Refuse each write while `stop` returns `true`.
    #[must_use]
    pub fn stopped_when(mut self, stop: StopWhen) -> Self {
        self.stop = Some(stop);
        self
    }

    /// Refuse each write when `known` returns an epoch above the writer's.
    /// That check needs no list.
    #[must_use]
    pub fn known_epoch(mut self, known: KnownEpoch) -> Self {
        self.known = Some(known);
        self
    }

    /// Call `hook` with the newer epoch when the destination shows one.
    #[must_use]
    pub fn on_fenced(mut self, hook: OnFenced) -> Self {
        self.on_fenced = Some(hook);
        self
    }

    /// The epoch of the writer.
    #[must_use]
    pub const fn epoch(&self) -> Option<u64> {
        self.epoch
    }

    /// The check before one write. One list at the destination when the
    /// writer has an epoch.
    ///
    /// # Errors
    ///
    /// [`DestinationError::Rejected`] when the writes are stopped.
    /// [`DestinationError::Fenced`] when a newer epoch owns the replica.
    /// The list error when the epochs cannot be read: no write goes
    /// unchecked.
    pub fn check(&self) -> Result<(), DestinationError> {
        if self.stop.as_ref().is_some_and(|stop| stop()) {
            return Err(DestinationError::Rejected {
                detail: format!(
                    "writes to the replica {:?} are stopped: its database closed or lost its slot",
                    self.root
                ),
            });
        }
        let Some(epoch) = self.epoch else {
            return Ok(());
        };
        if let Some(newest) = self
            .known
            .as_ref()
            .map(|known| known())
            .filter(|newest| *newest > epoch)
        {
            return Err(DestinationError::Fenced { epoch, newest });
        }
        let newest = self
            .destination
            .list(&segment::epochs_prefix(&self.root))?
            .iter()
            .filter_map(|key| segment::parse_epoch_marker_key(&self.root, key))
            .max();
        match newest {
            Some(newest) if newest > epoch => {
                if let Some(hook) = &self.on_fenced {
                    hook(newest);
                }
                Err(DestinationError::Fenced { epoch, newest })
            }
            _ => Ok(()),
        }
    }
}

/// A destination that runs a [`ShipFence`] before each `put`.
#[derive(Clone)]
pub struct FencedDestination {
    inner: Arc<dyn ReplicaDestination>,
    fence: ShipFence,
}

impl fmt::Debug for FencedDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FencedDestination")
            .field("destination", &self.inner.describe())
            .field("fence", &self.fence)
            .finish()
    }
}

impl FencedDestination {
    /// Writes to `inner` after each check of `fence`.
    #[must_use]
    pub fn new(inner: Arc<dyn ReplicaDestination>, fence: ShipFence) -> Self {
        Self { inner, fence }
    }

    /// The fence.
    #[must_use]
    pub const fn fence(&self) -> &ShipFence {
        &self.fence
    }
}

impl ReplicaDestination for FencedDestination {
    fn describe(&self) -> String {
        self.inner.describe()
    }

    fn put(&self, key: &str, body: &[u8]) -> Result<(), DestinationError> {
        self.fence.check()?;
        self.inner.put(key, body)
    }

    fn put_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        self.fence.check()?;
        self.inner.put_file(key, path)
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, DestinationError> {
        self.inner.get(key)
    }

    fn get_to_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        self.inner.get_to_file(key, path)
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, DestinationError> {
        self.inner.list(prefix)
    }

    fn delete(&self, key: &str) -> Result<(), DestinationError> {
        self.inner.delete(key)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::*;
    use crate::replication::destination::{FileDestination, ReplicaDestination};
    use crate::replication::segment;

    const ROOT: &str = "app/fleet/tenant/acme";

    fn dest(dir: &std::path::Path) -> Arc<dyn ReplicaDestination> {
        Arc::new(FileDestination::new(dir).unwrap())
    }

    fn mark(dest: &Arc<dyn ReplicaDestination>, epoch: u64) {
        dest.put(&segment::epoch_marker_key(ROOT, epoch), b"{}")
            .unwrap();
    }

    #[test]
    fn a_put_with_no_newer_epoch_goes_through() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = dest(tmp.path());
        mark(&inner, 3);
        let fenced = FencedDestination::new(
            Arc::clone(&inner),
            ShipFence::new(Arc::clone(&inner), ROOT, Some(3)),
        );
        fenced.put(&format!("{ROOT}/a"), b"x").unwrap();
        assert_eq!(inner.get(&format!("{ROOT}/a")).unwrap(), b"x");
    }

    #[test]
    fn a_newer_epoch_at_the_destination_refuses_every_put() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = dest(tmp.path());
        mark(&inner, 3);
        mark(&inner, 4);
        let seen = Arc::new(AtomicU64::new(0));
        let hook = Arc::clone(&seen);
        let fenced = FencedDestination::new(
            Arc::clone(&inner),
            ShipFence::new(Arc::clone(&inner), ROOT, Some(3))
                .on_fenced(Arc::new(move |newest| hook.store(newest, Ordering::SeqCst))),
        );
        let file = tmp.path().join("body");
        std::fs::write(&file, b"y").unwrap();
        assert_eq!(
            fenced.put(&format!("{ROOT}/a"), b"x"),
            Err(DestinationError::Fenced {
                epoch: 3,
                newest: 4
            })
        );
        assert!(matches!(
            fenced.put_file(&format!("{ROOT}/b"), &file),
            Err(DestinationError::Fenced { .. })
        ));
        assert_eq!(
            seen.load(Ordering::SeqCst),
            4,
            "ownership hears the newer epoch"
        );
        assert!(inner.list(&format!("{ROOT}/a")).unwrap().is_empty());
        assert!(inner.list(&format!("{ROOT}/b")).unwrap().is_empty());
        // Reads and deletes are not fenced.
        assert!(fenced.list(&segment::epochs_prefix(ROOT)).is_ok());
        assert!(fenced.delete(&format!("{ROOT}/a")).is_ok());
    }

    #[test]
    fn a_newer_epoch_this_host_knows_refuses_with_no_list() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = dest(tmp.path());
        let fence = ShipFence::new(Arc::clone(&inner), ROOT, Some(3)).known_epoch(Arc::new(|| 5));
        assert_eq!(
            fence.check(),
            Err(DestinationError::Fenced {
                epoch: 3,
                newest: 5
            })
        );
    }

    #[test]
    fn a_stopped_fence_refuses_and_a_fence_with_no_epoch_only_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = dest(tmp.path());
        mark(&inner, 9);
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopped);
        let fence = ShipFence::new(Arc::clone(&inner), ROOT, None)
            .stopped_when(Arc::new(move || flag.load(Ordering::SeqCst)));
        // No epoch: no ownership, so no epoch fence.
        assert_eq!(fence.check(), Ok(()));
        stopped.store(true, Ordering::SeqCst);
        assert!(matches!(
            fence.check(),
            Err(DestinationError::Rejected { .. })
        ));
    }

    #[test]
    fn a_destination_that_cannot_list_refuses_the_put() {
        struct Broken;
        impl ReplicaDestination for Broken {
            fn describe(&self) -> String {
                "broken".to_owned()
            }
            fn put(&self, _: &str, _: &[u8]) -> Result<(), DestinationError> {
                Ok(())
            }
            fn put_file(&self, _: &str, _: &std::path::Path) -> Result<(), DestinationError> {
                Ok(())
            }
            fn get(&self, key: &str) -> Result<Vec<u8>, DestinationError> {
                Err(DestinationError::NotFound {
                    key: key.to_owned(),
                })
            }
            fn get_to_file(&self, key: &str, _: &std::path::Path) -> Result<(), DestinationError> {
                Err(DestinationError::NotFound {
                    key: key.to_owned(),
                })
            }
            fn list(&self, _: &str) -> Result<Vec<String>, DestinationError> {
                Err(DestinationError::Io {
                    op: "list",
                    detail: "down".to_owned(),
                })
            }
            fn delete(&self, _: &str) -> Result<(), DestinationError> {
                Ok(())
            }
        }
        let inner: Arc<dyn ReplicaDestination> = Arc::new(Broken);
        let fenced = FencedDestination::new(
            Arc::clone(&inner),
            ShipFence::new(Arc::clone(&inner), ROOT, Some(1)),
        );
        assert!(matches!(
            fenced.put("k", b"x"),
            Err(DestinationError::Io { .. })
        ));
    }

    #[test]
    fn the_fence_describes_the_inner_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let inner = dest(tmp.path());
        let fenced = FencedDestination::new(
            Arc::clone(&inner),
            ShipFence::new(Arc::clone(&inner), ROOT, Some(1)),
        );
        assert!(fenced.describe().contains(&inner.describe()));
    }
}
