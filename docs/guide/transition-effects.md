# Transition effects

Autumn's lifecycle primitives — the compile-time [`#[lifecycle]`](lifecycle.md) typestate and the runtime [`#[state_machine]`](state-machines.md) string column — describe *which* transitions are legal. **Transition effects** describe what should *happen* when a legal transition fires: write an audit row, send an email, enqueue downstream work. Effects are declared per edge, right next to the transition table, in two flavours:

- **`on`** — synchronous, runs *inside* the transition's transaction. Returning an `Err` rolls the whole transition back.
- **`on_commit`** — asynchronous, enqueued transactionally and dispatched *after* the transaction commits, through the durable job outbox, with an auto-derived idempotency key.

---

## Declaring effects

Each edge in a `transitions(...)` list may carry a `key = value` suffix after `:`. The keys are `guard = "..."`, `on = "..."`, and `on_commit = <Job>`; they may appear in any order and are each optional.

```rust
#[model(table = "orders")]
pub struct Order {
    #[id]
    pub id: i64,
    #[state_machine(transitions(
        pending -> processing,
        // A pure synchronous, in-transaction effect.
        processing -> shipped: on = "record_audit",
        // A guard, a sync `on`, and an after-commit `on_commit` compose on one edge.
        shipped -> archived: guard = "can_archive", on = "record_audit", on_commit = AnnounceArchiveJob,
    ))]
    pub status: String,
}
```

> There is no separate `sync` keyword — a synchronous effect is simply the `on =` key. Declaring any effect is what upgrades the edge from a plain transition to an effectful one.

---

## `on` — synchronous, in-transaction

`on = "method"` names an inherent `async` method on the model:

```rust
impl Order {
    async fn record_audit(&self, conn: &mut AsyncPgConnection) -> AutumnResult<()> {
        // Runs inside the transition's transaction.
        Ok(())
    }
}
```

- It runs on the same connection and inside the same transaction that persists the new state, so the effect and the state change commit **atomically**.
- Returning `Err` **aborts the transition and rolls back** — the state does not advance.
- Reach for `on` when the effect must stay consistent with the state change: audit rows, derived columns, cross-row invariants.

---

## `on_commit` — asynchronous, after commit

`on_commit = <Job>` names a `#[job]` struct. The job is enqueued on the transition's own connection *inside* the transaction, so it is only ever dispatched if the transition commits (a transactional outbox); it then runs after commit, off the request path.

```rust
#[state_machine(transitions(
    processing -> shipped: on_commit = SendShippedEmailJob,
))]
```

The job receives a framework-provided `TransitionEffect` describing the edge that fired:

```rust
#[job(name = "send_shipped_email", unique, by = ["idempotency_key"])]
async fn send_shipped_email(state: AppState, effect: TransitionEffect) -> AutumnResult<()> {
    // effect.model / .field / .record_id / .from_state / .to_state / .idempotency_key
    Ok(())
}
```

### Idempotency

Every `on_commit` effect carries a dedup key derived automatically from the edge:

```
{model}:{field}:{record_id}:{from_state}:{to_state}
```

Declaring the job `#[job(unique, by = ["idempotency_key"])]` (as above) collapses a retried or duplicated enqueue to a single run, so the same order can never send two "shipped" emails even if the transition is replayed.

---

## Choosing between them

| | `on` | `on_commit` |
| --- | --- | --- |
| Runs | inside the transition transaction | after commit, via the outbox |
| On failure | rolls the transition back | retried by the job runner; never blocks the transition |
| Latency | on the request/transition path | off the request path |
| Use for | invariants and audit consistent with the state | emails, webhooks, downstream fan-out |

---

## Effects on a `lifecycle = <Enum>` machine

When the transition table comes from a `#[lifecycle]` enum rather than an inline list, declare effects at the binding site with a separate `effects(...)` clause (the enum owns legality; `effects(...)` only attaches side effects):

```rust
#[lifecycle(initial = Draft, terminal(Archived), transitions(
    Draft -> Published,
    Published -> Archived,
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderState {
    Draft,
    Published,
    Archived,
}

#[model(table = "articles")]
pub struct Article {
    #[id]
    pub id: i64,
    #[state_machine(lifecycle = OrderState, effects(
        Draft -> Published: on_commit = AnnouncePublishJob,
        Published -> Archived: on = "record_archive",
    ))]
    pub status: String,
}
```

- **Guards are not allowed** inside `effects(...)` — lifecycle transitions are unguarded (the enum already defines which edges are legal). Each `effects(...)` edge must declare `on` and/or `on_commit`.

---

## Generated API

Declaring any effect gives the model a transaction-aware transition method, `transition_{field}_to_on_conn(&self, conn, target)`, which validates the edge, applies the synchronous `on` effect, and enqueues any `on_commit` job — all on the supplied connection, so callers stay in control of the surrounding transaction.

---

## See also

- [Typed lifecycles](lifecycle.md)
- [State machines](state-machines.md)
- [Background jobs](jobs.md)
