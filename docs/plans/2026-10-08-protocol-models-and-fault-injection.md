# Protocol Models and Staging Fault Injection (issue #3071)

> **Status: executed.** Parent: #3050. Prior work: #3051 (job heartbeat),
> #3052 (tick table), #3053 (lease lock), #3066 (Verus in CI).

## Gap audit

| Item | State before this plan | Gap |
| --- | --- | --- |
| Verus in CI | Done. `verus.yml` runs every `verification/*.rs`. | None. |
| Job claim model | Verus proves one-step rules only. | No check of interleavings. |
| Tick election model | None. | No model. |
| Lease-lock model | Verus proves one-step rules only. | No check of interleavings. |
| Seeded bugs | None. | No proof that a check can fail. |
| Staging fault injection | `FaultPlan` is test-only. | No runtime seam. |

## Facts (white hat)

- A job claim is fenced by `claimed_by = $me AND status = 'running'`. There
  is no generation column. The worker gives up at 2/3 of the visibility
  timeout. Recovery starts after 3/3.
- A tick is claimed by `INSERT … ON CONFLICT DO NOTHING RETURNING
  generation`. The row stays for `retention + period`. `free` deletes only
  the row with the same generation.
- A lease grant increments `generation`. Renew and release check the
  generation. The resource admits a write when `stored <= incoming`.
- Nothing in the server computes an error-budget burn at run time.

## Brainstorming

1. Stateright models in a new workspace crate.
2. TLA+ specs, checked with TLC in CI.
3. A small model checker that we write.
4. More Verus proofs.
5. Shuttle tests over the real code.
6. A tower layer for route faults, with a config section.
7. A task-local fault scope that the DB checkout, the HTTP client and the
   Redis session store read.
8. Wrap the user's `DbConnectionInterceptor`.
9. An actuator endpoint to toggle faults.
10. A stop condition from the SLO burn-rate rule.

Selected: 1, 6, 7, 10. Item 2 needs Java in CI and a second language.
Item 3 needs its own proof of correctness. Item 4 does not explore
interleavings. Item 5 needs the database. Item 8 changes which interceptor runs when two are installed.
Item 9 adds an attack surface. A programmatic handle is sufficient.

## Reverse brainstorming ("how do we make this fail?")

| Failure we could cause | Prevention |
| --- | --- |
| A model passes because it never reaches a bad state. | Each seeded bug must give a counterexample. `sometimes` properties show that recovery and reclaim occur. |
| A model does not match the SQL. | Each action names its statement. The guard is a plain function. |
| The state space grows, and CI is slow. | Small bounds. A test asserts an upper limit on states. |
| Faults run in production by accident. | Config validation and the layer builder both refuse `prod`. Only `allow_in_production = true` overrides it. |
| Faults keep running while users suffer. | A burn-rate stop condition. It stays disarmed until an operator arms it again. |
| Injected latency causes a timeout that the stop condition does not see. | A dropped request with an injected fault counts as an error. |
| A health probe fails, and the orchestrator kills the pod. | Probe and actuator paths are never faulted. |
| A toggle leaves no trace. | Each toggle writes an audit event and a `warn` log. |
| Tests are not deterministic. | Random decisions use the app entropy. Windows use tokio time, which a paused test runtime controls. |
| A new dependency adds an advisory. | `cargo deny` passes with Stateright. |

## Six thinking hats

- **White:** see "Facts".
- **Red:** fault injection in a framework is alarming. The refusal and the
  logs must be easy to see.
- **Black:** the risks are a new dependency, model drift and scope growth. Redis is limited to
  the session store; other Redis users are out of scope.
- **Yellow:** seeded bugs prove that the checks work. Teams can inject faults
  in staging without a service mesh.
- **Green:** reuse `slo::max_error_ppm` for the stop rule, so one burn rule
  exists.
- **Blue:** TDD order: model tests (red), models (green), layer tests (red),
  layer (green), wiring, docs, review.

## Decisions

1. Crate `autumn-protocol-models` at `verification/models`. It is not
   published. `cargo test --workspace` runs it. The `Protocol models` CI job
   also runs it.
2. Each model has a `Variant`. `Correct` passes. Each seeded bug fails one
   named property.
3. `[fault_injection]` config. `FaultInjectionLayer` is the innermost
   framework layer. A task-local scope gives dependency faults to the
   database checkouts, to `http_client::Client::send` and to the Redis
   session store.

## Result

The tick model found a bug. A cron task that waited in the cost gate past its
window could claim a pruned tick row and run the occurrence twice.
`execute_cron_task` now checks the window after the wait.
