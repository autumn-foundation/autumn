------------------------- MODULE FleetSlotOwnership -------------------------
(***************************************************************************)
(* Slot-ownership epochs, local leases and replicator fencing of a SQLite  *)
(* fleet (#3233, ADR 0019 section 7). verification/fleet_slot_ownership.rs *)
(* proves each table decision one step at a time; this model checks every *)
(* interleaving of two hosts, their clocks and their replicators.          *)
(*                                                                         *)
(* One slot range, two hosts. Each decision is one transaction on the      *)
(* ownership table (OwnershipTable in autumn/src/db/fleet_ownership.rs):   *)
(*                                                                         *)
(*   claim    refused while another owner's lease is live on the claimer's *)
(*            wall clock. The owner's own row keeps its epoch; otherwise   *)
(*            the epoch is counter + 1.                                    *)
(*   renew    same owner and epoch, else the range is lost. A lapsed lease *)
(*            that nobody claimed can be renewed.                          *)
(*   release  the lease ends now; the epoch stays.                         *)
(*                                                                         *)
(* The lease expiry is the claiming host's wall time. A host serves until  *)
(* it sent its last renew plus ttl - ttl/3, on its monotonic clock (the    *)
(* global clock here). Host 2's wall clock runs one step ahead of host 1's.*)
(* ADR 0019 requires skew plus the longest request below ttl/3.            *)
(*                                                                         *)
(* A replicator ships under its database's epoch (Replicator in            *)
(* autumn/src/replication/engine.rs). A new generation lists the epoch     *)
(* markers (check_fence), then writes its own marker and the generation.   *)
(* The two steps are not atomic, and the periodic check runs every 60 s,   *)
(* so an old replicator can still write a generation after a newer marker *)
(* landed. restore::plan orders generations by (epoch, created) so that    *)
(* late generation never wins.                                             *)
(*                                                                         *)
(* Out of scope: the fence floor of claim_above. It only matters after the *)
(* control database is restored to an earlier time, and such a rollback   *)
(* also drops live leases, which no epoch can make safe.                   *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANT Variant

Variants == {
    "Correct",
    \* The local lease lasts the full ttl, so skew lets two hosts serve.
    "TrustFullTtl",
    \* A claim takes a live lease of another owner.
    "ClaimIgnoresLiveLease",
    \* A takeover keeps the old owner's epoch instead of a new one.
    "TakeoverKeepsEpoch",
    \* restore::plan picks the newest generation by time alone.
    "RestoreByTime",
    \* check_fence stops on any other epoch (`!=`), not only a newer one.
    "FenceOnAnyOtherEpoch"
}

ASSUME Variant \in Variants

Hosts == {1, 2}
NoHost == 0
Skew == [h \in Hosts |-> h - 1]   \* host 2's wall clock is one step ahead
Ttl == 6
Trust == 4                        \* ttl - ttl / 3
MaxClaims == 3
MaxGenerations == 2
MaxTime == 7

Max(a, b) == IF a >= b THEN a ELSE b
MaxOf(S) == IF S = {} THEN 0 ELSE CHOOSE x \in S : \A y \in S : y <= x

LocalTrust == IF Variant = "TrustFullTtl" THEN Ttl ELSE Trust

NotHeld == [epoch |-> 0, validUntil |-> 0]
NoReplicator == [epoch |-> 0, phase |-> "none", fenced |-> FALSE]

VARIABLES
    now,          \* the global (monotonic) clock
    row,          \* the ownership row of the range; owner NoHost: no row
    last,         \* the epoch counter
    held,         \* each host's epoch and local lease end; epoch 0: none
    repl,         \* each host's replicator for the range's database
    markers,      \* the epoch markers at the destination (<root>/epochs/)
    generations,  \* the generations at the destination
    claims,       \* bound: the number of claims
    granted,      \* ghost: the epochs the table has granted
    epochReused,  \* ghost: a fresh grant reused an earlier epoch
    tookOver,     \* ghost: a claim took the range from another owner
    lateGen       \* ghost: a generation landed under an epoch older than a marker

vars == <<now, row, last, held, repl, markers, generations, claims, granted,
          epochReused, tookOver, lateGen>>

TypeOK ==
    /\ now \in 0..MaxTime
    /\ row.owner \in Hosts \cup {NoHost}
    /\ \A h \in Hosts : repl[h].phase \in {"none", "idle", "checked", "shipping"}
    /\ \A g \in generations : g.host \in Hosts
    /\ claims \in 0..MaxClaims

\* The wall clock of host h.
Wall(h) == now + Skew[h]

\* The local lease: the host may open and serve the range.
Serving(h) == held[h].epoch # 0 /\ now < held[h].validUntil

\* OwnershipTable::admits: a write by h under its epoch is current.
Admits(h) == row.owner = h /\ row.epoch = held[h].epoch

\* claim_above: a live lease of another owner refuses the claim.
Refused(h) ==
    /\ Variant # "ClaimIgnoresLiveLease"
    /\ row.owner # NoHost
    /\ row.owner # h
    /\ row.expiresAt > Wall(h)

\* check_fence: the newest marker stops a replicator of an older epoch.
FencedBy(e) ==
    IF Variant = "FenceOnAnyOtherEpoch"
    THEN \E m \in markers : m # e
    ELSE MaxOf(markers) > e

\* restore::plan: the greatest (epoch, created).
Newer(a, b) ==
    IF Variant = "RestoreByTime"
    THEN a.created > b.created
    ELSE \/ a.epoch > b.epoch
         \/ a.epoch = b.epoch /\ a.created > b.created

RestorePicks == {g \in generations : \A o \in generations : ~Newer(o, g)}

Init ==
    /\ now = 0
    /\ row = [owner |-> NoHost, epoch |-> 0, expiresAt |-> 0]
    /\ last = 0
    /\ held = [h \in Hosts |-> NotHeld]
    /\ repl = [h \in Hosts |-> NoReplicator]
    /\ markers = {}
    /\ generations = {}
    /\ claims = 0
    /\ granted = {}
    /\ epochReused = FALSE
    /\ tookOver = FALSE
    /\ lateGen = FALSE

(***************************************************************************)
(* The ownership table.                                                    *)
(***************************************************************************)

Claim(h) ==
    /\ held[h].epoch = 0
    /\ claims < MaxClaims
    /\ ~Refused(h)
    /\ claims' = claims + 1
    /\ IF row.owner = h
       \* The own row with the same bounds keeps its epoch (a restart).
       THEN /\ row' = [row EXCEPT !.expiresAt = Wall(h) + Ttl]
            /\ held' = [held EXCEPT ![h] = [epoch |-> row.epoch,
                                            validUntil |-> now + LocalTrust]]
            /\ UNCHANGED <<last, granted, epochReused, tookOver>>
       ELSE LET next == IF Variant = "TakeoverKeepsEpoch" /\ row.owner # NoHost
                        THEN row.epoch
                        ELSE last + 1
            IN /\ row' = [owner |-> h, epoch |-> next, expiresAt |-> Wall(h) + Ttl]
               /\ last' = Max(last, next)
               /\ held' = [held EXCEPT ![h] = [epoch |-> next,
                                               validUntil |-> now + LocalTrust]]
               /\ epochReused' = (epochReused \/ next \in granted)
               /\ granted' = granted \cup {next}
               /\ tookOver' = (tookOver \/ row.owner # NoHost)
    /\ UNCHANGED <<now, repl, markers, generations, lateGen>>

Renew(h) ==
    /\ held[h].epoch # 0
    /\ IF Admits(h)
       THEN /\ row' = [row EXCEPT !.expiresAt = Wall(h) + Ttl]
            /\ held' = [held EXCEPT ![h].validUntil = now + LocalTrust]
       \* Lost: another host claimed the range. Close it.
       ELSE /\ held' = [held EXCEPT ![h] = NotHeld]
            /\ UNCHANGED row
    /\ UNCHANGED <<now, last, repl, markers, generations, claims, granted, epochReused,
                   tookOver, lateGen>>

Release(h) ==
    /\ held[h].epoch # 0
    /\ row' = IF Admits(h)
              THEN [row EXCEPT !.expiresAt = IF @ < Wall(h) THEN @ ELSE Wall(h)]
              ELSE row
    /\ held' = [held EXCEPT ![h] = NotHeld]
    /\ UNCHANGED <<now, last, repl, markers, generations, claims, granted, epochReused,
                   tookOver, lateGen>>

(***************************************************************************)
(* The replicator.                                                         *)
(***************************************************************************)

\* A fenced replicator makes its host lose the range (OwnershipManager::lose).
Fence(h) ==
    /\ repl' = [repl EXCEPT ![h].fenced = TRUE, ![h].phase = "none"]
    /\ held' = IF held[h].epoch # 0 /\ held[h].epoch < MaxOf(markers)
               THEN [held EXCEPT ![h] = NotHeld]
               ELSE held

\* The database opens under the host's epoch; set_epoch resets the replicator.
Open(h) ==
    /\ Serving(h)
    /\ repl[h].epoch # held[h].epoch
    /\ repl' = [repl EXCEPT ![h] = [epoch |-> held[h].epoch, phase |-> "idle",
                                    fenced |-> FALSE]]
    /\ UNCHANGED <<now, row, last, held, markers, generations, claims, granted,
                   epochReused, tookOver, lateGen>>

\* start_generation, part one: list the markers.
CheckFence(h) ==
    /\ repl[h].phase = "idle"
    /\ Cardinality(generations) < MaxGenerations
    /\ IF FencedBy(repl[h].epoch)
       THEN Fence(h)
       ELSE /\ repl' = [repl EXCEPT ![h].phase = "checked"]
            /\ UNCHANGED held
    /\ UNCHANGED <<now, row, last, markers, generations, claims, granted, epochReused,
                   tookOver, lateGen>>

\* start_generation, part two: write the marker and the generation.
CreateGeneration(h) ==
    /\ repl[h].phase = "checked"
    /\ markers' = markers \cup {repl[h].epoch}
    /\ generations' = generations \cup
                      {[epoch |-> repl[h].epoch, created |-> now, host |-> h]}
    /\ lateGen' = (lateGen \/ MaxOf(markers) > repl[h].epoch)
    /\ repl' = [repl EXCEPT ![h].phase = "shipping"]
    /\ UNCHANGED <<now, row, last, held, claims, granted, epochReused, tookOver>>

\* The check every 60 s while shipping.
PeriodicFence(h) ==
    /\ repl[h].phase = "shipping"
    /\ FencedBy(repl[h].epoch)
    /\ Fence(h)
    /\ UNCHANGED <<now, row, last, markers, generations, claims, granted, epochReused,
                   tookOver, lateGen>>

\* The replicator starts a new generation (the snapshot interval).
Rotate(h) ==
    /\ repl[h].phase = "shipping"
    /\ repl' = [repl EXCEPT ![h].phase = "idle"]
    /\ UNCHANGED <<now, row, last, held, markers, generations, claims, granted,
                   epochReused, tookOver, lateGen>>

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<row, last, held, repl, markers, generations, claims, granted,
                   epochReused, tookOver, lateGen>>

Next ==
    \/ \E h \in Hosts :
          \/ Claim(h) \/ Renew(h) \/ Release(h)
          \/ Open(h) \/ CheckFence(h) \/ CreateGeneration(h)
          \/ PeriodicFence(h) \/ Rotate(h)
    \/ Tick

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Safety.                                                                 *)
(***************************************************************************)

\* Within the clock bound, at most one host serves the range.
ServingExclusive == Cardinality({h \in Hosts : Serving(h)}) <= 1

\* A host that serves the range holds the current row and epoch.
ServingHostIsAdmitted == \A h \in Hosts : Serving(h) => Admits(h)

\* Each grant to a new owner gets an epoch that no earlier grant had.
EpochsUnique == ~epochReused

\* A restore takes a generation of the newest epoch at the destination.
RestoreTakesNewestEpoch ==
    \A p \in RestorePicks : p.epoch = MaxOf({g.epoch : g \in generations})

\* The replicator of the serving owner is never fenced.
CurrentOwnerNeverFenced ==
    \A h \in Hosts : (Serving(h) /\ repl[h].epoch = held[h].epoch) => ~repl[h].fenced

(***************************************************************************)
(* Non-vacuity: each must be reachable in the correct protocol.            *)
(***************************************************************************)

LeaseIsTakenOver == tookOver
ReplicatorIsFenced == \E h \in Hosts : repl[h].fenced
\* The check/write race: an old replicator writes after a newer marker.
StaleGenerationLandsLate == lateGen
\* The restore skips a generation that is newer by time.
RestoreOrderMatters == \E g \in generations, p \in RestorePicks : g.created > p.created

=============================================================================
