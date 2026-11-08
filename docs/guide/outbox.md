# Transactional outbox and inbox

A handler often changes the database and then sends work to another system.
The process can stop between these two writes. Then the data is saved and
the work is lost. This is the dual-write problem. The outbox removes it.

- **Outbox:** you write a message in the same transaction as your data. The
  message commits or rolls back with the data. A relay sends it after commit.
  A crash cannot lose a committed message.
- **Inbox:** the relay sends a message **at least once**. A crash at the
  wrong time causes a second send. The consumer records each message id and
  drops a copy.

The framework uses one table, `autumn_outbox`, for your messages and for its
own work: durable event listeners, job enqueue, `deliver_later` and webhook
dispatch. It works on Postgres and on SQLite.

## Turn it on

```toml
[outbox]
enabled = true
```

Or set `AUTUMN_OUTBOX__ENABLED=true`. At boot, the app then:

1. Creates `autumn_outbox` and `autumn_inbox` if they do not exist.
2. Starts the relay worker on each replica that runs workers (`role =
   "combined"` or `"worker"`).
3. Sends `deliver_later` mail through the outbox, if you did not set a
   `MailDeliveryQueue`.

On Postgres, the app sends no DDL when the two tables exist. If the database
role of the app cannot create tables, apply `autumn_web::outbox::SCHEMA_SQL`
in a migration before the deploy.

When the outbox is off, a write returns an error. A message that no relay
sends is never written.

## Write a message

Get an `Outbox` in the handler. Give `write` the connection of the open
transaction.

```rust
use autumn_web::outbox::Outbox;
use autumn_web::prelude::*;
use scoped_futures::ScopedFutureExt as _;

#[post("/orders")]
async fn place_order(mut db: Db, outbox: Outbox, Json(order): Json<NewOrder>) -> AutumnResult<()> {
    db.tx(|conn| {
        async move {
            let order = insert_order(conn, order).await?;
            outbox
                .write(conn, &format!("order:{}", order.id), "order.placed", &order)
                .await?;
            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
}
```

`write(conn, aggregate, topic, payload)`:

- `aggregate` is the ordering key. The relay sends the messages of one
  aggregate in write order. Use the id of the changed entity.
- `topic` selects the handler. It must have a handler in this app, so a
  typing error fails at once. Use only `A-Z a-z 0-9 . _ : -`. The `autumn.`
  prefix is reserved.
- `payload` is any `Serialize` value. The relay stores it as JSON.

> **Caution:** use the transaction connection. A message written on a
> different connection does not roll back with your data.

## Handle a message

Register one handler per topic:

```rust
autumn_web::app()
    .outbox_handler("order.placed", post_to_ledger)
    .run()
    .await;
```

A handler gets the `AppState` and an `OutboxMessage` (`id`, `aggregate`,
`topic`, `payload`, `attempt`, `created_at`). Return `Ok(())` to mark the
message sent. Return an error, or panic, to send it again later.

When the payload is an `#[event]`, put `#[event_consumer]` on the handler.
Then each payload field the handler reads must be one the event sends, or the
build fails. The read-set also goes into the release bundle, so
`autumn contracts check` covers the topic. See
[Events: wire contracts](events.md#wire-contracts).

```rust
#[event_consumer(OrderPlaced)]
async fn post_to_ledger(state: AppState, message: OutboxMessage) -> AutumnResult<()> {
    let order: OrderPlaced = message.payload_as()?;
    record(&state, order.total_cents).await
}
```

## Drop copies with the inbox

Call `Inbox::seen` in the same transaction as the side effect. Then the inbox
record and the effect commit together.

```rust
use autumn_web::outbox::{Inbox, OutboxMessage};
use scoped_futures::ScopedFutureExt as _;

async fn post_to_ledger(state: AppState, message: OutboxMessage) -> AutumnResult<()> {
    let pool = state
        .pool()
        .ok_or_else(|| AutumnError::service_unavailable_msg("no database"))?;
    let mut conn = pool.get().await?;
    autumn_web::db::scoped_transaction(&mut *conn, |conn| {
        async move {
            if Inbox::new("ledger").seen(conn, &message.id).await? {
                return Ok(()); // a copy: the effect is already done
            }
            insert_ledger_entry(conn, &message).await?;
            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
}
```

`seen` returns `true` when the id is already recorded for that consumer. Each
consumer name has its own set of ids. In code that the handler calls and that
does not get the message, `autumn_web::outbox::current_message_id()` gives
the id.

## Framework writers

| Work | In the transaction | Before |
| --- | --- | --- |
| Durable event listeners | `outbox.publish(conn, &event)` or `events.publish_in_tx(conn, event)` | Enqueued after commit, in process. A crash after commit dropped the reaction. |
| Job enqueue (Redis, SQLite) | `outbox.enqueue_job(conn, "name", &args)` | Not in the transaction. On Postgres, use `job::enqueue_in_tx`. |
| Mail | `outbox.deliver_mail(conn, mail)` | `deliver_later` used a `tokio::spawn` with no retry. |
| Outbound webhooks | `outbox.dispatch_webhook(conn, "topic", &payload)` or `manager.dispatch_in_tx(&state, conn, "topic", &payload)` | `dispatch` ran after the data commit, with no guarantee. |

Each of these writes one message with its own id as the aggregate. These
messages have no order between them.

- **Events and jobs:** after commit, the relay enqueues the job (for an
  event, the job of each durable listener). The listener keeps its own
  `max_attempts` and backoff. After the enqueue, the job backend holds the
  work. The `local` backend loses it in a crash, so use a durable backend.
- **Mail:** with `outbox.enabled = true`, a plain `mailer.deliver_later(mail)`
  also uses the outbox. It writes the row after the commit of the caller,
  from a spawned task. A crash in that short time can still lose the mail. To
  prevent this loss, call `outbox.deliver_mail` in the transaction.
- **Webhooks:** each delivery id comes from the message id and the
  subscription id. A second relay send skips a delivery that has a result.
  Before the first result, it can enqueue the delivery again. The copy has
  the same `webhook-id`, so the receiver can drop it. The delivery itself
  runs on the job backend and the webhook store, so use a durable job backend
  and a durable store (`OutboundWebhookPlugin::sql()`).

## Send to another service

A module can become its own service, with its own database. Then a handler
in that service cannot run in the same process. Use a remote destination.
The relay sends the messages of some topics to the inbox of the other
service. The outbox rules stay the same:

- The message commits or rolls back with your data.
- The relay sends it at least once, and the inbox drops each copy.
- The relay sends the messages of one aggregate in order.
- A failed send waits with jitter, then goes again. After `max_attempts`, it
  is a dead letter. `requeue` sends it again.

### Example: core sends `DealWon` to marketing

Both services use the event from a shared crate. The event must have a
`name`. See [Events: wire contracts](events.md#wire-contracts).

```rust
// contracts/src/lib.rs
use autumn_web::event;

#[event(name = "deal.won")]
pub struct DealWon {
    pub deal_id: i64,
    pub amount_cents: i64,
}
```

**Core** routes the topic to marketing:

```toml
# core/autumn.toml
[outbox]
enabled = true

[outbox.destinations.marketing]
url = "https://marketing.internal/_autumn/inbox"
signing_secret = "${MARKETING_INBOX_SECRET}"
topics = ["deal.won"]
```

Core writes the message in the transaction of its data. The topic is
`DealWon::NAME`. The deal id is the aggregate.

```rust
use autumn_web::events::Event as _;
use autumn_web::outbox::Outbox;
use autumn_web::prelude::*;
use contracts::DealWon;
use scoped_futures::ScopedFutureExt as _;

#[post("/deals/{id}/win")]
async fn win_deal(Path(id): Path<i64>, mut db: Db, outbox: Outbox) -> AutumnResult<()> {
    db.tx(|conn| {
        async move {
            let deal = mark_won(conn, id).await?;
            let won = DealWon { deal_id: id, amount_cents: deal.amount_cents };
            outbox
                .write(conn, &format!("deal:{id}"), DealWon::NAME, &won)
                .await?;
            Ok::<_, AutumnError>(())
        }
        .scope_boxed()
    })
    .await
}
```

**Marketing** serves the inbox. It uses the same secret, and trusts its own
host name:

```toml
# marketing/autumn.toml
[outbox]
enabled = true

[outbox.inbox]
enabled = true
signing_secret = "${MARKETING_INBOX_SECRET}"

[security.trusted_hosts]
hosts = ["marketing.internal"]
```

Marketing handles the topic with a `#[listener]`:

```rust
use autumn_web::prelude::*;
use contracts::DealWon;

#[listener(DealWon, durable)]
async fn start_campaign(state: AppState, event: DealWon) -> AutumnResult<()> {
    // `current_message_id()` returns `Some` with the id that core wrote.
    let id = autumn_web::outbox::current_message_id();
    send_thank_you(&state, event.deal_id, id).await
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .listeners(listeners![start_campaign])
        .run()
        .await;
}
```

### What happens

1. Core commits. The row is in the core `autumn_outbox`. A rollback writes
   no row, so nothing goes to marketing.
2. The core relay POSTs the message to the marketing inbox.
3. The inbox checks the signature. Then, in one transaction, it records the
   message id with `Inbox::seen` and writes one row for each listener of the
   topic in the marketing `autumn_outbox`. It returns `200`.
4. The core relay marks the message sent.
5. The marketing relay runs each listener. Each listener has its own retries
   and dead letters, and keeps the order of each aggregate.

If core does not get the `200`, it sends the message again. The inbox finds
the id, returns `200` with `"status": "duplicate"`, and writes nothing. So
marketing takes one copy only. The marketing relay itself sends at least
once: a crash after the listener and before the relay marks the row sent
runs the listener again. For an effect that occurs one time only, call
`Inbox::seen` with `current_message_id()` in the transaction of the effect.

### The request

`POST /_autumn/inbox` with a JSON body:

```json
{
  "id": "0b6c…",
  "topic": "deal.won",
  "aggregate": "deal:42",
  "created_at": "2026-11-08T10:00:00Z",
  "payload": { "deal_id": 42, "amount_cents": 990000 }
}
```

| Header | Value |
| --- | --- |
| `Autumn-Signature` | `t=<unix seconds>,v1=<hex HMAC-SHA256 of "{t}.{body}">` |
| `Autumn-Message-Id` | The message id. For logs only. |
| `Autumn-Topic` | The topic. For logs only. |

The signature covers the whole body, so a copy cannot change the id, the
topic or the aggregate. `autumn_web::outbox::inbox_signature` makes the
header value. It is the scheme of
[outbound webhooks](outbound-webhooks.md) (`Autumn-Signature`).

| Status | Cause | The relay |
| --- | --- | --- |
| `200`, `"accepted"` | The listeners have the message. | Marks it sent. |
| `200`, `"duplicate"` | The inbox has the id. | Marks it sent. |
| `400` | No signature, or a bad body. | Sends again. |
| `401` | A bad signature, or a time not in `timestamp_tolerance_secs`. | Sends again. |
| `413` | The body is larger than `max_body_bytes`. | Sends again. |
| `503` | This release has no listener for the topic, or no database. | Sends again. |

Only a `200` with one of these two statuses marks the message sent. The
relay does not follow a redirect, so a login page does not count as a
delivery. The relay sends again after each other result. Thus a rolling
deploy is safe: a topic that arrives before the release that handles it
gets `503`, and waits. The later messages of its aggregate wait too. Deploy
the receiver first, or keep `max_attempts` high enough for the rollout.

### Rules

- **Contracts:** each topic of a destination must be an
  `#[event(name = "...")]` in the binary of the producer. If it is not, the
  boot fails. Thus `autumn contracts check` covers the producer and each
  consumer. For an `#[event(opaque)]` it checks only the name and version.
  See [Events across releases](wire-contracts.md#events-across-releases).
- **One place per topic:** a topic goes to one destination. It cannot also
  have an `outbox_handler`.
- **Roles:** the relay sends only on a process with the `outbox_relay`
  capability (`combined` and `worker`). The inbox endpoint also runs on a
  `web` process. Its rows wait for a relay of that service.
- **Listeners:** on the receiver, each listener of the topic runs through
  the relay, sync or durable. The `[outbox]` settings of the receiver set
  its attempts and backoff, not the job settings of the listener. A queued
  row names its listener by module path. If you move or rename a listener,
  its queued rows fail and become dead letters. Let the queue empty first.
- **Retention:** the inbox keeps each id for `retention_ms`. The receiver
  can take a message, but the producer can miss the `200` and make it a dead
  letter. If the producer requeues it after `retention_ms`, the listener can
  run again.
- **Secrets:** `"${NAME}"` reads the environment variable `NAME`. An unset
  variable stops the boot. In production, a weak secret stops the boot. To
  rotate: on the receiver, set the new secret in `signing_secret` and move
  the old one to `previous_signing_secrets`. Change the producer. Then
  remove the old secret.
- **Trust:** the inbox has one secret. Each service that has it can send
  each topic with each id. Give it only to services you trust as much as
  your own.
- **Clocks:** the clocks of the two services must agree to within
  `timestamp_tolerance_secs` (default 300). `retention_ms` must be longer
  than two times this value, so a signature ends before the inbox forgets
  its id.
- **Hosts:** the receiver must trust the host of `url` in
  `[security.trusted_hosts]`. The relay does not filter the URL for private
  addresses: you set it, so an internal host is correct.
- **Middleware:** the endpoint is exempt from CSRF, one-time submit tokens
  and CAPTCHA. Its signature protects it.

## Relay

The relay claims ready messages, calls their handlers, and marks them sent.

- **Order:** a message is ready only when no older message of its aggregate is
  pending. So the relay sends one aggregate in order. Other aggregates do not
  wait.
- **Several replicas:** on Postgres, the claim uses `FOR UPDATE SKIP LOCKED`.
  Two relays do not claim one message.
- **Shards:** a write on a shard connection puts the row in that shard. The
  relay drains the app pool, if there is one, and the primary pool of each
  shard. At boot, the app creates the tables on each of them. A shard that
  is down at boot gets its tables when it comes back. With no app
  pool, `deliver_later` writes on the first shard. If one pool fails, the relay logs
  the error and drains the others.
- **Lease:** a claim holds a batch for `lease_ms`. A handler that is still
  running when the lease ends fails, and the relay stops the batch. It gives
  the rest of the batch back. It also gives it back on shutdown.
- **Crash:** if the relay stops, its claim ends after `lease_ms`. Then
  another relay sends the message again.
- **Retry:** a failed message waits `initial_backoff_ms`, doubled for each
  attempt, up to `max_backoff_ms`. The real delay is a random value between
  half the delay and the full delay (equal jitter).
- **Dead letters:** each claim counts one attempt, also a claim that a crash
  ends. After `max_attempts`, the message stops. Later messages of its
  aggregate continue. Read the dead letters with
  `outbox.dead_letters(conn, limit)`. Send one again with
  `outbox.requeue(conn, id)`.
- **Rolling deploys:** a relay claims only topics it has a handler for. A
  message with a new topic waits for a replica that knows it. The later
  messages of its aggregate wait too. An old replica that does not know a new
  job or listener fails that message, and each failure uses an attempt.
  Deploy the workers first, or keep `max_attempts` high enough for the
  rollout.
- **Clean-up:** the relay deletes sent messages and inbox entries older than
  `retention_ms`. Keep `retention_ms` longer than the time a copy can arrive.
  Dead letters stay until you requeue or delete them. Mail rows hold
  addresses and bodies until the clean-up.

> **Caution:** on Postgres, the database gives the sequence number at insert,
> not at commit. Two transactions that write one aggregate at the same time
> can commit out of order. Use a row lock on the aggregate to serialize the
> writes. A handler that changes one entity usually holds this lock.

## Configuration

| Key | Default | Env |
| --- | --- | --- |
| `outbox.enabled` | `false` | `AUTUMN_OUTBOX__ENABLED` |
| `outbox.poll_interval_ms` | `500` | `AUTUMN_OUTBOX__POLL_INTERVAL_MS` |
| `outbox.batch_size` | `100` | `AUTUMN_OUTBOX__BATCH_SIZE` |
| `outbox.max_attempts` | `10` | `AUTUMN_OUTBOX__MAX_ATTEMPTS` |
| `outbox.initial_backoff_ms` | `1000` | `AUTUMN_OUTBOX__INITIAL_BACKOFF_MS` |
| `outbox.max_backoff_ms` | `300000` | `AUTUMN_OUTBOX__MAX_BACKOFF_MS` |
| `outbox.lease_ms` | `60000` | `AUTUMN_OUTBOX__LEASE_MS` |
| `outbox.retention_ms` | `604800000` | `AUTUMN_OUTBOX__RETENTION_MS` |
| `outbox.destinations.<name>.url` | none | — |
| `outbox.destinations.<name>.signing_secret` | none | — |
| `outbox.destinations.<name>.topics` | none | — |
| `outbox.inbox.enabled` | `false` | — |
| `outbox.inbox.signing_secret` | none | — |
| `outbox.inbox.previous_signing_secrets` | `[]` | — |
| `outbox.inbox.timestamp_tolerance_secs` | `300` | — |
| `outbox.inbox.max_body_bytes` | `1048576` | — |

A secret value of the form `"${NAME}"` reads the environment variable
`NAME` at load.

`batch_size`, `max_attempts` and `lease_ms` must be greater than zero.
`max_backoff_ms` must not be smaller than `initial_backoff_ms`.

## Tables

All times are epoch milliseconds from the app clock.

`autumn_outbox`: `seq` (order), `id`, `aggregate`, `topic`, `payload` (JSON
text), `created_at`, `available_at`, `attempts`, `last_error`,
`claim_token`, `locked_until`, `dispatched_at`, `dead_at`.

`autumn_inbox`: `consumer`, `message_id`, `seen_at`. The key is
`(consumer, message_id)`.

## Testing

`TestApp` runs no relay worker. Enable the outbox, create the tables, and
drain:

```rust
let client = TestApp::new()
    .with_db(pool.clone())
    .with_outbox(OutboxConfig::default())
    .outbox_handler("order.placed", post_to_ledger)
    .build();
autumn_web::outbox::ensure_schema(&pool).await?;
// ... make a request that writes a message ...
autumn_web::outbox::drain(client.state(), 100).await?;
```

Under a `Sim`, `sim.run_to_idle()` drains the outbox. After
`sim.crash_and_restart(..)`, advance the clock by `lease_ms` first, so the
claims of the dead relay end. See `autumn/tests/sim_outbox_crash.rs` and
`autumn/tests/sim_outbox_mail.rs`.

## Not included

- Change data capture (Debezium, logical replication). The relay polls.
- An `#[inbox]` attribute. Call `Inbox::seen` in the handler.
- Exactly-once side effects outside your database. The inbox makes a
  database effect exactly once. A call to another system needs an id that
  system can use to drop copies, for example `webhook-id`. Another Autumn
  service gets this from its inbox: see
  [Send to another service](#send-to-another-service).
