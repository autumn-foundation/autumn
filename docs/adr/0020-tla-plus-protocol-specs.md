# ADR 0020: TLA+ Specs Next to the Stateright Models

- Status: Accepted
- Date: 2026-10-20
- Deciders: Autumn maintainers
- Tags: verification, model-checking, fencing, fleet

## Context

Issue #3071 added Stateright models of the job claim, the tick election and
the lease lock (`verification/models/`). Its plan rejected TLA+: "Item 2 needs
Java in CI and a second language"
(`docs/plans/2026-10-08-protocol-models-and-fault-injection.md`).

Two gaps stayed open after that:

1. **One encoding.** Each protocol had one model, in Rust. A modelling error
   there (a guard written wrong, an action that cannot fire) goes unseen,
   because nothing else states the protocol.
2. **The fleet had no interleaving check.** The slot-ownership epochs of
   #3233 have only Verus proofs, one step at a time
   (`verification/fleet_slot_ownership.rs`). The safety of a fleet rests on
   the order of events across hosts: clock skew, the local lease, and the
   replicator's check of the epoch markers, which is not atomic with its
   write. A one-step proof cannot see that order.

## Decision

1. **TLA+ specs in `verification/tla/`, checked with TLC.** One spec for each
   Stateright model (`JobClaim`, `TickElection`, `LeaseLock`), written from
   the SQL again rather than translated from the Rust, and one new spec,
   `FleetSlotOwnership`.
2. **The same discipline as the Stateright models.** Each spec has a
   `Variant` constant: `Correct` or a seeded bug. `checks.txt` lists every
   check. A correct variant must hold every invariant and reach every
   witness. Each seeded bug must give a counterexample. A run above 2,000,000
   states fails, as in the Stateright tests.
3. **Java only in its own workflow.** `.github/workflows/tla.yml` downloads
   `tla2tools.jar` at a pinned release with a pinned SHA-256 and runs
   `scripts/verify-tla.sh`. It is not a required check yet, like `verus.yml`
   and `kani.yml`. The workspace build does not need Java.
4. **Drift is caught without Java.** `verification/models/tests/tla_checks.rs`
   runs in `cargo test --workspace`. It fails when a Stateright seeded bug has
   no line in `checks.txt`, and when a spec has no correct or seeded check.

## Consequences

- A protocol change now touches up to three artefacts: the Rust, the
  Stateright model and the TLA+ spec. The drift test names the missing one.
- TLA+ is a second language in the repository. The specs stay small (under
  300 lines each) and each action names the statement or function it models.
- The fleet spec checks five properties over all interleavings: one serving
  host, a serving host's epoch is current, unique epochs, a restore takes the
  newest epoch, and the current owner's replicator is never fenced. Its
  witnesses show that the old replicator *does* write a generation after a
  newer marker landed, so the (epoch, time) restore order is load-bearing.
- Out of scope: liveness (TLC can check it, but the bounded clocks of these
  specs stop every behaviour), and the fence floor of `claim_above`, which
  only matters after a rollback of the control database. Such a rollback also
  drops live leases, and no epoch makes that safe.
