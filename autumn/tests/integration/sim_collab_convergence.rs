//! Convergence of the collaborative text CRDT under randomized interleavings
//! (issue #1806).
//!
//! The claim under test is the one the issue's Success Metric names: any
//! interleaving of a fixed operation set, across any number of replicas,
//! yields a **byte-identical** final state with zero dropped operations.
//!
//! Three levels of evidence, strongest first:
//!
//! 1. `sim_collab_every_interleaving_of_a_fixed_op_set_converges` — an
//!    *exhaustive* sweep of all 720 orderings of a six-operation set.
//! 2. `sim_collab_randomized_replica_interleavings_converge` — a property
//!    test over generated edit scripts across 2–5 replicas.
//! 3. `sim_collab_replays_byte_identically_under_a_fixed_seed` — the same
//!    seed replays the same bytes, so a failure is reproducible.
//!
//! The module is named `sim_*` so CI runs it in the single-threaded lane
//! (see CLAUDE.md): these are determinism tests, and they are meaningless
//! under CPU oversubscription. It holds no wall-clock budget.

#![cfg(feature = "collab")]

use std::collections::HashSet;

use autumn_web::collab::{CollabOp, CollabText};
use proptest::prelude::*;

/// A replica's local edit, before it becomes operations.
#[derive(Debug, Clone)]
enum Edit {
    Insert { index: usize, text: String },
    Delete { index: usize, count: usize },
}

/// The serialized form — what "byte-identical" means here.
fn bytes(doc: &CollabText) -> String {
    serde_json::to_string(doc).expect("a document always encodes")
}

/// Deterministic shuffle: a 64-bit xorshift keyed by `seed`, so a failing
/// case is reproducible from the seed alone.
fn shuffled<T>(mut items: Vec<T>, seed: u64) -> Vec<T> {
    let mut state = seed | 1; // never zero, which xorshift cannot leave
    for i in (1..items.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bound = u64::try_from(i).expect("an index fits in u64") + 1;
        let pick = usize::try_from(state % bound).expect("a value below len fits in usize");
        items.swap(i, pick);
    }
    items
}

/// Run `edits` on `replica_count` replicas that all start from `base`, then
/// deliver every operation to every other replica in a shuffled order.
///
/// Returns the replicas, each holding the whole operation set.
fn run_simulation(
    base: &CollabText,
    replica_count: usize,
    edits: &[(usize, Edit)],
    seed: u64,
) -> Vec<CollabText> {
    let mut replicas: Vec<CollabText> = (0..replica_count).map(|_| base.clone()).collect();

    // Each replica edits locally and keeps the operations it produced.
    let mut produced: Vec<Vec<CollabOp>> = vec![Vec::new(); replica_count];
    for (replica, edit) in edits {
        let replica = *replica % replica_count;
        let actor = format!("replica-{replica}");
        let ops = match edit {
            Edit::Insert { index, text } => {
                let at = index % (replicas[replica].len() + 1);
                replicas[replica]
                    .insert(&actor, at, text)
                    .expect("collab edit refused")
            }
            Edit::Delete { index, count } => {
                let live = replicas[replica].len();
                if live == 0 {
                    Vec::new()
                } else {
                    replicas[replica].remove(index % live, (*count % live) + 1)
                }
            }
        };
        produced[replica].extend(ops);
    }

    // Everyone receives everyone else's operations, in their own order.
    for (target, replica) in replicas.iter_mut().enumerate() {
        let incoming: Vec<CollabOp> = (0..replica_count)
            .filter(|source| *source != target)
            .flat_map(|source| produced[source].clone())
            .collect();
        let stream = u64::try_from(target).expect("a replica index fits in u64");
        for op in shuffled(incoming, seed.wrapping_add(stream)) {
            replica.apply(op);
        }
    }
    replicas
}

/// The strongest form of the claim: **every** ordering of a fixed operation
/// set — all 720 of them — reaches the same bytes.
#[test]
fn sim_collab_every_interleaving_of_a_fixed_op_set_converges() {
    // Six operations from three replicas branching off one base.
    let base = CollabText::from_text("seed", "abc").expect("collab edit refused");
    let mut ops = Vec::new();
    for (actor, index, text) in [("x", 0, "1"), ("y", 3, "2"), ("z", 2, "3")] {
        let mut replica = base.clone();
        ops.extend(
            replica
                .insert(actor, index, text)
                .expect("collab edit refused"),
        );
    }
    let mut deleter = base.clone();
    ops.extend(deleter.remove(1, 1)); // tombstone 'b'
    let mut appender = base.clone();
    ops.extend(appender.insert("w", 3, "45").expect("collab edit refused")); // two more characters
    assert_eq!(ops.len(), 6, "the fixed op set is six operations");

    let orders = permutations_of(ops.len());
    assert_eq!(
        orders.iter().collect::<HashSet<_>>().len(),
        720,
        "the sweep must be exhaustive: 6! distinct orderings"
    );

    let mut seen = HashSet::new();
    let mut permutations = 0usize;
    for order in orders {
        let mut doc = base.clone();
        for i in order {
            doc.apply(ops[i].clone());
        }
        assert_eq!(doc.pending_len(), 0, "every operation integrated");
        seen.insert(bytes(&doc));
        permutations += 1;
    }
    assert_eq!(permutations, 720, "all 6! orderings were exercised");
    assert_eq!(
        seen.len(),
        1,
        "every interleaving must reach one byte-identical state, got {seen:?}"
    );
}

/// All orderings of `n` indices (Heap's algorithm).
fn permutations_of(n: usize) -> Vec<Vec<usize>> {
    fn generate(k: usize, items: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if k == 1 {
            out.push(items.clone());
            return;
        }
        for i in 0..k {
            generate(k - 1, items, out);
            if k.is_multiple_of(2) {
                items.swap(i, k - 1);
            } else {
                items.swap(0, k - 1);
            }
        }
    }
    let mut items: Vec<usize> = (0..n).collect();
    let mut out = Vec::new();
    generate(n, &mut items, &mut out);
    out
}

/// The same seed replays the same bytes: a failure here is reproducible from
/// the seed, which is what makes the property test above actionable.
#[test]
fn sim_collab_replays_byte_identically_under_a_fixed_seed() {
    let base = CollabText::from_text("seed", "hello").expect("collab edit refused");
    let edits = vec![
        (
            0,
            Edit::Insert {
                index: 0,
                text: "A".into(),
            },
        ),
        (
            1,
            Edit::Insert {
                index: 5,
                text: "B".into(),
            },
        ),
        (2, Edit::Delete { index: 1, count: 2 }),
        (
            0,
            Edit::Insert {
                index: 2,
                text: "CD".into(),
            },
        ),
    ];

    let first = run_simulation(&base, 3, &edits, 0x5eed);
    let second = run_simulation(&base, 3, &edits, 0x5eed);
    assert_eq!(
        first.iter().map(bytes).collect::<Vec<_>>(),
        second.iter().map(bytes).collect::<Vec<_>>(),
        "the same seed replays byte-identically"
    );
    // And the replicas of one run agree with each other.
    let distinct: HashSet<String> = first.iter().map(bytes).collect();
    assert_eq!(distinct.len(), 1, "replicas converge: {distinct:?}");
}

fn edit_strategy() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (0usize..12, "[a-z]{1,3}").prop_map(|(index, text)| Edit::Insert { index, text }),
        (0usize..12, 0usize..3).prop_map(|(index, count)| Edit::Delete { index, count }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The issue's Success Metric: across 2–5 replicas and any interleaving,
    /// every replica reaches the same bytes and no operation is dropped.
    #[test]
    fn sim_collab_randomized_replica_interleavings_converge(
        replica_count in 2usize..=5,
        edits in proptest::collection::vec((0usize..5, edit_strategy()), 1..14),
        seed in any::<u64>(),
    ) {
        let base = CollabText::from_text("seed", "start").expect("collab edit refused");
        let replicas = run_simulation(&base, replica_count, &edits, seed);

        let distinct: HashSet<String> = replicas.iter().map(bytes).collect();
        prop_assert_eq!(
            distinct.len(),
            1,
            "replicas diverged under seed {}: {:?}",
            seed,
            replicas.iter().map(CollabText::text).collect::<Vec<_>>()
        );
        for replica in &replicas {
            prop_assert_eq!(replica.pending_len(), 0, "no operation was left waiting");
        }
    }

    /// No character is dropped: the surviving text holds every inserted
    /// character that was never deleted, in one consistent order.
    #[test]
    fn sim_collab_no_operation_is_dropped(
        edits in proptest::collection::vec((0usize..3, edit_strategy()), 1..10),
        seed in any::<u64>(),
    ) {
        let base = CollabText::from_text("seed", "abc").expect("collab edit refused");
        let replicas = run_simulation(&base, 3, &edits, seed);

        // Every operation any replica produced is present in every replica.
        let all_ops: HashSet<String> = replicas
            .iter()
            .flat_map(autumn_web::collab::CollabText::ops)
            .map(|op| serde_json::to_string(&op).expect("op encodes"))
            .collect();
        for replica in &replicas {
            let held: HashSet<String> = replica
                .ops()
                .into_iter()
                .map(|op| serde_json::to_string(&op).expect("op encodes"))
                .collect();
            prop_assert_eq!(held, all_ops.clone(), "a replica dropped an operation");
        }
    }
}

/// The stored form is part of convergence: a document carrying operations
/// that can never integrate must still round-trip, and a scrambled stored
/// order must replay back to the canonical one.
///
/// The sweeps above only ever encode fully-integrated documents, so neither
/// path was exercised.
#[test]
fn sim_collab_the_stored_form_round_trips_and_re_canonicalizes() {
    let mut doc = CollabText::from_text("seed", "abc").expect("collab edit refused");
    // An operation whose cause will never arrive: it must survive the trip.
    doc.apply(CollabOp::Insert {
        id: autumn_web::collab::OpId::new(900, "ghost"),
        after: Some(autumn_web::collab::OpId::new(800, "ghost")),
        ch: '?',
    });
    assert_eq!(doc.pending_len(), 1, "the orphan is buffered, not dropped");

    let encoded = serde_json::to_string(&doc).expect("encode");
    let decoded: CollabText = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, doc, "the buffer survives the stored form");
    assert_eq!(
        serde_json::to_string(&decoded).expect("re-encode"),
        encoded,
        "encoding is a fixed point"
    );

    // A stored order no replica would produce replays to the canonical one.
    let scrambled = {
        let mut value: serde_json::Value = serde_json::from_str(&encoded).expect("parse");
        let elems = value["elems"].as_array_mut().expect("elems");
        elems.reverse();
        serde_json::to_string(&value).expect("re-encode")
    };
    let repaired: CollabText = serde_json::from_str(&scrambled).expect("decode");
    assert_eq!(
        repaired.text(),
        doc.text(),
        "a scrambled stored order replays back to the canonical one"
    );
}

/// A causal chain deeper than one level. The exhaustive sweep branches every
/// operation off one base, so nothing there tests an anchor that is itself
/// concurrent with another insert.
#[test]
fn sim_collab_a_deep_causal_chain_converges_in_any_order() {
    let base = CollabText::from_text("seed", "ab").expect("collab edit refused");

    // Replica x builds a three-character chain; replica y branches off the
    // middle of it, which it can only do after receiving part of the chain.
    let mut x = base.clone();
    let chain = x.insert("x", 1, "123").expect("collab edit refused");
    let mut y = base.clone();
    y.apply_all(chain.clone());
    let branch = y.insert("y", 2, "-").expect("collab edit refused");
    let mut z = base.clone();
    let tail = z.insert("z", 2, "!").expect("collab edit refused");

    let mut ops = chain;
    ops.extend(branch);
    ops.extend(tail);

    let mut seen = HashSet::new();
    for order in permutations_of(ops.len()) {
        let mut doc = base.clone();
        for i in order {
            doc.apply(ops[i].clone());
        }
        assert_eq!(doc.pending_len(), 0, "every operation integrated");
        seen.insert(bytes(&doc));
    }
    assert_eq!(
        seen.len(),
        1,
        "a deep chain converges in every order too: {seen:?}"
    );
}
