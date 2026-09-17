//! Unit tests for the double-entry rules and the idempotency key (#1837).
//!
//! Everything here runs without a database: `Transaction::validate` is what
//! `post` calls before its first `INSERT`, so the zero-sum invariant is
//! testable on its own. The store itself is proved in
//! `autumn/tests/sqlite_money_ledger.rs` and
//! `autumn/tests/integration/money_ledger_postgres.rs`.

use super::*;
use crate::money::{Eur, Money, Usd};

fn usd(minor: i64) -> Money<Usd> {
    Money::<Usd>::from_minor(minor)
}

fn key(text: &str) -> IdempotencyKey {
    IdempotencyKey::new(text).expect("valid key")
}

fn balanced() -> Vec<Posting> {
    vec![
        Posting::debit("customer:wallet", usd(2500)),
        Posting::credit("platform:revenue", usd(2500)),
    ]
}

// ── Zero-sum ────────────────────────────────────────────────────────────────

#[test]
fn a_balanced_transaction_validates() {
    let checked = Transaction::new(key("k1"), balanced()).validate().unwrap();
    assert_eq!(checked.currency(), Usd::currency());
    assert_eq!(checked.total().minor(), 2500);
    assert_eq!(checked.request_hash().len(), 64);
}

#[test]
fn debits_that_do_not_equal_credits_are_refused() {
    let transfer = Transaction::new(
        key("k1"),
        vec![
            Posting::debit("customer:wallet", usd(2500)),
            Posting::credit("platform:revenue", usd(2499)),
        ],
    );
    assert!(matches!(
        transfer.validate(),
        Err(LedgerError::Unbalanced {
            debits: 2500,
            credits: 2499,
            currency: "USD",
        })
    ));
}

#[test]
fn a_many_sided_transaction_balances_across_all_of_it() {
    // A marketplace split: one charge, three destinations.
    let transfer = Transaction::new(
        key("k1"),
        vec![
            Posting::debit("customer:wallet", usd(10_000)),
            Posting::credit("seller:1", usd(7000)),
            Posting::credit("seller:2", usd(2000)),
            Posting::credit("platform:fees", usd(1000)),
        ],
    );
    assert_eq!(transfer.validate().unwrap().total().minor(), 10_000);
}

#[test]
fn an_empty_or_one_sided_transaction_does_not_balance_by_default() {
    assert!(matches!(
        Transaction::new(key("k1"), Vec::new()).validate(),
        Err(LedgerError::PostingCount { count: 0 })
    ));
    assert!(matches!(
        Transaction::new(key("k1"), vec![Posting::debit("a", usd(100))]).validate(),
        Err(LedgerError::PostingCount { count: 1 })
    ));
    // Two postings, both debits: the sum is not zero, and the message says why.
    assert!(matches!(
        Transaction::new(
            key("k1"),
            vec![Posting::debit("a", usd(100)), Posting::debit("b", usd(100))],
        )
        .validate(),
        Err(LedgerError::OneSided)
    ));
}

#[test]
fn a_transaction_that_moves_nothing_is_refused() {
    let transfer = Transaction::new(
        key("k1"),
        vec![Posting::debit("a", usd(0)), Posting::credit("b", usd(0))],
    );
    assert!(matches!(transfer.validate(), Err(LedgerError::ZeroValue)));
}

#[test]
fn mixed_currencies_are_refused() {
    let transfer = Transaction::new(
        key("k1"),
        vec![
            Posting::debit("a", usd(2500)),
            Posting::credit("b", Money::<Eur>::from_minor(2500)),
        ],
    );
    assert!(matches!(
        transfer.validate(),
        Err(LedgerError::MixedCurrencies {
            expected: "USD",
            found: "EUR",
        })
    ));
}

#[test]
fn a_negative_amount_belongs_to_the_side_not_to_the_value() {
    let transfer = Transaction::new(
        key("k1"),
        vec![
            Posting::debit("a", usd(-2500)),
            Posting::credit("b", usd(2500)),
        ],
    );
    assert!(matches!(
        transfer.validate(),
        Err(LedgerError::NegativeAmount { .. })
    ));
}

#[test]
fn the_extreme_value_cannot_reach_the_ledger() {
    // `i64::MIN` has no positive form, so it can only arrive as a negative
    // amount — which is refused before anything tries to negate it.
    let posting = Posting::credit("a", usd(i64::MIN));
    assert!(matches!(
        posting.signed_minor(),
        Err(LedgerError::NegativeAmount { .. })
    ));
}

#[test]
fn a_sum_that_leaves_i64_is_an_overflow_not_a_wrap() {
    let transfer = Transaction::new(
        key("k1"),
        vec![
            Posting::debit("a", usd(i64::MAX)),
            Posting::debit("b", usd(i64::MAX)),
            Posting::credit("c", usd(i64::MAX)),
        ],
    );
    assert!(matches!(
        transfer.validate(),
        Err(LedgerError::Money(MoneyError::Overflow))
    ));
}

#[test]
fn empty_and_over_long_text_is_refused() {
    assert!(matches!(
        IdempotencyKey::new(""),
        Err(LedgerError::InvalidText {
            field: "idempotency key",
            ..
        })
    ));
    assert!(matches!(
        IdempotencyKey::new("x".repeat(256)),
        Err(LedgerError::InvalidText {
            field: "idempotency key",
            ..
        })
    ));
    assert!(matches!(
        Transaction::new(
            key("k1"),
            vec![Posting::debit("", usd(100)), Posting::credit("b", usd(100))],
        )
        .validate(),
        Err(LedgerError::InvalidText {
            field: "account id",
            ..
        })
    ));
    assert!(matches!(
        Transaction::new(key("k1"), balanced())
            .memo("m".repeat(1025))
            .validate(),
        Err(LedgerError::InvalidText { field: "memo", .. })
    ));
}

// ── Sides ───────────────────────────────────────────────────────────────────

#[test]
fn sides_carry_the_sign_the_store_writes() {
    let debit = Posting::debit("a", usd(2500));
    assert!(debit.is_debit());
    assert_eq!(debit.side(), Side::Debit);
    assert_eq!(debit.signed_minor().unwrap(), 2500);
    assert_eq!(debit.amount().minor(), 2500);

    let credit = Posting::credit("b", usd(2500));
    assert!(credit.is_credit());
    assert_eq!(credit.side(), Side::Credit);
    assert_eq!(credit.signed_minor().unwrap(), -2500);
    assert_eq!(credit.amount().minor(), 2500);
}

// ── Idempotency keys ────────────────────────────────────────────────────────

#[test]
fn a_derived_key_is_stable_across_build_order() {
    let forward = IdempotencyKey::derive("order:9911", &balanced());
    let reversed = IdempotencyKey::derive(
        "order:9911",
        &[
            Posting::credit("platform:revenue", usd(2500)),
            Posting::debit("customer:wallet", usd(2500)),
        ],
    );
    assert_eq!(forward, reversed);
    assert_eq!(forward.as_str().len(), 64);
}

#[test]
fn a_derived_key_changes_with_the_money() {
    let base = IdempotencyKey::derive("order:9911", &balanced());
    let other_amount = IdempotencyKey::derive(
        "order:9911",
        &[
            Posting::debit("customer:wallet", usd(2501)),
            Posting::credit("platform:revenue", usd(2501)),
        ],
    );
    let other_account = IdempotencyKey::derive(
        "order:9911",
        &[
            Posting::debit("customer:other", usd(2500)),
            Posting::credit("platform:revenue", usd(2500)),
        ],
    );
    let other_namespace = IdempotencyKey::derive("order:9912", &balanced());
    let other_side = IdempotencyKey::derive(
        "order:9911",
        &[
            Posting::credit("customer:wallet", usd(2500)),
            Posting::debit("platform:revenue", usd(2500)),
        ],
    );
    for other in [other_amount, other_account, other_namespace, other_side] {
        assert_ne!(base, other);
    }
}

#[test]
fn the_namespace_and_the_postings_cannot_run_together() {
    // A length-prefixed hash: moving a character from the namespace into an
    // account id must not produce the same key.
    let left = IdempotencyKey::derive(
        "ab",
        &[Posting::debit("c", usd(1)), Posting::credit("d", usd(1))],
    );
    let right = IdempotencyKey::derive(
        "a",
        &[Posting::debit("bc", usd(1)), Posting::credit("d", usd(1))],
    );
    assert_ne!(left, right);
}

#[test]
fn the_request_hash_ignores_the_memo_and_the_order() {
    let plain = Transaction::new(key("k1"), balanced()).validate().unwrap();
    let described = Transaction::new(key("k1"), balanced())
        .memo("order 9911, retried")
        .validate()
        .unwrap();
    assert_eq!(plain.request_hash(), described.request_hash());

    let reordered = Transaction::new(
        key("k1"),
        vec![
            Posting::credit("platform:revenue", usd(2500)),
            Posting::debit("customer:wallet", usd(2500)),
        ],
    )
    .validate()
    .unwrap();
    assert_eq!(plain.request_hash(), reordered.request_hash());
}

#[test]
fn the_request_hash_changes_with_the_money() {
    let base = Transaction::new(key("k1"), balanced()).validate().unwrap();
    let different = Transaction::new(
        key("k1"),
        vec![
            Posting::debit("customer:wallet", usd(2501)),
            Posting::credit("platform:revenue", usd(2501)),
        ],
    )
    .validate()
    .unwrap();
    assert_ne!(base.request_hash(), different.request_hash());
}

// ── Accounts ────────────────────────────────────────────────────────────────

#[test]
fn accounts_allow_a_negative_balance_unless_told_otherwise() {
    let wallet = Account::new("customer:wallet", Usd::currency());
    assert!(wallet.allows_negative());
    assert_eq!(wallet.currency(), Usd::currency());
    assert_eq!(wallet.id(), "customer:wallet");

    let float = Account::new("platform:float", Usd::currency()).disallow_negative();
    assert!(!float.allows_negative());
}

// ── Error mapping ───────────────────────────────────────────────────────────

#[test]
fn errors_carry_the_status_a_handler_should_return() {
    use axum::http::StatusCode;

    assert_eq!(
        LedgerError::Unbalanced {
            debits: 1,
            credits: 2,
            currency: "USD"
        }
        .http_status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        LedgerError::KeyReuse {
            key: "k".to_owned()
        }
        .http_status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        LedgerError::NegativeBalance {
            account: "a".to_owned(),
            balance: -1,
            currency: "USD"
        }
        .http_status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        LedgerError::Money(MoneyError::Overflow).http_status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        LedgerError::Database(diesel::result::Error::NotFound).http_status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[test]
fn a_handler_gets_the_same_status_through_autumn_error() {
    use crate::error::AutumnError;
    use axum::http::StatusCode;

    let reuse = AutumnError::from(LedgerError::KeyReuse {
        key: "k".to_owned(),
    });
    assert_eq!(reuse.status(), StatusCode::CONFLICT);

    let unbalanced = AutumnError::from(LedgerError::Unbalanced {
        debits: 1,
        credits: 2,
        currency: "USD",
    });
    assert_eq!(unbalanced.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[test]
fn every_error_says_what_went_wrong() {
    let message = LedgerError::Unbalanced {
        debits: 2500,
        credits: 2499,
        currency: "USD",
    }
    .to_string();
    assert!(message.contains("2500"), "{message}");
    assert!(message.contains("2499"), "{message}");
    assert!(message.contains("USD"), "{message}");
}
