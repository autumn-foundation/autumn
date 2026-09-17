# Money and the double-entry ledger

Autumn gives you a typed money value and an append-only, double-entry ledger
that the framework enforces. A retried charge, a replayed webhook, or a crash
halfway through a post cannot double-count money, lose it, or leave the books
unbalanced.

The ledger in this page is the **money** ledger. It is not
[`autumn_web::ledger`](ledgered-entities.md), which records the history of a
`#[repository]` row.

- [Migrations](#migrations)
- [The money value](#the-money-value)
- [Rounding and splitting](#rounding-and-splitting)
- [The ledger](#the-ledger)
- [Idempotency](#idempotency)
- [Balances](#balances)
- [What the framework enforces](#what-the-framework-enforces)
- [Not in this slice](#not-in-this-slice)

## Migrations

The ledger keeps three framework tables — `_autumn_money_accounts`,
`_autumn_money_transactions` and `_autumn_money_postings`. They ship in
Autumn's own migration set, so `autumn migrate` creates them with the rest of
the framework schema. There is nothing to add to your own `migrations/`.

They live in the **control** database. A sharded app posts money against the
control database, not against a shard.

## The money value

`autumn_web::money::Money<C>` is an amount in one currency. The currency is a
type parameter, so the compiler refuses to add dollars to euros:

```rust
use autumn_web::money::{Money, Usd};

let fee = Money::<Usd>::from_minor(250);   // $2.50
let tip = Money::<Usd>::from_major(1)?;    // $1.00
let total = fee.checked_add(tip)?;
assert_eq!(total.minor(), 350);
assert_eq!(total.to_string(), "3.50 USD");
# Ok::<(), autumn_web::money::MoneyError>(())
```

The amount is an `i64` count of **minor units** — cents for `USD`, yen for
`JPY`, fils for `BHD`. There is no `f64` in the value or in any operation on it.

There are no `+` or `-` operators, because an operator cannot report an
overflow. Use `checked_add`, `checked_sub`, `checked_neg`, `checked_abs`,
`checked_mul` and `try_sum`. Each returns `Result<_, MoneyError>`.

`autumn_web::money::CurrencyCode::known()` lists the currencies this build
knows. Each carries its ISO 4217 code, its minor-unit exponent and a symbol.

### When the currency is known only at run time

A ledger row stores its currency as text, so the same value has a
runtime-tagged form, `AnyMoney`. It rejects what `Money<C>` refuses to compile:

```rust
use autumn_web::money::{AnyMoney, CurrencyCode, MoneyError, Usd};

let dollars = AnyMoney::new(500, CurrencyCode::parse("USD")?);
let euros = AnyMoney::new(500, CurrencyCode::parse("EUR")?);
assert!(matches!(
    dollars.checked_add(euros),
    Err(MoneyError::CurrencyMismatch { .. })
));

let typed = dollars.try_into_typed::<Usd>()?;
assert_eq!(typed.minor(), 500);
# Ok::<(), MoneyError>(())
```

## Rounding and splitting

A conversion that can round always names the rule:

```rust
use autumn_web::money::{Money, Rounding, Usd};
use rust_decimal::Decimal;

let price: Decimal = "1.005".parse().unwrap();
assert_eq!(Money::<Usd>::from_decimal(price, Rounding::HalfUp)?.minor(), 101);
assert_eq!(Money::<Usd>::from_decimal(price, Rounding::HalfEven)?.minor(), 100);

// Or refuse to round.
assert!(Money::<Usd>::from_decimal_exact(price).is_err());
# Ok::<(), autumn_web::money::MoneyError>(())
```

`Rounding` covers `HalfUp` (the default), `HalfEven`, `HalfDown`, `TowardZero`,
`AwayFromZero`, `Floor` and `Ceiling`.

`allocate` splits an amount by weights and loses nothing — the parts always sum
back to the whole:

```rust
use autumn_web::money::{Money, Usd};

// A dollar in three.
let parts = Money::<Usd>::from_minor(100).split(3)?;
assert_eq!(parts.iter().map(|p| p.minor()).collect::<Vec<_>>(), vec![34, 33, 33]);

// A marketplace split: 70 / 20 / 10.
let shares = Money::<Usd>::from_minor(9999).allocate(&[70, 20, 10])?;
assert_eq!(Money::<Usd>::try_sum(shares)?.minor(), 9999);
# Ok::<(), autumn_web::money::MoneyError>(())
```

To render money for a page, hand `to_decimal()` to
[`number_to_currency`](format-helpers.md).

## The ledger

A transaction is a set of postings. A **debit is positive** and a **credit is
negative**, and an account's balance is the sum of its postings. Build postings
with `Posting::debit` and `Posting::credit`, and the sign is handled for you.

Open the accounts once — at boot, or the first time you touch one:

```rust,no_run
use autumn_web::money::ledger::{self, Account};
use autumn_web::money::Usd;
use autumn_web::prelude::*;

# async fn open(conn: &mut autumn_web::db::RuntimeConnection) -> AutumnResult<()> {
ledger::ensure_account(conn, Account::new("platform:cash", Usd::currency())).await?;
ledger::ensure_account(conn, Account::new("platform:revenue", Usd::currency())).await?;

// An account that must never go below zero.
ledger::ensure_account(
    conn,
    Account::new("platform:float", Usd::currency()).disallow_negative(),
)
.await?;
# Ok(())
# }
```

The account identifier is yours. Pick a shape you can rebuild from your own
rows, such as `"platform:cash"`.

Then post, inside the same `Db::tx` as the application rows the money
justifies:

```rust,no_run
use autumn_web::money::ledger::{self, IdempotencyKey, Posting, Transaction};
use autumn_web::money::{Money, Usd};
use autumn_web::prelude::*;
use scoped_futures::ScopedFutureExt as _;

# async fn charge(mut db: Db) -> AutumnResult<()> {
let amount = Money::<Usd>::from_major(25)?;
let postings = vec![
    Posting::debit("platform:cash", amount),
    Posting::credit("platform:revenue", amount),
];
let key = IdempotencyKey::derive("order:9911", &postings);
let transfer = Transaction::new(key, postings).memo("order 9911");

let outcome = db
    .tx(|conn| {
        async move {
            // ... insert or update the order row here ...
            ledger::post(conn, &transfer).await.map_err(AutumnError::from)
        }
        .scope_boxed()
    })
    .await?;

if outcome.is_replayed() {
    // The charge was already posted. Nothing was written now.
}
# Ok(())
# }
```

Because `post` takes the connection, the postings and your rows commit
together. If the handler fails after the post, both roll back and the
idempotency key is free again.

**`post` must run inside a transaction.** A call on a bare connection is
refused with `LedgerError::NotInTransaction`. The account locks and the balance
check only mean something inside one, and the tables are append-only — a
transaction row written without its postings could never be repaired.

On Postgres the account rows are held with `SELECT ... FOR UPDATE`, so two
concurrent posts against one account are serialized. SQLite has no row lock and
`Db::tx` opens a deferred transaction, so two concurrent posts there can leave
the second with "database is locked". A SQLite app that posts concurrently must
retry the transaction.

The locks are sorted within one call, not across a transaction. If one
transaction posts more than once over overlapping accounts, use `Db::tx_with`,
which retries a deadlock.

A transaction may carry more than two postings. A marketplace split is one
transaction:

```rust,no_run
# use autumn_web::money::ledger::Posting;
# use autumn_web::money::{Money, Usd};
# fn example() {
# let m = |c| Money::<Usd>::from_minor(c);
let postings = vec![
    Posting::debit("platform:cash", m(10_000)),
    Posting::credit("seller:1", m(7_000)),
    Posting::credit("seller:2", m(2_000)),
    Posting::credit("platform:fees", m(1_000)),
];
# let _ = postings;
# }
```

## Idempotency

Every transaction carries an idempotency key with a `UNIQUE` index behind it.
Submitting the same transaction twice writes one entry: the second call reads
the first call's transaction and returns `PostOutcome::Replayed`.

Two ways to get a key:

- `IdempotencyKey::new("ch_3Ox...")` — use an identifier you already have, such
  as a provider charge id.
- `IdempotencyKey::derive("order:9911", &postings)` — **idempotent by
  construction**. The key is a hash of the namespace and the money the postings
  move, so the same charge built again produces the same key with nothing for
  the caller to remember. The postings are sorted first, so build order does not
  matter.

The same key for **different** money is a `LedgerError::KeyReuse` (HTTP 409),
not a silent replay of the wrong result. The memo is not part of the money, so
a retry may word it differently and still replay.

## Balances

```rust,no_run
use autumn_web::money::ledger;
# async fn read(conn: &mut autumn_web::db::RuntimeConnection) -> Result<(), ledger::LedgerError> {
let cash = ledger::balance(conn, "platform:cash").await?;
println!("{cash}"); // 25.00 USD

// Every currency's total across the whole ledger. Each must be zero.
for total in ledger::trial_balance(conn).await? {
    assert!(total.total().is_zero());
}
# Ok(())
# }
```

`trial_balance` is the global invariant made queryable. A non-zero total is
proof that something wrote around `post` — run it from a scheduled job or a
health check.

## What the framework enforces

| Rule | Where it is enforced |
| --- | --- |
| Debits equal credits | `Transaction::validate`, before the first `INSERT` |
| One currency per transaction | `Transaction::validate` |
| A transaction has a debit and a credit | `Transaction::validate` |
| No posting is zero, and no amount is negative | `Transaction::validate` |
| A balance stays inside `i64` | `post`, before anything is written |
| A posting's currency matches its account | `post`, after the account row is read |
| The same key posts once | `UNIQUE (idempotency_key)`, with `ON CONFLICT DO NOTHING` |
| The same key for different money is refused | a stored request hash |
| No negative balance where forbidden | the balance the posting would leave, read before anything is written |
| Nothing is rewritten | database triggers on both backends, `TRUNCATE` and SQLite `REPLACE` included |
| An account never changes currency | a trigger on both backends; only `allow_negative` stays editable |
| A cancelled `post` never half-writes | the postings go in before their transaction row, behind a deferred foreign key |

The last two matter most.

`post` never updates or deletes a row, and a trigger aborts an `UPDATE`, a
`DELETE` or a `TRUNCATE` that comes from anywhere else.

An account's currency is fixed once the account exists. It is what `post`
checks a posting against, so a change would relabel every stored minor unit and
let the next posting in the new currency pass that check — two currencies in
one account. `set_allow_negative` still works; only the currency is frozen.

`INSERT OR REPLACE` is the same rewrite in disguise, and SQLite needs two
things to refuse it. SQLite settles the conflict by deleting the row that is in
the way, and it skips `DELETE` triggers for that deletion unless
`PRAGMA recursive_triggers` is on. Autumn's pool therefore sets that pragma on
every SQLite connection, which puts the append-only triggers back in the path.
The migration also refuses an insert that collides with a posting's row keys,
which holds even on a connection that does not set the pragma.

Both are needed. The deferred foreign key is **not** enough on its own: a
`REPLACE` that collides on the idempotency key can put the deleted row's id
back before `COMMIT`, which satisfies the foreign key and leaves the real
postings under forged transaction metadata. Only the `DELETE` trigger, reached
through the pragma, stops that.

Postgres needs neither. It has no `REPLACE`, and it answers the same statement
with `ON CONFLICT DO UPDATE`, which is an `UPDATE` the trigger above sees.

And `post` writes the postings **before** the transaction row they belong to.
That looks backwards; it is what keeps a cancelled call from half-writing the
books. The foreign key is deferred, so the database checks it at `COMMIT` — and
a `post` whose future is dropped between the two leaves postings with no parent,
which refuses the commit. The alternative order could commit a transaction row
with no postings, and append-only tables could never repair that.

The ledger therefore ends up with either nothing or one complete balanced
transaction. It does **not** tell you which: a drop after the last write, while
the read-back is in flight, leaves a complete transaction that commits normally.
If you race `post` against a timeout, treat a cancellation as indeterminate and
settle it by posting the same idempotency key again — that replays if the money
moved and posts if it did not. Not racing it is simpler.

## Not in this slice

- Reconciliation against an external provider.
- FX conversion between currencies. `Money<C>` refuses cross-currency
  arithmetic; it does not convert.
- A payment-provider client. This is the primitive underneath one. For
  subscription state mirrored from a provider, see [billing](billing.md).
- Credit limits beyond the one `disallow_negative` flag per account.
- Statements, invoices and accounting exports.
