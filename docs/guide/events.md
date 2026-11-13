# Events & Listeners (`#[event]` / `#[listener]`)

Autumn provides a **typed domain event bus** so you can react to a business event
with several decoupled side effects. Instead of one fat handler that does
everything inline, you publish a typed event and independent *listeners* react to
it — synchronously in-request, or durably on the background job queue.

The payoff: **adding a new reaction is a new file and zero edits to the code that
publishes the event.**

## When to use it

- You have a handler that triggers several unrelated side effects ("on signup →
  send welcome email **and** seed a workspace **and** record analytics") and you
  want each reaction to live on its own.
- You want some reactions to be guaranteed-durable (survive a restart, get retry +
  DLQ) without hand-rolling a queue subscriber.

For a single side effect tied to one model's CRUD, reach for
[hooks](hooks-and-transactions.md). For client-facing realtime, use
[channels](realtime.md). The event bus is for **server-side, decoupled reactions**.

## Declare an event

```rust,ignore
use autumn_web::prelude::*;

#[event]
struct UserSignedUp {
    user_id: i64,
}
```

`#[event]` derives the serde + `Clone`/`Debug` impls the bus needs (through
`autumn-web`'s re-exported `serde`, so you don't need a direct `serde`
dependency) and implements `autumn_web::events::Event` with a stable `NAME`. The
default `NAME` is **module-qualified** (e.g. `my_app::events::UserSignedUp`) so
two events that share a short name like `Created` in different modules never
collide on the bus; override it with `#[event(name = "user.signed_up")]`.

## Publish an event

Inject the `Events` publisher just like any other extractor and call `publish`:

```rust,ignore
#[post("/signup")]
async fn signup(events: Events /* , db: Db, ... */) -> AutumnResult<Redirect> {
    // ... create the user ...
    events.publish(UserSignedUp { user_id: 42 }).await?;
    Ok(Redirect::to("/welcome"))
}
```

Outside a request (in a service, job, or scheduled task) use the module-level
`autumn_web::events::publish(UserSignedUp { .. }).await?` instead.

## Worked example: welcome email on signup

A listener is an async function over `(AppState, YourEvent)`. Declare it with
`#[listener]`, in **its own file** — the `signup` handler above never changes when
you add it. `Mailer` and `Mail` are behind the non-default `mail` feature
(`features = ["mail"]`); see [Mail](mail.md).

```rust,ignore
use autumn_web::prelude::*;

// Durable: rides the #[job] queue, so it survives a restart and is retried.
#[listener(UserSignedUp, durable, max_attempts = 5)]
async fn send_welcome_email(state: AppState, event: UserSignedUp) -> AutumnResult<()> {
    let mailer = state.extension::<Mailer>().expect("mailer configured");
    let mail = Mail::builder()
        .to(/* look up the user's email by */ format!("user-{}@example.com", event.user_id))
        .subject("Welcome to Acme!")
        .text("Thanks for signing up.".to_string())
        .build()?;
    mailer.send(mail).await
}
```

Want a second reaction? Add another file with another listener — still zero edits
to `signup`:

```rust,ignore
// Synchronous: runs in-request, before the response is returned.
#[listener(UserSignedUp)]
async fn seed_default_workspace(state: AppState, event: UserSignedUp) -> AutumnResult<()> {
    // ... create the user's first workspace ...
    Ok(())
}
```

## Register listeners

Collect listeners with `listeners![..]` and register them on the app, consistent
with how `routes!`/`jobs!`/`tasks!` register today:

```rust,ignore
autumn_web::app()
    .routes(routes![signup])
    .listeners(listeners![send_welcome_email, seed_default_workspace])
    .run()
    .await;
```

**You do not also list durable listeners in `jobs![..]`** — `.listeners(..)` wires
them onto the job runtime for you. Sync listeners need no job runtime at all.

## Sync vs. durable

| Mode | Declared with | Runs | Use for |
|---|---|---|---|
| Sync | `#[listener(Event)]` | in-request, before the response | invariants the caller depends on |
| Durable | `#[listener(Event, durable)]` | on the `#[job]` queue | reactions that must survive a restart |

- **Sync** listeners run before `publish` returns. Each runs **independently and
  isolated**: one listener erroring *or panicking* never blocks the others, and
  never fails the publish. (Their failures are logged.)
- **Durable** listeners are enqueued onto the existing background job queue, so
  they inherit its retry, dead-letter, and restart-safety behavior. Durable
  retry knobs mirror `#[job]`: `max_attempts` and `backoff_ms`.

A published event with **no registered listeners is a no-op**, not an error.

## Delivery & ordering guarantees

- **Durable listeners are at-least-once.** They ride the job queue, so the same
  guarantees as [background jobs](jobs.md) apply: with the `postgres`/`redis`
  backends a reaction is **not lost across a process restart**, and a recovered
  retry can run a listener more than once. **Make durable listeners idempotent**
  (key off the event's domain ids). With the `local` backend, durable listeners
  run in-process and are lost on restart — use `postgres`/`redis` in production.
- **Durable listeners are transaction-aware.** Publishing inside a
  [`Db::tx`](transactions.md) block enqueues durable listeners **after the
  transaction commits** — a rolled-back event never fires them, and the job is
  not picked up before the event's data is visible. (This after-commit deferral
  is process-local, mirroring `job::enqueue_after_commit`: a crash between commit
  and enqueue can drop the reaction.) Publishing outside a transaction enqueues
  immediately.
- **Crash-safe publish: use the outbox.** `events.publish_in_tx(conn, event)`
  writes one outbox row per durable listener on the transaction connection.
  After commit, the relay enqueues the job of the listener. A crash between
  commit and enqueue cannot drop it. The listener keeps its own retry
  settings. Needs `outbox.enabled = true` and a durable job backend. See
  [Transactional Outbox and Inbox](outbox.md).
- **No ordering guarantee between independent listeners.** Sync listeners run
  concurrently; durable listeners are independent queue jobs. Do not rely on one
  listener observing another's effects.
- **No cross-event ordering.** Two published events may have their listeners
  interleave; the bus is fan-out pub/sub, not an ordered log.

## Testing

The in-process test client records every published event, so you can assert on
publications without standing up the job runner:

```rust,ignore
use autumn_web::test::TestApp;

#[tokio::test]
async fn signup_publishes_event() {
    let client = TestApp::new()
        .routes(routes![signup])
        .listeners(listeners![send_welcome_email])
        .build();

    client.post("/signup").send().await.assert_ok();

    client.assert_event_published::<UserSignedUp>();
    let published = client.published_events::<UserSignedUp>();
    assert_eq!(published[0].user_id, 42);
}
```

Recording happens synchronously at publish time, so `assert_event_published`
works whether or not the listeners have run.

## Wire contracts

An event is a contract between a producer and its consumers. After a service
split, the producer and the consumer ship at different times. So `#[event]`
records the wire shape of the payload. Each consumer is checked against it,
as `#[contract_checked]` checks an HTTP caller. See
[Wire contracts](wire-contracts.md).

### Name and version

```rust,ignore
#[event(name = "contact.updated")]
pub struct ContactUpdated {
    pub id: i64,
    pub email: String,
}

#[event(name = "contact.updated", version = 2)]
pub struct ContactUpdatedV2 {
    pub id: i64,
    pub email_address: String,
}
```

- `name` is the topic. Set it on an event that crosses a service boundary.
  The default name holds the module path, so a module move changes it. The
  cross-release check covers only events with a `name`.
- `version` is the schema version. The default is 1. Version 1 keeps the bare
  name. A later version adds `.vN`: the topic of `ContactUpdatedV2` is
  `contact.updated.v2`. So both versions can be on the bus at the same time.
- With `version` above 1, write the base name only. `#[event(name =
  "contact.updated.v2", version = 2)]` is an error.

`Event::NAME` is the topic and `Event::VERSION` is the version.

### The payload shape

`#[event]` also derives `WireShape`. Do not derive it a second time. It
refuses the [same shapes](wire-contracts.md#refusals): generics, enums, tuple
structs, `#[serde(flatten)]`, `#[serde(transparent)]`, `#[serde(into)]` and
the other shapes the macro cannot read.

To publish a payload that `WireShape` refuses, write `#[event(opaque)]`. An
opaque event has no shape. Its consumers are not checked.

A field whose type has no `WireShape` and is not a known leaf, such as an
enum, stops `autumn contracts export`. Mark the field `#[wire(opaque)]`. The
check then compares it by type name only.

Each payload field records its JSON form. `#[wire(type = …)]` names the form
of a field that the macro cannot read. See
[Wire types](wire-contracts.md#wire-types).

### Checked consumers

A `#[listener]` reads fields off its event. Each field it reads must be one
the event sends. If it is not, the build fails at the read:

```rust,ignore
#[event(name = "contact.updated")]
pub struct ContactUpdated {
    pub id: i64,
    #[serde(skip_serializing)]
    pub email: String,
}

#[listener(ContactUpdated)]
async fn sync_marketing(_state: AppState, event: ContactUpdated) -> AutumnResult<()> {
    let _ = event.email; // error: the event does not send `email`
    Ok(())
}
```

```text
error[E0080]: evaluation panicked: event contract broken in `sync_marketing`:
reads field `email` of event `ContactUpdated`, which the event does not send
```

The check sees `event.email`, a struct pattern (in a `let`, a parameter, an
`if let` or a `match`), and a macro body such as
`format!("{}", event.email)`. The field must be in both tables of the event.
A `skip_serializing` field does not arrive. A `skip_deserializing` field
reads as its default value.

For an outbox topic handler, or any function that decodes a payload, use
`#[event_consumer]`:

```rust,ignore
#[event_consumer(OrderPlaced)]
async fn post_to_ledger(state: AppState, message: OutboxMessage) -> AutumnResult<()> {
    let order: OrderPlaced = message.payload_as()?;
    record(&state, order.total_cents).await
}
```

`#[event_consumer]` finds the payload by its type: a parameter
(`order: OrderPlaced`), a typed `let`, a struct pattern
(`let OrderPlaced { total_cents, .. } = …`), or a turbofish
(`payload_as::<OrderPlaced>()`). Write the type as the attribute writes it,
or shorter: `legacy::OrderPlaced` is another type. If the macro finds no
payload, it refuses, because a check that sees no payload checks nothing.

`#[event_consumer]` also works on a method in an `impl` block.

### Descriptors and the cross-release check

`#[event]` writes an `event.*.json` descriptor, and each consumer writes a
`listener.*.json` descriptor with its read-set. `autumn contracts export`
puts both in the release bundle. `autumn contracts check` then checks events
across releases. See [Events across releases](wire-contracts.md#events-across-releases).

### Limits

- The check sees only the fields that the consumer names. It does not see
  reads in a function that gets the whole payload.
- A hand-written `impl Event` has no shape. Its consumers are not checked.
- An event without `name` and its consumers are not in the release bundle.
  The compile-time check still applies to them.
- A consumer that names its event through an alias (`use x::ContactUpdated
  as Updated`) is not in the bundle. Use the event's own type name.
  `autumn contracts export` warns about each consumer it cannot place. The
  check names them, and then it does not say that a version can be dropped.
- Two `#[event]` types with one type name in one crate (`v1::OrderPlaced`
  and `v2::OrderPlaced`) stop the export. Give each its own type name.
- An `#[event]` type writes an `event.*` descriptor only, not a `type.*`
  one. Do not use it as the body of an `#[endpoint]`: use a separate DTO.
- The bundle finds a consumer's event by its type name and the first
  segment of its path. When that segment names no crate in the pass (a
  module or a renamed dependency), and both the consumer's crate and
  another crate have an event of that name, the export stops. Write
  `crate::…` for a local event, or the package name of the dependency.

## Out of scope (today)

- Cross-process delivery through an external broker (Kafka/NATS). To send
  an event to another Autumn service, use an outbox destination. See
  [Send to another service](outbox.md#send-to-another-service).
- Event sourcing / an append-only event store. This is in-process pub/sub for
  side effects, not persistence.
- Auto-emitting events from model CRUD — that overlaps
  [hooks](hooks-and-transactions.md).
