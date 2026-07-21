# Declarative State Machines

Autumn's `#[state_machine]` field attribute lets you declare valid status
transitions directly in your model struct. The macro generates a compile-time
transition table, a `can_transition_{field}_to` predicate, and a
`transition_{field}_to` method that enforces the graph at runtime — no
hand-written `match` blocks or scattered `if old_status == "draft" { ... }`
checks spread across route handlers.

---

## Model setup

Add `#[state_machine(transitions(...))]` to any `String` field:

```rust
#[autumn_web::model]
pub struct Order {
    #[id]
    pub id: i64,
    pub amount: i64,
    #[state_machine(transitions(
        pending -> processing,
        processing -> shipped: "can_ship",
        processing -> cancelled,
        shipped -> delivered,
    ))]
    pub status: String,
}

impl Order {
    fn can_ship(&self) -> bool {
        self.amount > 0
    }
}
```

Each line inside `transitions(...)` declares one edge:

```
from_state -> to_state
from_state -> to_state: "guard_method_name"
```

A trailing comma after the last transition is accepted. State names are
unquoted identifiers; underscores and digits are allowed (`in_progress`,
`state_2`). Guard names are quoted strings that must name a `&self -> bool`
method on the model.

---

## Generated API

For a field named `status` the macro generates three items on the struct:

| Item | Signature | Purpose |
|------|-----------|---------|
| `__AUTUMN_SM_STATUS_TRANSITIONS` | `&'static [(&'static str, &'static str, Option<&'static str>)]` | Compile-time edge list `(from, to, guard_name)` |
| `can_transition_status_to` | `(&self, target: &str) -> bool` | Predicate — true when the transition is defined and all guards pass |
| `transition_status_to` | `(&self, target: &str) -> AutumnResult<String>` | Enforcing version — returns `Ok(target.to_owned())` or a 400 error |

For a field named `phase`, replace `STATUS` / `status` with `PHASE` / `phase`
throughout.

---

## Guard methods

A guard is called on `self` at the time `transition_{field}_to` (or
`can_transition_{field}_to`) is called. The record it receives is whatever
you pass — usually the current database snapshot.

```rust
// In a route or service:
let new_status = order.transition_status_to("shipped")?;
// → calls order.can_ship(); returns Err if false or if edge doesn't exist
```

Because the guard receives the record as-is, you can check any field:

```rust
impl Invoice {
    fn can_approve(&self) -> bool {
        self.line_item_count > 0 && self.total_cents > 0
    }
}
```

An unguarded edge (`pending -> processing`) always succeeds when the `from`
state matches.

---

## Enforcing transitions in `before_update`

The most common integration point is the `before_update` hook. Check the
incoming status change against the transition table before the SQL runs:

```rust
impl MutationHooks for OrderHooks {
    type Model = Order;
    type NewModel = NewOrder;
    type UpdateModel = UpdateOrder;

    async fn before_update(
        &self,
        _ctx: &mut MutationContext,
        draft: &mut UpdateDraft<Order>,
    ) -> AutumnResult<()> {
        if draft.after.status != draft.before.status {
            // Build a "proposed" record: new field values but the current
            // status, so guards evaluate the content being persisted rather
            // than stale before-state values.
            let mut proposed = draft.after.clone();
            proposed.status = draft.before.status.clone();
            proposed.transition_status_to(&draft.after.status)?;
        }
        Ok(())
    }
}
```

**Why clone?** Guards run on `self`, which in the simple `draft.before.transition_status_to(...)` pattern means they see the record's *old* field values. If a user submits a status change together with edits to fields the guard reads (e.g. clearing a body field while publishing), the guard would evaluate the old data and give the wrong answer. Cloning `draft.after` and then restoring only the `status` to the before-value lets the edge lookup work correctly while the guard sees the *proposed* final content.

If you only want to check without returning an error (for example, to pick a
default):

```rust
let mut proposed = draft.after.clone();
proposed.status = draft.before.status.clone();
if !proposed.can_transition_status_to(&draft.after.status) {
    draft.after.status = draft.before.status.clone(); // revert silently
}
```

---

## Multiple state machine fields

A single model can have several `#[state_machine]` fields. Each generates its
own constant and pair of methods:

```rust
#[autumn_web::model]
pub struct Ticket {
    #[id]
    pub id: i64,
    #[state_machine(transitions(
        open -> in_progress: "can_start",
        in_progress -> closed,
    ))]
    pub status: String,
    #[state_machine(transitions(
        low -> medium,
        medium -> high,
    ))]
    pub priority: String,
}
```

This generates:
- `Ticket::__AUTUMN_SM_STATUS_TRANSITIONS`
- `Ticket::can_transition_status_to` / `Ticket::transition_status_to`
- `Ticket::__AUTUMN_SM_PRIORITY_TRANSITIONS`
- `Ticket::can_transition_priority_to` / `Ticket::transition_priority_to`

---

## Runtime reflection

The transitions constant is public and carries the guard name as a string so
you can build UI or API metadata from it at runtime:

```rust
for (from, to, guard) in Order::__AUTUMN_SM_STATUS_TRANSITIONS {
    println!("{from} → {to}{}",
        guard.map_or(String::new(), |g| format!(" [guard: {g}]")));
}
```

You can also expose allowed next states to a client:

```rust
let next_states: Vec<&str> = Order::__AUTUMN_SM_STATUS_TRANSITIONS
    .iter()
    .filter(|(from, _, _)| *from == order.status.as_str())
    .map(|(_, to, _)| *to)
    .collect();
```

---

## Constraints

- Only `String` fields are supported. Attempting `#[state_machine]` on an `i32`
  or other type is a compile error.
- Multiple `#[state_machine]` attributes on the same field are rejected at
  compile time.
- State names and guard names must be valid Rust identifiers (no spaces, no
  hyphens). Use underscores: `in_progress`, not `in-progress`.
- The transition graph is not validated for reachability or completeness. Dead
  states and disconnected subgraphs compile fine — they just can never be
  reached at runtime.

---

## Wiki example

[`examples/wiki`](../../examples/wiki) ships a `Page` model with `draft`,
`published`, and `archived` states. `#[state_machine]` is added to its `status`
field and the `PageHooks::before_update` implementation enforces valid
transitions, so a direct API call or form submission cannot skip
`draft → published → archived` or jump backward. Each edge also declares a
[transition effect](transition-effects.md) (`on = "record_publish_revision"` /
`on = "record_archive_revision"`) that appends the audit `Revision` inside the
transition's transaction, and the `transition_status` handler drives the
effectful `transition_status_to_on_conn` under `Db::tx_with` so the status change
and its audit row commit atomically. See `examples/wiki/src/models.rs` and
`examples/wiki/src/routes/pages.rs`.
