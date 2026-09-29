//! Loom model-check of the global job client's first-init race.
//!
//! loom 0.7 has no `OnceLock`, and it cannot see inside `std::sync::OnceLock`,
//! so calling the production functions under loom would model nothing. Instead
//! this models the two candidate implementations of the `OnceLock` slot with
//! loom primitives and checks the invariant the fix restores: every value-write
//! must land in the single canonical published slot, never in a temporary slot
//! that loses the publish race and is silently dropped (the get-then-set bug).
//!
//! Default run exercises the FIXED (`get_or_init`) pattern and passes across all
//! interleavings. Set `JOB_CLIENT_LOOM_SHOW_BUG=1` to exercise the OLD
//! get-then-set pattern; loom then finds an interleaving where a write is lost
//! and the test fails. Run:
//!
//! ```text
//! cargo test -p autumn-web --test integration_tests chaos_job_client_loom
//! JOB_CLIENT_LOOM_SHOW_BUG=1 cargo test -p autumn-web --test integration_tests chaos_job_client_loom
//! ```

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex, RwLock};
use loom::thread;

/// Identifier of an installed job client. `0` models the "cleared" state.
type ClientId = usize;

/// One backing `RwLock<Option<ClientId>>`, tagged with a unique id so the test
/// can detect writes that land in a slot that never becomes canonical.
struct Slot {
    id: usize,
    value: RwLock<Option<ClientId>>,
}

/// Models `OnceLock<RwLock<Option<ClientId>>>`. `published` is guarded by a
/// `Mutex` so get/set are each linearizable but SEPARATE ops (letting the buggy
/// get-then-set composition interleave). `writes` records the slot id of every
/// value-write for the end-of-model invariant check.
struct OnceCell {
    published: Mutex<Option<Arc<Slot>>>,
    next_id: AtomicUsize,
    writes: Mutex<Vec<usize>>,
}

impl OnceCell {
    /// Create an empty cell with no published slot.
    fn new() -> Self {
        Self {
            published: Mutex::new(None),
            next_id: AtomicUsize::new(0),
            writes: Mutex::new(Vec::new()),
        }
    }

    /// Build a fresh, uniquely-tagged slot (not yet published).
    fn new_slot(&self, initial: Option<ClientId>) -> Arc<Slot> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        Arc::new(Slot {
            id,
            value: RwLock::new(initial),
        })
    }

    /// Read the currently published slot, if any.
    fn get(&self) -> Option<Arc<Slot>> {
        self.published.lock().unwrap().clone()
    }

    /// Publish `slot` iff nothing is published yet. Mirrors `OnceLock::set`.
    fn set(&self, slot: Arc<Slot>) -> Result<(), ()> {
        let mut published = self.published.lock().unwrap();
        if published.is_some() {
            Err(())
        } else {
            *published = Some(slot);
            drop(published);
            Ok(())
        }
    }

    /// Return the canonical published slot, creating and publishing one via
    /// `make` iff none exists yet. Mirrors `OnceLock::get_or_init`.
    fn get_or_init(&self, make: impl FnOnce() -> Arc<Slot>) -> Arc<Slot> {
        let mut published = self.published.lock().unwrap();
        published.get_or_insert_with(make).clone()
    }

    /// Record that a value-write targeted the slot with the given id.
    fn record_write(&self, id: usize) {
        self.writes.lock().unwrap().push(id);
    }

    /// Perform a value-write through `slot`, recording it first.
    fn write_through(&self, slot: &Arc<Slot>, v: Option<ClientId>) {
        self.record_write(slot.id);
        *slot.value.write().unwrap() = v;
    }
}

/// OLD, buggy get-then-set install. When no slot is published yet it builds a
/// fresh slot, records its write, then *may* lose the `set` race and silently
/// drop that slot — losing the write it just recorded.
fn buggy_install(cell: &OnceCell, client: ClientId) {
    if let Some(slot) = cell.get() {
        cell.write_through(&slot, Some(client));
    } else {
        let slot = cell.new_slot(Some(client));
        cell.record_write(slot.id);
        let _ = cell.set(slot);
    }
}

/// OLD, buggy get-then-set clear (same shape as `buggy_install`, writing the
/// cleared state).
fn buggy_clear(cell: &OnceCell) {
    if let Some(slot) = cell.get() {
        cell.write_through(&slot, None);
    } else {
        let slot = cell.new_slot(None);
        cell.record_write(slot.id);
        let _ = cell.set(slot);
    }
}

/// FIXED install: `get_or_init` returns the single canonical slot; every write
/// goes to it.
fn fixed_install(cell: &OnceCell, client: ClientId) {
    let slot = cell.get_or_init(|| cell.new_slot(None));
    cell.write_through(&slot, Some(client));
}

/// FIXED clear: `get_or_init` returns the single canonical slot; every write
/// goes to it.
fn fixed_clear(cell: &OnceCell) {
    let slot = cell.get_or_init(|| cell.new_slot(None));
    cell.write_through(&slot, None);
}

#[test]
fn global_job_client_first_init_race() {
    let show_bug = std::env::var_os("JOB_CLIENT_LOOM_SHOW_BUG").is_some();
    loom::model(move || {
        let cell = Arc::new(OnceCell::new());

        let c1 = cell.clone();
        let t1 = thread::spawn(move || {
            if show_bug {
                buggy_install(&c1, 1);
            } else {
                fixed_install(&c1, 1);
            }
        });

        let c2 = cell.clone();
        let t2 = thread::spawn(move || {
            if show_bug {
                buggy_clear(&c2);
            } else {
                fixed_clear(&c2);
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();

        // Invariant: every value-write landed in the canonical published slot.
        // `get_or_init` guarantees one canonical slot; get-then-set can drop a
        // write into a temporary slot that loses the publish race.
        let canonical = cell.published.lock().unwrap().as_ref().map(|s| s.id);
        let writes = cell.writes.lock().unwrap();
        for &w in writes.iter() {
            assert_eq!(
                Some(w),
                canonical,
                "a value-write landed in a non-canonical slot -> lost update (get-then-set race)"
            );
        }
    });
}
