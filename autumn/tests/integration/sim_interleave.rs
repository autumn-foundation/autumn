//! Sim Phase 2 (issue #2967): the interleaving shuffler.
//!
//! `Sim::interleave` and `Sim::spawn` reorder ready work from the seed. The
//! same seed replays the same interleaving. Different seeds explore different
//! ones.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use autumn_web::sim::Sim;
use autumn_web::sim_test;

type Trace = Arc<Mutex<Vec<(usize, u32)>>>;

/// One op: three steps, with a yield between each.
async fn stepper(id: usize, trace: Trace) -> usize {
    for step in 0..3 {
        trace.lock().unwrap().push((id, step));
        tokio::task::yield_now().await;
    }
    id
}

async fn interleave_trace(sim: &Sim) -> Vec<(usize, u32)> {
    let trace = Trace::default();
    let ops = (0..4).map(|id| stepper(id, Arc::clone(&trace))).collect();
    let outputs = sim.interleave(ops).await;
    assert_eq!(outputs, vec![0, 1, 2, 3], "outputs keep the input order");
    trace.lock().unwrap().clone()
}

async fn spawn_trace(sim: &Sim) -> Vec<(usize, u32)> {
    let trace = Trace::default();
    let handles: Vec<_> = (0..4)
        .map(|id| sim.spawn(stepper(id, Arc::clone(&trace))))
        .collect();
    for (id, handle) in handles.into_iter().enumerate() {
        assert_eq!(handle.await.unwrap(), id);
    }
    trace.lock().unwrap().clone()
}

#[sim_test]
async fn sim_interleave_same_seed_replays(sim: Sim) {
    let first = interleave_trace(&sim).await;
    let again = interleave_trace(&Sim::from_seed(sim.seed)).await;
    assert_eq!(first, again, "the same seed replays the same interleaving");
    assert_eq!(first.len(), 12, "every step of every op ran");
}

#[sim_test]
async fn sim_interleave_seeds_explore_orders(_sim: Sim) {
    let mut seen = BTreeSet::new();
    for seed in 0..16 {
        seen.insert(interleave_trace(&Sim::from_seed(seed)).await);
    }
    assert!(seen.len() > 1, "16 seeds gave one interleaving only");
}

#[sim_test]
async fn sim_interleave_spawn_same_seed_replays(sim: Sim) {
    let first = spawn_trace(&sim).await;
    let again = spawn_trace(&Sim::from_seed(sim.seed)).await;
    assert_eq!(first, again, "the same seed replays the same task order");
    assert_eq!(first.len(), 12);
}

#[sim_test]
async fn sim_interleave_spawn_seeds_explore_orders(_sim: Sim) {
    let mut seen = BTreeSet::new();
    for seed in 0..16 {
        seen.insert(spawn_trace(&Sim::from_seed(seed)).await);
    }
    assert!(seen.len() > 1, "16 seeds gave one task order only");
}
