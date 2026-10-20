---------------------------- MODULE TickElection ----------------------------
(***************************************************************************)
(* Scheduler tick election with the durable tick record (#3052). The       *)
(* Stateright twin is verification/models/src/tick_election.rs; the two    *)
(* must agree.                                                             *)
(*                                                                         *)
(* Three replicas race for one tick of one task. Statements                *)
(* (PostgresTickSchedulerCoordinator, autumn/src/scheduler.rs):            *)
(*                                                                         *)
(*   prune  DELETE FROM autumn_scheduler_ticks WHERE expires_at < now()    *)
(*   claim  INSERT ... expires_at = now() + hold ON CONFLICT DO NOTHING    *)
(*          RETURNING generation, with hold = retention + period           *)
(*   free   DELETE ... WHERE tick_key = $3 AND generation = $4. A replica  *)
(*          frees an unrun claim when the cost gate rises, then claims     *)
(*          again.                                                         *)
(*                                                                         *)
(* A replica can wait (the cost gate, a slow claim) between the choice of  *)
(* the tick and the insert. execute_cron_task checks on its own clock that *)
(* the tick is not past its window, before and after the claim. The model  *)
(* checks at the insert. The check is closed (<=), which over-approximates *)
(* continuous time. Each replica clock has a skew of up to Retention; the  *)
(* database clock decides the row expiry. A free can be sent again after   *)
(* it applied (a retry after a lost reply). Release keeps the row, so it   *)
(* is not an action.                                                       *)
(***************************************************************************)
EXTENDS Integers, FiniteSets

CONSTANT Variant

Variants == {
    "Correct",
    \* The claim uses ON CONFLICT DO UPDATE: it takes an existing row.
    "OverwriteOnConflict",
    \* The row expires one step before the end of the window.
    "HoldShorterThanPeriod",
    \* The row holds for max(retention, period), not their sum.
    "HoldIsMax",
    \* A replica claims after a long wait with no window check (the bug the
    \* Stateright model found in execute_cron_task).
    "NoLatenessCheck",
    \* Free deletes the row of the tick with no generation check.
    "FreeWithoutGeneration"
}

ASSUME Variant \in Variants

Replicas == {0, 1, 2}
Period == 3                          \* the tick period: the window of the tick
Retention == 1                       \* scheduler.tick_retention; covers the skew
Skew == [r \in Replicas |-> r - 1]   \* -Retention .. Retention
MaxTime == 6

Max(a, b) == IF a >= b THEN a ELSE b

Hold ==
    CASE Variant = "HoldShorterThanPeriod" -> Period - 1
      [] Variant = "HoldIsMax"             -> Max(Retention, Period)
      [] OTHER                             -> Retention + Period

NoRow == [present |-> FALSE, owner |-> 0, generation |-> 0, expiresAt |-> 0]

VARIABLES
    now,             \* the database clock
    row,             \* the tick row
    nextGeneration,  \* the next generation value
    replicas,        \* one phase per replica
    freed,           \* the replicas that freed a claim; a replica frees once
    retries,         \* free retries in flight: their generations
    runs,            \* ghost: the number of runs of the tick
    reclaimed,       \* ghost: a claim succeeded after a free
    pruned           \* ghost: the prune deleted the row

vars == <<now, row, nextGeneration, replicas, freed, retries, runs, reclaimed, pruned>>

Phase(k, g) == [kind |-> k, generation |-> g]

TypeOK ==
    /\ now \in 0..MaxTime
    /\ row.present \in BOOLEAN
    /\ \A r \in Replicas : replicas[r].kind \in {"Idle", "Holding", "Skipped", "Ran"}
    /\ freed \subseteq Replicas
    /\ runs \in Nat

\* The window check of replica r, on its own clock.
InWindow(r) == Variant = "NoLatenessCheck" \/ now + Skew[r] <= Period

FreeRow(g) ==
    IF row.present /\ (Variant = "FreeWithoutGeneration" \/ row.generation = g)
    THEN NoRow
    ELSE row

Init ==
    /\ now = 0
    /\ row = NoRow
    /\ nextGeneration = 1
    /\ replicas = [r \in Replicas |-> Phase("Idle", 0)]
    /\ freed = {}
    /\ retries = {}
    /\ runs = 0
    /\ reclaimed = FALSE
    /\ pruned = FALSE

Prune ==
    /\ row.present
    /\ row.expiresAt < now
    /\ row' = NoRow
    /\ pruned' = TRUE
    /\ UNCHANGED <<now, nextGeneration, replicas, freed, retries, runs, reclaimed>>

Claim(r) ==
    /\ replicas[r].kind = "Idle"
    /\ InWindow(r)
    /\ nextGeneration' = nextGeneration + 1
    /\ IF ~row.present \/ Variant = "OverwriteOnConflict"
       THEN /\ row' = [present |-> TRUE, owner |-> r, generation |-> nextGeneration,
                       expiresAt |-> now + Hold]
            /\ replicas' = [replicas EXCEPT ![r] = Phase("Holding", nextGeneration)]
            /\ reclaimed' = (reclaimed \/ freed # {})
       ELSE /\ replicas' = [replicas EXCEPT ![r] = Phase("Skipped", 0)]
            /\ UNCHANGED <<row, reclaimed>>
    /\ UNCHANGED <<now, freed, retries, runs, pruned>>

Run(r) ==
    /\ replicas[r].kind = "Holding"
    /\ replicas' = [replicas EXCEPT ![r] = Phase("Ran", 0)]
    /\ runs' = runs + 1
    /\ UNCHANGED <<now, row, nextGeneration, freed, retries, reclaimed, pruned>>

Free(r) ==
    /\ replicas[r].kind = "Holding"
    /\ r \notin freed
    /\ row' = FreeRow(replicas[r].generation)
    /\ replicas' = [replicas EXCEPT ![r] = Phase("Idle", 0)]
    /\ freed' = freed \cup {r}
    /\ retries' = retries \cup {replicas[r].generation}
    /\ UNCHANGED <<now, nextGeneration, runs, reclaimed, pruned>>

RetryFree(g) ==
    /\ g \in retries
    /\ retries' = retries \ {g}
    /\ row' = FreeRow(g)
    /\ UNCHANGED <<now, nextGeneration, replicas, freed, runs, reclaimed, pruned>>

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<row, nextGeneration, replicas, freed, retries, runs, reclaimed, pruned>>

Next ==
    \/ Prune
    \/ \E r \in Replicas : Claim(r) \/ Run(r) \/ Free(r)
    \/ \E g \in retries : RetryFree(g)
    \/ Tick

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Safety.                                                                 *)
(***************************************************************************)

TickRunsAtMostOnce == runs <= 1

(***************************************************************************)
(* Non-vacuity: each must be reachable in the correct protocol.            *)
(***************************************************************************)

TickRuns == runs = 1
FreedTickIsReclaimed == reclaimed
RowIsPruned == pruned

=============================================================================
