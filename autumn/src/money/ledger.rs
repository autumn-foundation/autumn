// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]
//! An append-only, double-entry money ledger (issue #1837).
//!
//! Not to be confused with [`crate::ledger`], which records the history of a
//! `#[repository]` row. This ledger records **money**.
//!
//! # What it guarantees
//!
//! * **Every transaction balances.** [`post`] adds the postings and refuses the
//!   transaction when the total is not zero. The check runs before the first
//!   `INSERT`, so an unbalanced transaction never reaches the database.
//! * **Posting twice posts once.** Each transaction carries an idempotency key
//!   with a `UNIQUE` index behind it. The second call reads the first call's
//!   transaction and reports [`PostOutcome::Replayed`].
//! * **Nothing is rewritten.** A trigger on both backends aborts any `UPDATE`
//!   or `DELETE` of a transaction or a posting.
//! * **It commits with your rows.** [`post`] takes a connection, so it runs
//!   inside [`Db::tx`](crate::db::Db::tx) with the application rows the
//!   transaction justifies.
//!
//! # Signs
//!
//! A posting holds a signed amount. A **debit is positive** and a **credit is
//! negative**, and a balance is the sum of an account's postings. An account
//! that holds money for somebody else (a customer wallet) therefore runs
//! negative as it fills, which is what keeps the books in balance. Build
//! postings with [`Posting::debit`] and [`Posting::credit`] and the sign is
//! handled for you.
//!
//! # Example
//!
//! ```rust,no_run
//! use autumn_web::money::ledger::{self, Account, IdempotencyKey, Posting, Transaction};
//! use autumn_web::money::{Money, Usd};
//! use autumn_web::prelude::*;
//! use scoped_futures::ScopedFutureExt as _;
//!
//! # async fn example(mut db: Db) -> AutumnResult<()> {
//! let charge = Money::<Usd>::from_major(25)?;
//! let postings = vec![
//!     Posting::debit("customer:42:wallet", charge),
//!     Posting::credit("platform:revenue", charge),
//! ];
//! // The key comes from the postings, so a retry of the same charge collapses.
//! let key = IdempotencyKey::derive("order:9911", &postings);
//! let transfer = Transaction::new(key, postings).memo("order 9911");
//!
//! let outcome = db
//!     .tx(|conn| {
//!         async move {
//!             ledger::ensure_account(conn, Account::new("customer:42:wallet", Usd::currency())).await?;
//!             ledger::ensure_account(conn, Account::new("platform:revenue", Usd::currency())).await?;
//!             // ... the application rows this charge justifies go here ...
//!             ledger::post(conn, &transfer).await
//!         }
//!         .scope_boxed()
//!     })
//!     .await?;
//!
//! assert!(outcome.is_posted());
//! # Ok(())
//! # }
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fmt::Write as _;

use diesel::sql_types::{BigInt, Bool, Text};
use diesel_async::RunQueryDsl as _;
use sha2::Digest as _;

use crate::db::RuntimeConnection;
use crate::money::{AnyMoney, CurrencyCode, MoneyError};

/// The account table. A framework table, hence the `_autumn_` prefix.
pub const ACCOUNTS_TABLE: &str = "_autumn_ledger_accounts";
/// The transaction table.
pub const TRANSACTIONS_TABLE: &str = "_autumn_ledger_transactions";
/// The posting table.
pub const POSTINGS_TABLE: &str = "_autumn_ledger_postings";

/// The longest account identifier the ledger accepts.
const MAX_ACCOUNT_ID: usize = 255;
/// The longest idempotency key the ledger accepts.
const MAX_IDEMPOTENCY_KEY: usize = 255;
/// The longest memo the ledger stores.
const MAX_MEMO: usize = 1024;
/// The most postings one transaction can carry.
const MAX_POSTINGS: usize = 1024;

/// `CURRENT_TIMESTAMP` rather than `NOW()`: both backends spell it that way.
const NOW: &str = "CURRENT_TIMESTAMP";

// Backend-forked placeholder. Postgres numbers its binds, `SQLite` does not.
#[cfg(not(feature = "sqlite"))]
fn ph(n: usize) -> String {
    format!("${n}")
}
#[cfg(feature = "sqlite")]
fn ph(_n: usize) -> String {
    "?".to_owned()
}

// Row lock on one account row. On Postgres `FOR UPDATE` holds the row until the
// enclosing transaction ends. On `SQLite` the writer lock the transaction
// already holds excludes every other writer, so the clause degrades to nothing.
#[cfg(not(feature = "sqlite"))]
const FOR_UPDATE: &str = " FOR UPDATE";
#[cfg(feature = "sqlite")]
const FOR_UPDATE: &str = "";

// ── Errors ──────────────────────────────────────────────────────────────────

/// What can go wrong when money is posted.
#[derive(Debug)]
#[non_exhaustive]
pub enum LedgerError {
    /// A money value could not be built or combined.
    Money(MoneyError),
    /// The debits do not equal the credits.
    Unbalanced {
        /// The sum of the debits, in minor units.
        debits: i64,
        /// The sum of the credits, in minor units, as a positive number.
        credits: i64,
        /// The currency of the transaction.
        currency: &'static str,
    },
    /// The transaction carries no postings, or more than the ledger accepts.
    PostingCount {
        /// How many postings were supplied.
        count: usize,
    },
    /// The transaction has no debit, or no credit. A transfer needs both.
    OneSided,
    /// Every posting is zero, so the transaction moves no money.
    ZeroValue,
    /// The postings are not all in one currency. This slice does not convert.
    MixedCurrencies {
        /// The currency of the first posting.
        expected: &'static str,
        /// The currency that did not match.
        found: &'static str,
    },
    /// A posting names an account that does not exist.
    UnknownAccount {
        /// The account identifier.
        id: String,
    },
    /// A posting's currency is not the account's currency.
    AccountCurrency {
        /// The account identifier.
        account: String,
        /// The account's currency.
        expected: &'static str,
        /// The posting's currency.
        found: &'static str,
    },
    /// The idempotency key was used before, for different money.
    KeyReuse {
        /// The key that was reused.
        key: String,
    },
    /// A posting carries a negative amount. The sign belongs to the side.
    NegativeAmount {
        /// The account the posting names.
        account: String,
    },
    /// An identifier, key or memo is empty or too long.
    InvalidText {
        /// Which field is wrong.
        field: &'static str,
        /// Why it is wrong.
        reason: &'static str,
    },
    /// The posting would take an account below zero, and the account refuses
    /// that.
    NegativeBalance {
        /// The account identifier.
        account: String,
        /// The balance the posting would leave, in minor units.
        balance: i64,
        /// The account's currency.
        currency: &'static str,
    },
    /// The database refused the statement.
    Database(diesel::result::Error),
}

impl LedgerError {
    /// The HTTP status a handler returns for this error.
    ///
    /// `AutumnError`'s blanket `From<E: Error>` impl forecloses a dedicated
    /// `From`, so `error.rs` reads this through a downcast, the same way
    /// `ConstelaError` and `PushError` are mapped.
    #[must_use]
    pub fn http_status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Self::Money(err) => err.http_status(),
            Self::KeyReuse { .. } | Self::NegativeBalance { .. } => StatusCode::CONFLICT,
            Self::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Money(err) => write!(f, "{err}"),
            Self::Unbalanced {
                debits,
                credits,
                currency,
            } => write!(
                f,
                "ledger transaction does not balance: {debits} debit \
                 against {credits} credit ({currency} minor units)"
            ),
            Self::PostingCount { count } => write!(
                f,
                "a ledger transaction takes 2 to {MAX_POSTINGS} postings, not {count}"
            ),
            Self::OneSided => f.write_str("a ledger transaction needs a debit and a credit"),
            Self::ZeroValue => f.write_str("a ledger transaction must move money"),
            Self::MixedCurrencies { expected, found } => write!(
                f,
                "a ledger transaction is in one currency: expected {expected}, found {found}"
            ),
            Self::UnknownAccount { id } => write!(f, "no ledger account {id}"),
            Self::NegativeAmount { account } => write!(
                f,
                "the posting to {account} has a negative amount; use the credit side instead"
            ),
            Self::AccountCurrency {
                account,
                expected,
                found,
            } => write!(
                f,
                "account {account} holds {expected}, but the posting is {found}"
            ),
            Self::KeyReuse { key } => write!(
                f,
                "idempotency key {key} was used before for different postings"
            ),
            Self::InvalidText { field, reason } => write!(f, "{field} {reason}"),
            Self::NegativeBalance {
                account,
                balance,
                currency,
            } => write!(
                f,
                "account {account} refuses a negative balance, and this posting \
                 leaves {balance} {currency} minor units"
            ),
            Self::Database(err) => write!(f, "ledger database error: {err}"),
        }
    }
}

impl std::error::Error for LedgerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Money(err) => Some(err),
            Self::Database(err) => Some(err),
            _ => None,
        }
    }
}

impl From<MoneyError> for LedgerError {
    fn from(err: MoneyError) -> Self {
        Self::Money(err)
    }
}

impl From<diesel::result::Error> for LedgerError {
    fn from(err: diesel::result::Error) -> Self {
        Self::Database(err)
    }
}

// ── Accounts ────────────────────────────────────────────────────────────────

/// One account in the ledger.
///
/// The identifier is yours: `"customer:42:wallet"`, `"platform:revenue"`. It is
/// the primary key, so pick a shape you can rebuild from your own rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    id: String,
    currency: CurrencyCode,
    allow_negative: bool,
}

impl Account {
    /// An account in `currency` that may go negative.
    ///
    /// Most accounts should: a customer wallet that holds money is negative by
    /// the sign rules above, and a revenue account only ever goes one way.
    #[must_use]
    pub fn new(id: impl Into<String>, currency: CurrencyCode) -> Self {
        Self {
            id: id.into(),
            currency,
            allow_negative: true,
        }
    }

    /// Refuse any posting that would leave this account below zero.
    ///
    /// The check reads the balance under a row lock taken by [`post`], so two
    /// concurrent postings cannot both pass it.
    #[must_use]
    pub const fn disallow_negative(mut self) -> Self {
        self.allow_negative = false;
        self
    }

    /// The account identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The currency this account holds.
    #[must_use]
    pub const fn currency(&self) -> CurrencyCode {
        self.currency
    }

    /// Whether this account may hold a negative balance.
    #[must_use]
    pub const fn allows_negative(&self) -> bool {
        self.allow_negative
    }
}

// ── Postings and transactions ───────────────────────────────────────────────

/// Which way a posting moves money.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    /// Money into the account. Adds to the balance.
    Debit,
    /// Money out of the account. Takes from the balance.
    Credit,
}

impl Side {
    /// The side of a stored, signed amount. Zero counts as a debit.
    const fn of(signed_minor: i64) -> Self {
        if signed_minor < 0 { Self::Credit } else { Self::Debit }
    }
}

/// One line of a transaction: an account, an amount, and a side.
///
/// The amount is held as supplied and the side is held beside it, so a negative
/// amount is refused by [`Transaction::validate`] rather than silently flipped.
/// That also keeps `i64::MIN` out of the ledger, which has no positive form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    account_id: String,
    amount: AnyMoney,
    side: Side,
}

impl Posting {
    /// Money into `account_id`.
    #[must_use]
    pub fn debit(account_id: impl Into<String>, amount: impl Into<AnyMoney>) -> Self {
        Self {
            account_id: account_id.into(),
            amount: amount.into(),
            side: Side::Debit,
        }
    }

    /// Money out of `account_id`.
    #[must_use]
    pub fn credit(account_id: impl Into<String>, amount: impl Into<AnyMoney>) -> Self {
        Self {
            account_id: account_id.into(),
            amount: amount.into(),
            side: Side::Credit,
        }
    }

    /// The account this line touches.
    #[must_use]
    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// The amount, without a sign.
    #[must_use]
    pub const fn amount(&self) -> AnyMoney {
        self.amount
    }

    /// Which way the money moves.
    #[must_use]
    pub const fn side(&self) -> Side {
        self.side
    }

    /// True when this line adds to the account.
    #[must_use]
    pub fn is_debit(&self) -> bool {
        self.side == Side::Debit
    }

    /// True when this line takes from the account.
    #[must_use]
    pub fn is_credit(&self) -> bool {
        self.side == Side::Credit
    }

    /// The amount as the ledger stores it: positive for a debit, negative for a
    /// credit.
    ///
    /// # Errors
    ///
    /// [`LedgerError::NegativeAmount`] when the amount is below zero. A sign
    /// belongs to the side, not to the amount.
    pub fn signed_minor(&self) -> Result<i64, LedgerError> {
        let minor = self.amount.minor();
        if minor < 0 {
            return Err(LedgerError::NegativeAmount {
                account: self.account_id.clone(),
            });
        }
        match self.side {
            Side::Debit => Ok(minor),
            // Safe: `minor` is at or above zero, so its negation exists.
            Side::Credit => minor.checked_neg().ok_or(LedgerError::Money(MoneyError::Overflow)),
        }
    }
}

/// A key that makes a posting idempotent.
///
/// Supply your own with [`IdempotencyKey::new`] when you already have a stable
/// identifier for the money movement — a provider charge id, an order number.
/// Otherwise let [`IdempotencyKey::derive`] build one from the postings
/// themselves, which is what "idempotent by construction" means here: the same
/// money, submitted again, produces the same key and collapses.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Use a key you already have.
    ///
    /// # Errors
    ///
    /// [`LedgerError::InvalidText`] when the key is empty or longer than 255
    /// bytes.
    pub fn new(key: impl Into<String>) -> Result<Self, LedgerError> {
        let key = key.into();
        check_text("idempotency key", &key, MAX_IDEMPOTENCY_KEY)?;
        Ok(Self(key))
    }

    /// Build a key from `namespace` and the money `postings` move.
    ///
    /// Lowercase hex SHA-256 over the namespace and the normalized postings.
    /// The postings are sorted first, so the same transfer built in a different
    /// order produces the same key. The memo is left out on purpose: it
    /// describes the movement, it is not part of it.
    ///
    /// `namespace` separates two callers that happen to move identical money —
    /// use the business identity of the movement, such as an order id.
    #[must_use]
    pub fn derive(namespace: &str, postings: &[Posting]) -> Self {
        let mut hasher = sha2::Sha256::new();
        push_field(&mut hasher, "namespace", namespace.as_bytes());
        push_normalized_postings(&mut hasher, postings);
        Self(hex_lower(hasher.finalize()))
    }

    /// The key as stored.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A set of postings submitted as one transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    key: IdempotencyKey,
    postings: Vec<Posting>,
    memo: String,
}

impl Transaction {
    /// A transaction with `key` over `postings`.
    #[must_use]
    pub fn new(key: IdempotencyKey, postings: Vec<Posting>) -> Self {
        Self {
            key,
            postings,
            memo: String::new(),
        }
    }

    /// Attach a description. The memo is not part of the money movement, so it
    /// does not change the request hash and a retry may word it differently.
    #[must_use]
    pub fn memo(mut self, memo: impl Into<String>) -> Self {
        self.memo = memo.into();
        self
    }

    /// The idempotency key.
    #[must_use]
    pub const fn key(&self) -> &IdempotencyKey {
        &self.key
    }

    /// The postings, in the order they were supplied.
    #[must_use]
    pub fn postings(&self) -> &[Posting] {
        &self.postings
    }

    /// The description.
    #[must_use]
    pub fn memo_text(&self) -> &str {
        &self.memo
    }

    /// The currency every posting is in.
    ///
    /// # Errors
    ///
    /// See [`Transaction::validate`].
    pub fn currency(&self) -> Result<CurrencyCode, LedgerError> {
        self.validate().map(|checked| checked.currency)
    }

    /// Check the double-entry rules, and report the first that fails.
    ///
    /// [`post`] calls this before it writes anything, so an unbalanced
    /// transaction never reaches the database. Call it yourself to reject a
    /// request earlier.
    ///
    /// # Errors
    ///
    /// * [`LedgerError::PostingCount`] — fewer than two postings, or more than
    ///   1024.
    /// * [`LedgerError::MixedCurrencies`] — the postings are not all in one
    ///   currency.
    /// * [`LedgerError::OneSided`] — no debit, or no credit.
    /// * [`LedgerError::ZeroValue`] — every posting is zero.
    /// * [`LedgerError::Unbalanced`] — the debits do not equal the credits.
    /// * [`LedgerError::InvalidText`] — an empty or over-long account id, key
    ///   or memo.
    /// * [`LedgerError::Money`] — the postings sum past `i64`.
    pub fn validate(&self) -> Result<Validated, LedgerError> {
        check_text("idempotency key", self.key.as_str(), MAX_IDEMPOTENCY_KEY)?;
        if !self.memo.is_empty() && self.memo.len() > MAX_MEMO {
            return Err(LedgerError::InvalidText {
                field: "memo",
                reason: "is longer than 1024 bytes",
            });
        }
        if self.postings.len() < 2 || self.postings.len() > MAX_POSTINGS {
            return Err(LedgerError::PostingCount {
                count: self.postings.len(),
            });
        }

        let mut currency: Option<CurrencyCode> = None;
        let mut debits: i64 = 0;
        let mut credits: i64 = 0;
        for posting in &self.postings {
            check_text("account id", &posting.account_id, MAX_ACCOUNT_ID)?;
            let posting_currency = posting.amount.currency();
            match currency {
                None => currency = Some(posting_currency),
                Some(expected) if expected != posting_currency => {
                    return Err(LedgerError::MixedCurrencies {
                        expected: expected.code(),
                        found: posting_currency.code(),
                    });
                }
                Some(_) => {}
            }
            let minor = posting.signed_minor()?;
            if minor > 0 {
                debits = debits.checked_add(minor).ok_or(MoneyError::Overflow)?;
            } else {
                credits = credits.checked_sub(minor).ok_or(MoneyError::Overflow)?;
            }
        }

        let Some(currency) = currency else {
            return Err(LedgerError::PostingCount { count: 0 });
        };
        if debits == 0 && credits == 0 {
            return Err(LedgerError::ZeroValue);
        }
        if debits == 0 || credits == 0 {
            return Err(LedgerError::OneSided);
        }
        if debits != credits {
            return Err(LedgerError::Unbalanced {
                debits,
                credits,
                currency: currency.code(),
            });
        }

        Ok(Validated {
            currency,
            total: debits,
            request_hash: self.request_hash(currency),
        })
    }

    /// A content address for the money this transaction moves.
    ///
    /// Covers the currency and the normalized postings, and nothing else. Two
    /// submissions of the same money hash the same however they were built and
    /// however they are worded.
    fn request_hash(&self, currency: CurrencyCode) -> String {
        let mut hasher = sha2::Sha256::new();
        push_field(&mut hasher, "currency", currency.code().as_bytes());
        push_normalized_postings(&mut hasher, &self.postings);
        hex_lower(hasher.finalize())
    }
}

/// A transaction that passed [`Transaction::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validated {
    currency: CurrencyCode,
    total: i64,
    request_hash: String,
}

impl Validated {
    /// The currency every posting is in.
    #[must_use]
    pub const fn currency(&self) -> CurrencyCode {
        self.currency
    }

    /// The money the transaction moves: the sum of the debits, which equals the
    /// sum of the credits.
    #[must_use]
    pub const fn total(&self) -> AnyMoney {
        AnyMoney::new(self.total, self.currency)
    }

    /// The content address of the money this transaction moves.
    #[must_use]
    pub fn request_hash(&self) -> &str {
        &self.request_hash
    }
}

// ── Stored transactions ─────────────────────────────────────────────────────

/// A transaction as the ledger stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedTransaction {
    id: String,
    key: IdempotencyKey,
    currency: CurrencyCode,
    memo: String,
    postings: Vec<Posting>,
    posted_at: String,
}

impl PostedTransaction {
    /// The ledger's identifier for this transaction.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The idempotency key it was posted under.
    #[must_use]
    pub const fn key(&self) -> &IdempotencyKey {
        &self.key
    }

    /// The currency of every posting.
    #[must_use]
    pub const fn currency(&self) -> CurrencyCode {
        self.currency
    }

    /// The description, empty when none was given.
    #[must_use]
    pub fn memo(&self) -> &str {
        &self.memo
    }

    /// The stored postings, in the order they were written.
    #[must_use]
    pub fn postings(&self) -> &[Posting] {
        &self.postings
    }

    /// When the transaction was posted, as the database rendered it.
    ///
    /// Text rather than a timestamp: the column is `TIMESTAMPTZ` on Postgres
    /// and `TEXT` on `SQLite`, and this is a display field.
    #[must_use]
    pub fn posted_at(&self) -> &str {
        &self.posted_at
    }

    /// The money the transaction moved: the sum of its debits.
    #[must_use]
    pub fn total(&self) -> AnyMoney {
        let total = self
            .postings
            .iter()
            .filter(|posting| posting.is_debit())
            .fold(0_i64, |sum, posting| sum.saturating_add(posting.amount.minor()));
        AnyMoney::new(total, self.currency)
    }
}

/// What [`post`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutcome {
    /// The transaction was written now.
    Posted(PostedTransaction),
    /// The idempotency key was already used for this money. This is the
    /// transaction the first call wrote; nothing was written now.
    Replayed(PostedTransaction),
}

impl PostOutcome {
    /// The transaction, whether it was written now or before.
    #[must_use]
    pub const fn transaction(&self) -> &PostedTransaction {
        match self {
            Self::Posted(tx) | Self::Replayed(tx) => tx,
        }
    }

    /// True when this call wrote the transaction.
    #[must_use]
    pub const fn is_posted(&self) -> bool {
        matches!(self, Self::Posted(_))
    }

    /// True when this call found the transaction already written.
    #[must_use]
    pub const fn is_replayed(&self) -> bool {
        matches!(self, Self::Replayed(_))
    }
}

/// One currency's total across the whole ledger. Every total must be zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrencyTotal {
    /// The currency.
    pub currency: CurrencyCode,
    /// The sum of every posting in that currency.
    pub total: AnyMoney,
}

// ── Hashing helpers ─────────────────────────────────────────────────────────

/// Length-prefix one labelled field into the hash.
///
/// The length prefix stops two different field splits from hashing the same.
fn push_field(hasher: &mut sha2::Sha256, label: &str, value: &[u8]) {
    hasher.update(label.as_bytes());
    hasher.update(b":");
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(b":");
    hasher.update(value);
    hasher.update(b";");
}

/// Hash the postings in a canonical order, so build order does not matter.
///
/// A posting whose amount is negative cannot be hashed as a signed value, so it
/// keeps its own amount and side. `validate` refuses it either way; this only
/// has to stay injective.
fn push_normalized_postings(hasher: &mut sha2::Sha256, postings: &[Posting]) {
    let mut lines: Vec<(String, i64, &'static str)> = postings
        .iter()
        .map(|posting| {
            (
                posting.account_id.clone(),
                posting.signed_minor().unwrap_or(posting.amount.minor()),
                posting.amount.currency().code(),
            )
        })
        .collect();
    lines.sort_unstable();
    push_field(hasher, "count", lines.len().to_string().as_bytes());
    for (account, minor, currency) in lines {
        push_field(hasher, "account", account.as_bytes());
        push_field(hasher, "minor", minor.to_string().as_bytes());
        push_field(hasher, "currency", currency.as_bytes());
    }
}

fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().fold(
        String::with_capacity(bytes.as_ref().len().saturating_mul(2)),
        |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        },
    )
}

fn check_text(field: &'static str, value: &str, max: usize) -> Result<(), LedgerError> {
    if value.is_empty() {
        return Err(LedgerError::InvalidText {
            field,
            reason: "is empty",
        });
    }
    if value.len() > max {
        return Err(LedgerError::InvalidText {
            field,
            reason: "is too long",
        });
    }
    Ok(())
}

// ── Rows ────────────────────────────────────────────────────────────────────

#[derive(diesel::QueryableByName)]
struct AccountRow {
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = Text)]
    currency: String,
    #[diesel(sql_type = Bool)]
    allow_negative: bool,
}

#[derive(diesel::QueryableByName)]
struct TransactionRow {
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = Text)]
    idempotency_key: String,
    #[diesel(sql_type = Text)]
    request_hash: String,
    #[diesel(sql_type = Text)]
    currency: String,
    #[diesel(sql_type = Text)]
    memo: String,
    #[diesel(sql_type = Text)]
    posted_at: String,
}

#[derive(diesel::QueryableByName)]
struct PostingRow {
    #[diesel(sql_type = Text)]
    account_id: String,
    #[diesel(sql_type = BigInt)]
    amount_minor: i64,
    #[diesel(sql_type = Text)]
    currency: String,
}

#[derive(diesel::QueryableByName)]
struct TotalRow {
    #[diesel(sql_type = BigInt)]
    total: i64,
}

#[derive(diesel::QueryableByName)]
struct CurrencyTotalRow {
    #[diesel(sql_type = Text)]
    currency: String,
    #[diesel(sql_type = BigInt)]
    total: i64,
}

fn parse_currency(code: &str) -> Result<CurrencyCode, LedgerError> {
    CurrencyCode::parse(code).map_err(LedgerError::Money)
}

impl AccountRow {
    fn into_account(self) -> Result<Account, LedgerError> {
        Ok(Account {
            currency: parse_currency(&self.currency)?,
            id: self.id,
            allow_negative: self.allow_negative,
        })
    }
}

impl PostingRow {
    /// `post` never writes `i64::MIN`, so `saturating_abs` cannot change a
    /// stored amount.
    fn into_posting(self) -> Result<Posting, LedgerError> {
        Ok(Posting {
            amount: AnyMoney::new(
                self.amount_minor.saturating_abs(),
                parse_currency(&self.currency)?,
            ),
            side: Side::of(self.amount_minor),
            account_id: self.account_id,
        })
    }
}

// ── Store ───────────────────────────────────────────────────────────────────

/// Create `account` when it does not exist, and return the stored account.
///
/// Never changes an account that is already there, so calling it on every boot
/// is safe. The returned [`Account`] is the **stored** one: when an existing
/// account's negative-balance policy differs from the one supplied, the stored
/// policy is what applies. Use [`set_allow_negative`] to change it on purpose.
///
/// # Errors
///
/// [`LedgerError::AccountCurrency`] when an account with this id already holds
/// another currency, [`LedgerError::InvalidText`] for an empty or over-long id,
/// and [`LedgerError::Database`] when the statement fails.
pub async fn ensure_account(
    conn: &mut RuntimeConnection,
    account: Account,
) -> Result<Account, LedgerError> {
    check_text("account id", &account.id, MAX_ACCOUNT_ID)?;

    let sql = format!(
        "INSERT INTO {ACCOUNTS_TABLE} (id, currency, allow_negative, created_at) \
         VALUES ({}, {}, {}, {NOW}) ON CONFLICT (id) DO NOTHING",
        ph(1),
        ph(2),
        ph(3)
    );
    diesel::sql_query(sql)
        .bind::<Text, _>(account.id.clone())
        .bind::<Text, _>(account.currency.code())
        .bind::<Bool, _>(account.allow_negative)
        .execute(conn)
        .await?;

    let stored = load_account(conn, &account.id)
        .await?
        .ok_or_else(|| LedgerError::UnknownAccount {
            id: account.id.clone(),
        })?;
    if stored.currency != account.currency {
        return Err(LedgerError::AccountCurrency {
            account: account.id,
            expected: stored.currency.code(),
            found: account.currency.code(),
        });
    }
    Ok(stored)
}

/// Change whether `account_id` may hold a negative balance.
///
/// Only the policy changes. The postings are append-only and are never touched,
/// so an account that is already negative stays negative; the new policy
/// applies to the next posting.
///
/// # Errors
///
/// [`LedgerError::UnknownAccount`] when there is no such account, and
/// [`LedgerError::Database`] when the statement fails.
pub async fn set_allow_negative(
    conn: &mut RuntimeConnection,
    account_id: &str,
    allow_negative: bool,
) -> Result<Account, LedgerError> {
    let sql = format!(
        "UPDATE {ACCOUNTS_TABLE} SET allow_negative = {} WHERE id = {}",
        ph(1),
        ph(2)
    );
    let updated = diesel::sql_query(sql)
        .bind::<Bool, _>(allow_negative)
        .bind::<Text, _>(account_id)
        .execute(conn)
        .await?;
    if updated == 0 {
        return Err(LedgerError::UnknownAccount {
            id: account_id.to_owned(),
        });
    }
    load_account(conn, account_id)
        .await?
        .ok_or_else(|| LedgerError::UnknownAccount {
            id: account_id.to_owned(),
        })
}

/// Read one account.
///
/// # Errors
///
/// [`LedgerError::Database`] when the statement fails, and
/// [`LedgerError::Money`] when a stored currency code is not one this build
/// knows.
pub async fn account(
    conn: &mut RuntimeConnection,
    account_id: &str,
) -> Result<Option<Account>, LedgerError> {
    load_account(conn, account_id).await
}

async fn load_account(
    conn: &mut RuntimeConnection,
    account_id: &str,
) -> Result<Option<Account>, LedgerError> {
    let sql = format!(
        "SELECT id, currency, allow_negative FROM {ACCOUNTS_TABLE} WHERE id = {}",
        ph(1)
    );
    let rows: Vec<AccountRow> = diesel::sql_query(sql)
        .bind::<Text, _>(account_id)
        .load(conn)
        .await?;
    rows.into_iter().next().map(AccountRow::into_account).transpose()
}

/// Read one account and hold its row until the enclosing transaction ends.
///
/// The lock is what makes the negative-balance check exact: two concurrent
/// postings against one account are serialized behind it, so neither can read a
/// balance the other is about to change.
async fn lock_account(
    conn: &mut RuntimeConnection,
    account_id: &str,
) -> Result<Option<Account>, LedgerError> {
    let sql = format!(
        "SELECT id, currency, allow_negative FROM {ACCOUNTS_TABLE} WHERE id = {}{FOR_UPDATE}",
        ph(1)
    );
    let rows: Vec<AccountRow> = diesel::sql_query(sql)
        .bind::<Text, _>(account_id)
        .load(conn)
        .await?;
    rows.into_iter().next().map(AccountRow::into_account).transpose()
}

/// Post `transfer` to the ledger, exactly once.
///
/// Run this inside [`Db::tx`](crate::db::Db::tx), with the application rows the
/// money justifies. Everything the call writes then commits or rolls back with
/// them, and the row locks it takes are held for the whole transaction.
///
/// The order of work is what makes the guarantees hold:
///
/// 1. [`Transaction::validate`] checks the double-entry rules.
/// 2. Each account named by a posting is read under a row lock, in sorted order
///    so two concurrent postings over the same accounts cannot deadlock. The
///    lock is held for the rest of the call.
/// 3. The idempotency key is read. When it is taken, the money was already
///    posted: that transaction is returned as [`PostOutcome::Replayed`].
/// 4. Any account that refuses a negative balance has the balance this
///    transaction *would* leave checked against the locked rows.
/// 5. Only now is anything written: the transaction row with
///    `ON CONFLICT DO NOTHING` on the key, then the postings.
///
/// Every refusal therefore happens before the first `INSERT`, so a rejected
/// post leaves no row behind even when it is called outside a transaction.
///
/// # Errors
///
/// Every variant of [`LedgerError`]. The ones worth handling by name are
/// [`LedgerError::Unbalanced`] (a bug in the caller), [`LedgerError::KeyReuse`]
/// (the same key for different money) and [`LedgerError::NegativeBalance`].
pub async fn post(
    conn: &mut RuntimeConnection,
    transfer: &Transaction,
) -> Result<PostOutcome, LedgerError> {
    let checked = transfer.validate()?;

    // Sorted and deduplicated, so every caller takes the locks in one order.
    let mut wanted: BTreeSet<&str> = BTreeSet::new();
    for posting in transfer.postings() {
        wanted.insert(posting.account_id());
    }
    let mut accounts: BTreeMap<String, Account> = BTreeMap::new();
    for id in &wanted {
        let account = lock_account(conn, id)
            .await?
            .ok_or_else(|| LedgerError::UnknownAccount { id: (*id).to_owned() })?;
        if account.currency != checked.currency {
            return Err(LedgerError::AccountCurrency {
                account: account.id,
                expected: account.currency.code(),
                found: checked.currency.code(),
            });
        }
        accounts.insert((*id).to_owned(), account);
    }

    // Already posted? Then the money moved once and this call is the retry.
    // Read before writing anything, so the checks below cannot refuse a
    // transaction that is already in the books.
    if let Some(existing) = load_transaction(conn, transfer.key().as_str()).await? {
        if existing.request_hash != *checked.request_hash() {
            return Err(LedgerError::KeyReuse {
                key: transfer.key().as_str().to_owned(),
            });
        }
        return Ok(PostOutcome::Replayed(existing.transaction));
    }

    check_negative_balances(conn, &accounts, transfer).await?;

    // Two attempts, not a loop without a bound. The account locks serialize
    // two posts over the same accounts, so the read above is normally the
    // whole answer; `ON CONFLICT DO NOTHING` covers a poster that shares the
    // key but not the accounts. It waits for the conflicting transaction to
    // finish before it skips, so a conflict whose row is then missing means
    // that transaction rolled back and the key is free again. One retry
    // settles it.
    for attempt in 0..2 {
        let id = uuid::Uuid::new_v4().to_string();
        let insert_sql = format!(
            "INSERT INTO {TRANSACTIONS_TABLE} \
               (id, idempotency_key, request_hash, currency, memo, posted_at) \
             VALUES ({}, {}, {}, {}, {}, {NOW}) \
             ON CONFLICT (idempotency_key) DO NOTHING",
            ph(1),
            ph(2),
            ph(3),
            ph(4),
            ph(5)
        );
        let inserted = diesel::sql_query(insert_sql)
            .bind::<Text, _>(id.clone())
            .bind::<Text, _>(transfer.key().as_str())
            .bind::<Text, _>(checked.request_hash().to_owned())
            .bind::<Text, _>(checked.currency.code())
            .bind::<Text, _>(transfer.memo_text())
            .execute(conn)
            .await?;

        if inserted == 0 {
            match load_transaction(conn, transfer.key().as_str()).await? {
                Some(existing) => {
                    if existing.request_hash != *checked.request_hash() {
                        return Err(LedgerError::KeyReuse {
                            key: transfer.key().as_str().to_owned(),
                        });
                    }
                    return Ok(PostOutcome::Replayed(existing.transaction));
                }
                // The other caller rolled back. Try the insert once more.
                None if attempt == 0 => continue,
                None => return Err(LedgerError::Database(diesel::result::Error::NotFound)),
            }
        }

        write_postings(conn, &id, transfer).await?;

        // Read the row back, so the caller gets what the ledger stored: the
        // database's own timestamp and the postings as written.
        let stored = load_transaction(conn, transfer.key().as_str())
            .await?
            .ok_or(LedgerError::Database(diesel::result::Error::NotFound))?;
        return Ok(PostOutcome::Posted(stored.transaction));
    }

    Err(LedgerError::Database(diesel::result::Error::NotFound))
}

/// Write one transaction's postings, in the order they were supplied.
async fn write_postings(
    conn: &mut RuntimeConnection,
    transaction_id: &str,
    transfer: &Transaction,
) -> Result<(), LedgerError> {
    let sql = format!(
        "INSERT INTO {POSTINGS_TABLE} \
           (transaction_id, seq, account_id, amount_minor, currency, posted_at) \
         VALUES ({}, {}, {}, {}, {}, {NOW})",
        ph(1),
        ph(2),
        ph(3),
        ph(4),
        ph(5)
    );
    for (index, posting) in transfer.postings().iter().enumerate() {
        let seq = i64::try_from(index).map_err(|_| LedgerError::PostingCount {
            count: transfer.postings().len(),
        })?;
        diesel::sql_query(sql.clone())
            .bind::<Text, _>(transaction_id)
            .bind::<BigInt, _>(seq)
            .bind::<Text, _>(posting.account_id())
            .bind::<BigInt, _>(posting.signed_minor()?)
            .bind::<Text, _>(posting.amount().currency().code())
            .execute(conn)
            .await?;
    }
    Ok(())
}

/// Reject a transaction that takes an account below zero, when the account says
/// so.
///
/// Reads the balance the transaction *would* leave: the stored balance plus
/// this transaction's own postings for that account. The account row is already
/// locked, so no other poster can change that balance in between, and nothing
/// has been written yet — so a refusal leaves no row to roll back.
async fn check_negative_balances(
    conn: &mut RuntimeConnection,
    accounts: &BTreeMap<String, Account>,
    transfer: &Transaction,
) -> Result<(), LedgerError> {
    for account in accounts.values() {
        if account.allow_negative {
            continue;
        }
        let mut balance = sum_postings(conn, &account.id).await?;
        for posting in transfer.postings() {
            if posting.account_id() == account.id {
                balance = balance
                    .checked_add(posting.signed_minor()?)
                    .ok_or(MoneyError::Overflow)?;
            }
        }
        if balance < 0 {
            return Err(LedgerError::NegativeBalance {
                account: account.id.clone(),
                balance,
                currency: account.currency.code(),
            });
        }
    }
    Ok(())
}

/// The balance of `account_id`: the sum of its postings.
///
/// # Errors
///
/// [`LedgerError::UnknownAccount`] when there is no such account, and
/// [`LedgerError::Database`] when the statement fails.
pub async fn balance(
    conn: &mut RuntimeConnection,
    account_id: &str,
) -> Result<AnyMoney, LedgerError> {
    let account = load_account(conn, account_id)
        .await?
        .ok_or_else(|| LedgerError::UnknownAccount {
            id: account_id.to_owned(),
        })?;
    let total = sum_postings(conn, account_id).await?;
    Ok(AnyMoney::new(total, account.currency))
}

/// `CAST(... AS BIGINT)` because Postgres widens `SUM(BIGINT)` to `NUMERIC`,
/// which no `BigInt` decoder accepts. `SQLite` keeps the integer either way.
async fn sum_postings(
    conn: &mut RuntimeConnection,
    account_id: &str,
) -> Result<i64, LedgerError> {
    let sql = format!(
        "SELECT CAST(COALESCE(SUM(amount_minor), 0) AS BIGINT) AS total \
         FROM {POSTINGS_TABLE} WHERE account_id = {}",
        ph(1)
    );
    let rows: Vec<TotalRow> = diesel::sql_query(sql)
        .bind::<Text, _>(account_id)
        .load(conn)
        .await?;
    Ok(rows.into_iter().next().map_or(0, |row| row.total))
}

/// Read the transaction posted under `key`, if there is one.
///
/// # Errors
///
/// [`LedgerError::Database`] when a statement fails, and [`LedgerError::Money`]
/// when a stored currency code is not one this build knows.
pub async fn transaction_by_key(
    conn: &mut RuntimeConnection,
    key: &IdempotencyKey,
) -> Result<Option<PostedTransaction>, LedgerError> {
    Ok(load_transaction(conn, key.as_str())
        .await?
        .map(|stored| stored.transaction))
}

/// A stored transaction plus the request hash, which callers never see.
struct StoredTransaction {
    transaction: PostedTransaction,
    request_hash: String,
}

async fn load_transaction(
    conn: &mut RuntimeConnection,
    key: &str,
) -> Result<Option<StoredTransaction>, LedgerError> {
    let sql = format!(
        "SELECT id, idempotency_key, request_hash, currency, memo, \
           CAST(posted_at AS TEXT) AS posted_at \
         FROM {TRANSACTIONS_TABLE} WHERE idempotency_key = {}",
        ph(1)
    );
    let rows: Vec<TransactionRow> = diesel::sql_query(sql)
        .bind::<Text, _>(key)
        .load(conn)
        .await?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };

    let postings_sql = format!(
        "SELECT account_id, amount_minor, currency FROM {POSTINGS_TABLE} \
         WHERE transaction_id = {} ORDER BY seq",
        ph(1)
    );
    let posting_rows: Vec<PostingRow> = diesel::sql_query(postings_sql)
        .bind::<Text, _>(row.id.clone())
        .load(conn)
        .await?;
    let postings = posting_rows
        .into_iter()
        .map(PostingRow::into_posting)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Some(StoredTransaction {
        transaction: PostedTransaction {
            id: row.id,
            key: IdempotencyKey(row.idempotency_key),
            currency: parse_currency(&row.currency)?,
            memo: row.memo,
            postings,
            posted_at: row.posted_at,
        },
        request_hash: row.request_hash,
    }))
}

/// Every currency's total across the whole ledger.
///
/// Each total must be zero: a balanced transaction contributes zero, and the
/// ledger only holds balanced transactions. A non-zero total is proof that
/// something wrote around [`post`].
///
/// # Errors
///
/// [`LedgerError::Database`] when the statement fails, and
/// [`LedgerError::Money`] when a stored currency code is not one this build
/// knows.
pub async fn trial_balance(
    conn: &mut RuntimeConnection,
) -> Result<Vec<CurrencyTotal>, LedgerError> {
    let sql = format!(
        "SELECT currency, CAST(SUM(amount_minor) AS BIGINT) AS total \
         FROM {POSTINGS_TABLE} GROUP BY currency ORDER BY currency"
    );
    let rows: Vec<CurrencyTotalRow> = diesel::sql_query(sql).load(conn).await?;
    rows.into_iter()
        .map(|row| {
            let currency = parse_currency(&row.currency)?;
            Ok(CurrencyTotal {
                currency,
                total: AnyMoney::new(row.total, currency),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
