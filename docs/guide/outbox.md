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

## Relay

The relay claims ready messages, calls their handlers, and marks them sent.

- **Order:** a message is ready only when no older message of its aggregate is
  pending. So the relay sends one aggregate in order. Other aggregates do not
  wait.
- **Several replicas:** on Postgres, the claim uses `FOR UPDATE SKIP LOCKED`.
  Two relays do not claim one message.
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

`batch_size`, `max_attempts` and `lease_ms` must be greater than zero.

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
  system can use to drop copies, for example `webhook-id`.
