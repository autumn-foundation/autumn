------------------------------ MODULE LeaseLock -----------------------------
(***************************************************************************)
(* LeaseLock acquire, renew, release and expiry with fencing (#3053,       *)
(* docs/adr/0015-fencing-lease-lock.md). The Stateright twin is            *)
(* verification/models/src/lease_lock.rs; the two must agree.              *)
(*                                                                         *)
(* Two holders race for one lock name. A resource stores the last admitted *)
(* token. Statements (autumn/src/lock/lease.rs):                           *)
(*                                                                         *)
(*   acquire  upsert that sets generation = generation + 1 only WHERE      *)
(*            owner IS NULL OR expires_at <= now()                         *)
(*   renew    WHERE generation = $2 AND owner IS NOT NULL AND              *)
(*            expires_at > now()                                           *)
(*   release  SET owner = NULL WHERE generation = $2 AND owner IS NOT NULL *)
(*   write    WHERE fencing_token <= $token (FencingToken::admits)         *)
(*                                                                         *)
(* A holder trusts its lease until ttl - ttl / 3 after the last send. It   *)
(* can write at any time, also after its lease expires (for example, in a  *)
(* paused process). That stale write is what the fence must reject.        *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANT Variant

Variants == {
    "Correct",
    \* The resource admits every write.
    "ResourceIgnoresToken",
    \* Acquire does not increment generation.
    "AcquireKeepsGeneration",
    \* Acquire takes a live lease. Fencing must still reject stale writes.
    "AcquireWhileHeld",
    \* Release deletes the row, so the next grant starts at token 1 again.
    "ReleaseDeletesRow",
    \* Renew does not check the generation.
    "RenewIgnoresGeneration"
}

ASSUME Variant \in Variants

Holders == {0, 1}
NoOwner == 99
Ttl == 3
Trust == 2        \* ttl - ttl / 3
MaxGrants == 3
MaxTime == 6

Max(a, b) == IF a >= b THEN a ELSE b

NotHeld == [has |-> FALSE, token |-> 0, grant |-> 0, trustedUntil |-> 0]
NotStored == [has |-> FALSE, token |-> 0, grant |-> 0]

VARIABLES
    now,            \* the database clock
    row,            \* the autumn_lease_locks row; generation 0 means no row
    holders,        \* the token each holder kept from a grant
    stored,         \* the resource's stored token, and the grant (ghost) that wrote it
    grants,         \* ghost: the number of grants
    maxToken,       \* ghost: the largest token given so far
    staleAdmitted,  \* ghost: the resource admitted a stale write
    tokenReused,    \* ghost: an acquire gave a token that is not new
    rejected,       \* ghost: the resource rejected a write
    tookOver        \* ghost: an acquire took over another holder's lease

vars == <<now, row, holders, stored, grants, maxToken, staleAdmitted, tokenReused,
          rejected, tookOver>>

TypeOK ==
    /\ now \in 0..MaxTime
    /\ row.owner \in Holders \cup {NoOwner}
    /\ grants \in 0..MaxGrants
    /\ \A h \in Holders : holders[h].has \in BOOLEAN

CanAcquire ==
    \/ Variant = "AcquireWhileHeld"
    \/ row.owner = NoOwner
    \/ row.expiresAt <= now

Init ==
    /\ now = 0
    /\ row = [owner |-> NoOwner, generation |-> 0, expiresAt |-> 0]
    /\ holders = [h \in Holders |-> NotHeld]
    /\ stored = NotStored
    /\ grants = 0
    /\ maxToken = 0
    /\ staleAdmitted = FALSE
    /\ tokenReused = FALSE
    /\ rejected = FALSE
    /\ tookOver = FALSE

Acquire(h) ==
    /\ grants < MaxGrants
    /\ CanAcquire
    /\ LET gen == IF Variant # "AcquireKeepsGeneration" \/ row.generation = 0
                  THEN row.generation + 1
                  ELSE row.generation
       IN /\ row' = [owner |-> h, generation |-> gen, expiresAt |-> now + Ttl]
          /\ tokenReused' = (tokenReused \/ gen <= maxToken)
          /\ maxToken' = Max(maxToken, gen)
          /\ holders' = [holders EXCEPT ![h] =
                           [has |-> TRUE, token |-> gen, grant |-> grants + 1,
                            trustedUntil |-> now + Trust]]
    /\ tookOver' = (tookOver \/ (row.owner # NoOwner /\ row.owner # h))
    /\ grants' = grants + 1
    /\ UNCHANGED <<now, stored, staleAdmitted, rejected>>

Renew(h) ==
    /\ holders[h].has
    /\ IF /\ \/ row.generation = holders[h].token
             \/ Variant = "RenewIgnoresGeneration"
          /\ row.owner # NoOwner
          /\ row.expiresAt > now
       THEN /\ row' = [row EXCEPT !.expiresAt = now + Ttl]
            /\ holders' = [holders EXCEPT ![h].trustedUntil = now + Trust]
       \* A renewal that matches no row marks the lease lost.
       ELSE /\ holders' = [holders EXCEPT ![h].trustedUntil = 0]
            /\ UNCHANGED row
    /\ UNCHANGED <<now, stored, grants, maxToken, staleAdmitted, tokenReused, rejected,
                   tookOver>>

Release(h) ==
    /\ holders[h].has
    /\ holders' = [holders EXCEPT ![h] = NotHeld]
    /\ row' = IF row.generation = holders[h].token /\ row.owner # NoOwner
              THEN [row EXCEPT !.owner = NoOwner,
                               !.generation = IF Variant = "ReleaseDeletesRow" THEN 0 ELSE @]
              ELSE row
    /\ UNCHANGED <<now, stored, grants, maxToken, staleAdmitted, tokenReused, rejected,
                   tookOver>>

Write(h) ==
    /\ holders[h].has
    /\ LET held == holders[h]
           admits == \/ Variant = "ResourceIgnoresToken"
                     \/ ~stored.has
                     \/ stored.token <= held.token
       IN IF admits
          \* Stale: an older grant than the one that wrote last.
          THEN /\ staleAdmitted' = (staleAdmitted \/ (stored.has /\ held.grant < stored.grant))
               /\ stored' = [has |-> TRUE, token |-> held.token, grant |-> held.grant]
               /\ UNCHANGED rejected
          ELSE /\ rejected' = TRUE
               /\ UNCHANGED <<staleAdmitted, stored>>
    \* A holder writes once per grant, which bounds the model.
    /\ holders' = [holders EXCEPT ![h] = NotHeld]
    /\ UNCHANGED <<now, row, grants, maxToken, tokenReused, tookOver>>

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<row, holders, stored, grants, maxToken, staleAdmitted, tokenReused,
                   rejected, tookOver>>

Next ==
    \/ \E h \in Holders : Acquire(h) \/ Renew(h) \/ Release(h) \/ Write(h)
    \/ Tick

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Safety.                                                                 *)
(***************************************************************************)

\* The resource rejects a write with a token older than one it admitted.
StaleWriteRejected == ~staleAdmitted

\* Each grant gets a new token, larger than all earlier tokens.
TokensAreUnique == ~tokenReused

\* At most one holder trusts a live lease.
LiveHoldersExclusive ==
    Cardinality({h \in Holders : holders[h].has /\ now < holders[h].trustedUntil}) <= 1

(***************************************************************************)
(* Non-vacuity: each must be reachable in the correct protocol.            *)
(***************************************************************************)

StaleWriteIsTried == rejected
ExpiredLeaseIsTaken == tookOver

=============================================================================
