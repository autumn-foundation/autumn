------------------------------ MODULE JobClaim ------------------------------
(***************************************************************************)
(* Durable job claim, heartbeat, recovery and settle (#3051,               *)
(* docs/adr/0016-durable-job-claim-lease.md). The Stateright twin is       *)
(* verification/models/src/job_claim.rs; the two must agree.               *)
(*                                                                         *)
(* Two workers race for one job row. The fence is `claimed_by = $me AND    *)
(* status = 'running'`; the job has no generation column. Statements      *)
(* (autumn/src/job.rs):                                                    *)
(*                                                                         *)
(*   claim    pg_claim_sql_single_queue                                    *)
(*   renew    pg_renew_claim: SET claimed_at = NOW() WHERE id = $1 AND     *)
(*            claimed_by = $2 AND status = 'running'                       *)
(*   settle   pg_ack_success, pg_nack_failure: the same guard              *)
(*   recover  pg_recover_stale_claims: status = 'running' AND              *)
(*            claimed_at < NOW() - visibility                              *)
(*                                                                         *)
(* A worker renews every Heartbeat and gives up GiveUp after its last      *)
(* successful renewal (LeaseHeartbeat). A renewal can be late, can fail,   *)
(* or can apply with a lost reply. A settle can be late. The clock does    *)
(* not move past a due give-up, because the give-up timer fires on time.   *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANT Variant

Variants == {
    "Correct",
    \* Settle checks only status = 'running', not claimed_by.
    "SettleWithoutOwnerFence",
    \* The worker never gives up after failed renewals.
    "NoGiveUp",
    \* The sweep recovers a running claim before its deadline.
    "RecoverLiveClaim"
}

ASSUME Variant \in Variants

Workers == {0, 1}
NoWorker == 99
MaxAttempts == 2
Visibility == 3         \* the visibility timeout
Heartbeat == 1          \* visibility / 3
GiveUp == 2             \* 2 * visibility / 3
MaxTime == 8

Statuses == {"Enqueued", "Running", "Completed", "Failed"}

VARIABLES
    now,          \* the database clock
    row,          \* the job row
    workers,      \* one phase per worker
    net,          \* messages in flight
    staleSettle,  \* ghost: a settle applied for a worker without the attempt
    recovered,    \* ghost: the sweep recovered a claim
    claimers,     \* ghost: the workers that claimed the job
    lateSettle    \* ghost: a settle arrived while another worker held the row

vars == <<now, row, workers, net, staleSettle, recovered, claimers, lateSettle>>

Idle == [kind |-> "Idle", attempt |-> 0, renewed |-> 0, renewing |-> FALSE]
Running(a, r) == [kind |-> "Running", attempt |-> a, renewed |-> r, renewing |-> FALSE]
Settling(a) == [kind |-> "Settling", attempt |-> a, renewed |-> 0, renewing |-> FALSE]

\* One record shape for every message. `outcome` is "-" on a request.
Msg(t, w, a, s, o) == [type |-> t, worker |-> w, attempt |-> a, sent |-> s, outcome |-> o]

TypeOK ==
    /\ now \in 0..MaxTime
    /\ row.status \in Statuses
    /\ row.attempt \in 1..MaxAttempts
    /\ row.claimedBy \in Workers \cup {NoWorker}
    /\ row.claimedAt \in 0..MaxTime
    /\ \A w \in Workers : workers[w].kind \in {"Idle", "Running", "Settling"}
    /\ \A m \in net : m.type \in {"Renew", "RenewReply", "Settle"}
    /\ claimers \subseteq Workers

\* The guard of renew and settle: claimed_by = $me AND status = 'running'.
Holds(w) == row.status = "Running" /\ row.claimedBy = w

SettleGuard(w) ==
    IF Variant = "SettleWithoutOwnerFence"
    THEN row.status = "Running"
    ELSE Holds(w)

GiveUpDue(w) ==
    /\ Variant # "NoGiveUp"
    /\ workers[w].kind = "Running"
    /\ now - workers[w].renewed >= GiveUp

Recoverable ==
    /\ row.status = "Running"
    /\ \/ Variant = "RecoverLiveClaim"
       \/ now - row.claimedAt > Visibility

Init ==
    /\ now = 0
    /\ row = [status |-> "Enqueued", attempt |-> 1, claimedBy |-> NoWorker, claimedAt |-> 0]
    /\ workers = [w \in Workers |-> Idle]
    /\ net = {}
    /\ staleSettle = FALSE
    /\ recovered = FALSE
    /\ claimers = {}
    /\ lateSettle = FALSE

Claim(w) ==
    /\ workers[w].kind = "Idle"
    /\ row.status = "Enqueued"
    /\ row' = [row EXCEPT !.status = "Running", !.claimedBy = w, !.claimedAt = now]
    /\ workers' = [workers EXCEPT ![w] = Running(row.attempt, now)]
    /\ claimers' = claimers \cup {w}
    /\ UNCHANGED <<now, net, staleSettle, recovered, lateSettle>>

SendRenew(w) ==
    /\ workers[w].kind = "Running"
    /\ ~workers[w].renewing
    /\ now - workers[w].renewed >= Heartbeat
    /\ workers' = [workers EXCEPT ![w].renewing = TRUE]
    /\ net' = net \cup {Msg("Renew", w, workers[w].attempt, now, "-")}
    /\ UNCHANGED <<now, row, staleSettle, recovered, claimers, lateSettle>>

\* The database applies a renewal and replies, applies it and loses the
\* reply ("error"), or fails before it applies ("error").
ApplyRenew(m, mode) ==
    /\ m \in net
    /\ m.type = "Renew"
    /\ LET applied == mode # "fail" /\ Holds(m.worker)
           outcome == IF mode # "reply" THEN "error"
                      ELSE IF applied THEN "ok" ELSE "lost"
       IN /\ row' = IF applied THEN [row EXCEPT !.claimedAt = now] ELSE row
          /\ net' = (net \ {m}) \cup
                    {Msg("RenewReply", m.worker, m.attempt, m.sent, outcome)}
    /\ UNCHANGED <<now, workers, staleSettle, recovered, claimers, lateSettle>>

ReadRenewReply(m) ==
    /\ m \in net
    /\ m.type = "RenewReply"
    /\ net' = net \ {m}
    /\ LET p == workers[m.worker] IN
       workers' =
         IF p.kind = "Running" /\ p.attempt = m.attempt
         THEN [workers EXCEPT ![m.worker] =
                 CASE m.outcome = "ok"   -> [p EXCEPT !.renewing = FALSE, !.renewed = m.sent]
                   \* The lease is lost: drop the handler, do not settle.
                   [] m.outcome = "lost" -> Idle
                   [] OTHER              -> [p EXCEPT !.renewing = FALSE]]
         ELSE workers
    /\ UNCHANGED <<now, row, staleSettle, recovered, claimers, lateSettle>>

GiveUpRun(w) ==
    /\ GiveUpDue(w)
    /\ workers' = [workers EXCEPT ![w] = Idle]
    /\ UNCHANGED <<now, row, net, staleSettle, recovered, claimers, lateSettle>>

\* The handler completes; the worker sends the settle.
Finish(w) ==
    /\ workers[w].kind = "Running"
    /\ workers' = [workers EXCEPT ![w] = Settling(workers[w].attempt)]
    /\ net' = net \cup {Msg("Settle", w, workers[w].attempt, 0, "-")}
    /\ UNCHANGED <<now, row, staleSettle, recovered, claimers, lateSettle>>

ApplySettle(m) ==
    /\ m \in net
    /\ m.type = "Settle"
    /\ net' = net \ {m}
    \* The fence matters: another worker holds the running row.
    /\ lateSettle' = (lateSettle \/ (row.status = "Running" /\ row.claimedBy # m.worker))
    /\ IF SettleGuard(m.worker)
       THEN /\ staleSettle' = (staleSettle \/ ~Holds(m.worker) \/ row.attempt # m.attempt)
            /\ row' = [row EXCEPT !.status = "Completed", !.claimedBy = NoWorker]
       ELSE UNCHANGED <<staleSettle, row>>
    /\ workers' = IF workers[m.worker] = Settling(m.attempt)
                  THEN [workers EXCEPT ![m.worker] = Idle]
                  ELSE workers
    /\ UNCHANGED <<now, recovered, claimers>>

Recover ==
    /\ Recoverable
    /\ row' = IF row.attempt < MaxAttempts
              THEN [row EXCEPT !.status = "Enqueued", !.attempt = @ + 1, !.claimedBy = NoWorker]
              ELSE [row EXCEPT !.status = "Failed", !.claimedBy = NoWorker]
    /\ recovered' = TRUE
    /\ UNCHANGED <<now, workers, net, staleSettle, claimers, lateSettle>>

Tick ==
    /\ now < MaxTime
    /\ \A w \in Workers : ~GiveUpDue(w)
    /\ now' = now + 1
    /\ UNCHANGED <<row, workers, net, staleSettle, recovered, claimers, lateSettle>>

Next ==
    \/ \E w \in Workers : Claim(w) \/ SendRenew(w) \/ GiveUpRun(w) \/ Finish(w)
    \/ \E m \in net : \/ ApplyRenew(m, "reply")
                      \/ ApplyRenew(m, "lose-reply")
                      \/ ApplyRenew(m, "fail")
                      \/ ReadRenewReply(m)
                      \/ ApplySettle(m)
    \/ Recover
    \/ Tick

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Safety.                                                                 *)
(***************************************************************************)

\* A settle applies only for the worker that holds the current attempt.
StaleSettleRejected == ~staleSettle

\* No two workers run the handler of the job at one time.
OneExecutionAtATime ==
    Cardinality({w \in Workers : workers[w].kind = "Running"}) <= 1

(***************************************************************************)
(* Non-vacuity: each must be reachable in the correct protocol.            *)
(***************************************************************************)

JobIsRecovered == recovered
JobIsReclaimed == claimers = Workers
LateSettleArrives == lateSettle
JobCompletes == row.status = "Completed"

=============================================================================
