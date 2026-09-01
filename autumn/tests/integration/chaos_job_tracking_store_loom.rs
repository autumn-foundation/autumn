//! Loom model-check of the global job tracking store's get-then-set race.
//!
//! Models the `OnceLock` get-then-set bug found in `clear_global_tracking_store`.

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex, RwLock};
use loom::thread;

type StoreId = usize;

struct Slot {
    id: usize,
    value: RwLock<Option<StoreId>>,
}

struct OnceCell {
    published: Mutex<Option<Arc<Slot>>>,
    next_id: AtomicUsize,
    writes: Mutex<Vec<usize>>,
}

impl OnceCell {
    fn new() -> Self {
        Self {
            published: Mutex::new(None),
            next_id: AtomicUsize::new(0),
            writes: Mutex::new(Vec::new()),
        }
    }

    fn new_slot(&self, initial: Option<StoreId>) -> Arc<Slot> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        Arc::new(Slot {
            id,
            value: RwLock::new(initial),
        })
    }

    fn get(&self) -> Option<Arc<Slot>> {
        self.published.lock().unwrap().clone()
    }

    fn set(&self, slot: Arc<Slot>) -> Result<(), ()> {
        let mut published = self.published.lock().unwrap();
        if published.is_some() {
            Err(())
        } else {
            *published = Some(slot);
            Ok(())
        }
    }

    fn get_or_init(&self, make: impl FnOnce() -> Arc<Slot>) -> Arc<Slot> {
        let mut published = self.published.lock().unwrap();
        published.get_or_insert_with(make).clone()
    }

    fn record_write(&self, id: usize) {
        self.writes.lock().unwrap().push(id);
    }

    fn write_through(&self, slot: &Arc<Slot>, v: Option<StoreId>) {
        self.record_write(slot.id);
        *slot.value.write().unwrap() = v;
    }
}

// BUGGY get-then-set for clear
fn buggy_clear(cell: &OnceCell) {
    if let Some(slot) = cell.get() {
        cell.write_through(&slot, None);
    } else {
        let slot = cell.new_slot(None);
        cell.record_write(slot.id);
        let _ = cell.set(slot);
    }
}

// FIXED get_or_init for clear
fn fixed_clear(cell: &OnceCell) {
    let slot = cell.get_or_init(|| cell.new_slot(None));
    cell.write_through(&slot, None);
}

// BUGGY get-then-set for init
fn buggy_init(cell: &OnceCell, store: StoreId) {
    if let Some(slot) = cell.get() {
        cell.write_through(&slot, Some(store));
    } else {
        let slot = cell.new_slot(Some(store));
        cell.record_write(slot.id);
        let _ = cell.set(slot);
    }
}

// FIXED get_or_init for init
fn fixed_init(cell: &OnceCell, store: StoreId) {
    let slot = cell.get_or_init(|| cell.new_slot(None));
    cell.write_through(&slot, Some(store));
}

#[test]
fn global_tracking_store_race() {
    let show_bug = std::env::var_os("TRACKING_STORE_LOOM_SHOW_BUG").is_some();
    loom::model(move || {
        let cell = Arc::new(OnceCell::new());

        let c1 = cell.clone();
        let t1 = thread::spawn(move || {
            if show_bug {
                buggy_init(&c1, 1);
            } else {
                fixed_init(&c1, 1);
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
