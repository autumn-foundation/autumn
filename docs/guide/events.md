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
you add it:

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

## Out of scope (today)

- Cross-process delivery via an external broker (Kafka/NATS). Durable fan-out
  rides the existing Postgres/Redis job queue.
- Event sourcing / an append-only event store. This is in-process pub/sub for
  side effects, not persistence.
- Auto-emitting events from model CRUD — that overlaps
  [hooks](hooks-and-transactions.md).
