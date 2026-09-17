//! Unit tests for the money value type (issue #1837, AC-1).

use super::*;

// ── Minor units and major units ─────────────────────────────────────────────

#[test]
fn minor_units_round_trip() {
    assert_eq!(Money::<Usd>::from_minor(1250).minor(), 1250);
    assert_eq!(Money::<Usd>::from_minor(-1250).minor(), -1250);
    assert_eq!(Money::<Usd>::ZERO.minor(), 0);
}

#[test]
fn major_units_scale_by_the_currency_exponent() {
    assert_eq!(Money::<Usd>::from_major(3).unwrap().minor(), 300);
    assert_eq!(Money::<Jpy>::from_major(3).unwrap().minor(), 3);
    assert_eq!(Money::<Bhd>::from_major(3).unwrap().minor(), 3000);
}

#[test]
fn major_unit_overflow_is_an_error_not_a_wrap() {
    assert_eq!(Money::<Usd>::from_major(i64::MAX), Err(MoneyError::Overflow));
}

// ── Arithmetic in one currency ──────────────────────────────────────────────

#[test]
fn same_currency_adds_and_subtracts() {
    let fee = Money::<Usd>::from_minor(1250);
    let tip = Money::<Usd>::from_minor(375);
    assert_eq!(fee.checked_add(tip).unwrap().minor(), 1625);
    assert_eq!(fee.checked_sub(tip).unwrap().minor(), 875);
    assert_eq!(tip.checked_sub(fee).unwrap().minor(), -875);
}

#[test]
fn negation_and_absolute_value() {
    let owed = Money::<Usd>::from_minor(-500);
    assert_eq!(owed.checked_neg().unwrap().minor(), 500);
    assert_eq!(owed.checked_abs().unwrap().minor(), 500);
    assert!(owed.is_negative());
    assert!(!owed.is_positive());
    assert!(Money::<Usd>::ZERO.is_zero());
}

#[test]
fn multiplication_by_a_quantity() {
    let unit = Money::<Usd>::from_minor(199);
    assert_eq!(unit.checked_mul(7).unwrap().minor(), 1393);
}

#[test]
fn every_overflowing_operation_reports_it() {
    let big = Money::<Usd>::from_minor(i64::MAX);
    let one = Money::<Usd>::from_minor(1);
    assert_eq!(big.checked_add(one), Err(MoneyError::Overflow));
    assert_eq!(
        Money::<Usd>::from_minor(i64::MIN).checked_sub(one),
        Err(MoneyError::Overflow)
    );
    assert_eq!(
        Money::<Usd>::from_minor(i64::MIN).checked_neg(),
        Err(MoneyError::Overflow)
    );
    assert_eq!(
        Money::<Usd>::from_minor(i64::MIN).checked_abs(),
        Err(MoneyError::Overflow)
    );
    assert_eq!(big.checked_mul(2), Err(MoneyError::Overflow));
}

#[test]
fn summing_is_checked_too() {
    let values = vec![
        Money::<Usd>::from_minor(100),
        Money::<Usd>::from_minor(250),
        Money::<Usd>::from_minor(-50),
    ];
    assert_eq!(Money::<Usd>::try_sum(values).unwrap().minor(), 300);
    assert_eq!(Money::<Usd>::try_sum(Vec::new()).unwrap().minor(), 0);
    assert_eq!(
        Money::<Usd>::try_sum(vec![
            Money::<Usd>::from_minor(i64::MAX),
            Money::<Usd>::from_minor(1),
        ]),
        Err(MoneyError::Overflow)
    );
}

// ── Cross-currency ──────────────────────────────────────────────────────────

/// The compile-time half of the guarantee is the `compile_fail` doctest in the
/// module header. This is the run-time half, which is what a ledger row goes
/// through: a stored currency is a `TEXT` column, not a type.
#[test]
fn cross_currency_addition_is_rejected_at_run_time() {
    let usd = Money::<Usd>::from_minor(500).to_any();
    let eur = Money::<Eur>::from_minor(500).to_any();
    assert_eq!(
        usd.checked_add(eur),
        Err(MoneyError::CurrencyMismatch {
            expected: "USD",
            found: "EUR",
        })
    );
    assert!(matches!(
        usd.checked_sub(eur),
        Err(MoneyError::CurrencyMismatch { .. })
    ));
    assert!(matches!(
        AnyMoney::try_sum(Usd::currency(), vec![usd, eur]),
        Err(MoneyError::CurrencyMismatch { .. })
    ));
}

#[test]
fn recovering_the_typed_form_checks_the_currency() {
    let usd = Money::<Usd>::from_minor(500).to_any();
    assert_eq!(usd.try_into_typed::<Usd>().unwrap().minor(), 500);
    assert_eq!(
        usd.try_into_typed::<Eur>(),
        Err(MoneyError::CurrencyMismatch {
            expected: "EUR",
            found: "USD",
        })
    );
}

#[test]
fn runtime_currencies_resolve_by_code() {
    assert_eq!(CurrencyCode::parse("usd").unwrap(), Usd::currency());
    assert_eq!(CurrencyCode::parse("USD").unwrap().exponent(), 2);
    assert_eq!(CurrencyCode::parse("JPY").unwrap().exponent(), 0);
    assert_eq!(CurrencyCode::parse("KWD").unwrap().exponent(), 3);
    assert_eq!(
        CurrencyCode::parse("XYZ"),
        Err(MoneyError::UnknownCurrency {
            code: "XYZ".to_owned()
        })
    );
}

#[test]
fn the_currency_table_is_sorted_and_free_of_duplicates() {
    let codes: Vec<&str> = CurrencyCode::known().iter().map(|c| c.code()).collect();
    let mut sorted = codes.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(codes, sorted, "KNOWN must be sorted and unique");
}

#[test]
fn every_known_currency_has_a_supported_exponent() {
    for currency in CurrencyCode::known() {
        assert!(
            currency.exponent() <= MAX_EXPONENT,
            "{} exponent is out of range",
            currency.code()
        );
        assert!(currency.scale() > 0);
    }
}

// ── Rounding ────────────────────────────────────────────────────────────────

fn dec(text: &str) -> Decimal {
    text.parse().expect("decimal literal")
}

#[test]
fn decimal_round_trips_through_minor_units() {
    assert_eq!(Money::<Usd>::from_minor(1250).to_decimal(), dec("12.50"));
    assert_eq!(Money::<Jpy>::from_minor(1250).to_decimal(), dec("1250"));
    assert_eq!(Money::<Bhd>::from_minor(1250).to_decimal(), dec("1.250"));
}

#[test]
fn rounding_modes_are_explicit_and_distinct() {
    let half = dec("1.005");
    assert_eq!(
        Money::<Usd>::from_decimal(half, Rounding::HalfUp).unwrap().minor(),
        101
    );
    assert_eq!(
        Money::<Usd>::from_decimal(half, Rounding::HalfEven).unwrap().minor(),
        100
    );
    assert_eq!(
        Money::<Usd>::from_decimal(half, Rounding::HalfDown).unwrap().minor(),
        100
    );
    assert_eq!(
        Money::<Usd>::from_decimal(dec("1.009"), Rounding::TowardZero).unwrap().minor(),
        100
    );
    assert_eq!(
        Money::<Usd>::from_decimal(dec("1.001"), Rounding::AwayFromZero).unwrap().minor(),
        101
    );
    assert_eq!(
        Money::<Usd>::from_decimal(dec("-1.001"), Rounding::Floor).unwrap().minor(),
        -101
    );
    assert_eq!(
        Money::<Usd>::from_decimal(dec("-1.009"), Rounding::Ceiling).unwrap().minor(),
        -100
    );
}

#[test]
fn rounding_is_symmetric_for_negative_values() {
    assert_eq!(
        Money::<Usd>::from_decimal(dec("-1.005"), Rounding::HalfUp).unwrap().minor(),
        -101
    );
    assert_eq!(
        Money::<Usd>::from_decimal(dec("-1.005"), Rounding::HalfDown).unwrap().minor(),
        -100
    );
}

#[test]
fn exact_conversion_refuses_to_round() {
    assert_eq!(
        Money::<Usd>::from_decimal_exact(dec("12.50")).unwrap().minor(),
        1250
    );
    assert_eq!(
        Money::<Usd>::from_decimal_exact(dec("12.5")).unwrap().minor(),
        1250
    );
    assert!(matches!(
        Money::<Usd>::from_decimal_exact(dec("1.005")),
        Err(MoneyError::Inexact { .. })
    ));
    // A currency with no minor unit refuses a fraction outright.
    assert!(matches!(
        Money::<Jpy>::from_decimal_exact(dec("1.5")),
        Err(MoneyError::Inexact { .. })
    ));
}

#[test]
fn decimal_conversion_overflow_is_an_error() {
    assert_eq!(
        Money::<Usd>::from_decimal(dec("100000000000000000000"), Rounding::HalfUp),
        Err(MoneyError::Overflow)
    );
}

// ── Allocation ──────────────────────────────────────────────────────────────

fn minors<C: Currency>(parts: &[Money<C>]) -> Vec<i64> {
    parts.iter().map(|part| part.minor()).collect()
}

#[test]
fn allocation_loses_no_minor_unit() {
    let parts = Money::<Usd>::from_minor(100).allocate(&[1, 1, 1]).unwrap();
    assert_eq!(minors(&parts), vec![34, 33, 33]);
    assert_eq!(Money::<Usd>::try_sum(parts).unwrap().minor(), 100);
}

#[test]
fn allocation_follows_the_weights() {
    let parts = Money::<Usd>::from_minor(10_000).allocate(&[70, 20, 10]).unwrap();
    assert_eq!(minors(&parts), vec![7000, 2000, 1000]);

    // 5 cents over three unequal weights: the largest remainders take the
    // leftovers.
    let parts = Money::<Usd>::from_minor(5).allocate(&[1, 1, 1]).unwrap();
    assert_eq!(minors(&parts), vec![2, 2, 1]);
}

#[test]
fn allocation_of_a_negative_amount_also_loses_nothing() {
    let parts = Money::<Usd>::from_minor(-100).allocate(&[1, 1, 1]).unwrap();
    assert_eq!(Money::<Usd>::try_sum(parts.clone()).unwrap().minor(), -100);
    assert_eq!(minors(&parts), vec![-33, -33, -34]);
}

#[test]
fn allocation_sums_back_across_many_shapes() {
    for amount in [-1001_i64, -7, 0, 1, 99, 100, 12_345, 1_000_000] {
        for weights in [
            vec![1],
            vec![1, 1],
            vec![1, 1, 1],
            vec![3, 1],
            vec![7, 11, 13],
            vec![0, 1, 0, 1],
        ] {
            let parts = Money::<Usd>::from_minor(amount).allocate(&weights).unwrap();
            assert_eq!(parts.len(), weights.len());
            assert_eq!(
                Money::<Usd>::try_sum(parts).unwrap().minor(),
                amount,
                "amount {amount} over weights {weights:?}"
            );
        }
    }
}

#[test]
fn a_zero_weight_gets_nothing_when_the_amount_divides() {
    let parts = Money::<Usd>::from_minor(90).allocate(&[0, 1, 2]).unwrap();
    assert_eq!(minors(&parts), vec![0, 30, 60]);
}

#[test]
fn bad_weights_are_rejected() {
    assert_eq!(
        Money::<Usd>::from_minor(100).allocate(&[]),
        Err(MoneyError::InvalidWeights)
    );
    assert_eq!(
        Money::<Usd>::from_minor(100).allocate(&[0, 0]),
        Err(MoneyError::InvalidWeights)
    );
    assert_eq!(
        Money::<Usd>::from_minor(100).allocate(&[1, -1]),
        Err(MoneyError::InvalidWeights)
    );
    assert_eq!(
        Money::<Usd>::from_minor(100).split(0),
        Err(MoneyError::InvalidWeights)
    );
}

#[test]
fn split_is_allocation_with_equal_weights() {
    let parts = Money::<Usd>::from_minor(100).split(3).unwrap();
    assert_eq!(minors(&parts), vec![34, 33, 33]);
}

// ── Display ─────────────────────────────────────────────────────────────────

#[test]
fn display_uses_the_currency_exponent() {
    assert_eq!(Money::<Usd>::from_minor(1250).to_string(), "12.50 USD");
    assert_eq!(Money::<Usd>::from_minor(-5).to_string(), "-0.05 USD");
    assert_eq!(Money::<Jpy>::from_minor(1250).to_string(), "1250 JPY");
    assert_eq!(Money::<Bhd>::from_minor(1).to_string(), "0.001 BHD");
    assert_eq!(Money::<Usd>::ZERO.to_string(), "0.00 USD");
}

#[test]
fn display_handles_the_extreme_values() {
    assert_eq!(
        Money::<Usd>::from_minor(i64::MIN).to_string(),
        "-92233720368547758.08 USD"
    );
    assert_eq!(
        Money::<Usd>::from_minor(i64::MAX).to_string(),
        "92233720368547758.07 USD"
    );
}

#[test]
fn any_money_displays_the_same_way() {
    let value = AnyMoney::new(1250, CurrencyCode::parse("USD").unwrap());
    assert_eq!(value.to_string(), "12.50 USD");
    assert_eq!(value.to_decimal(), dec("12.50"));
}

// ── Ordering, equality, conversion ──────────────────────────────────────────

#[test]
fn values_order_by_amount() {
    let mut values = vec![
        Money::<Usd>::from_minor(5),
        Money::<Usd>::from_minor(-5),
        Money::<Usd>::from_minor(0),
    ];
    values.sort_unstable();
    assert_eq!(minors(&values), vec![-5, 0, 5]);
    assert_eq!(Money::<Usd>::default(), Money::<Usd>::ZERO);
}

#[test]
fn the_debug_form_names_the_currency() {
    assert_eq!(
        format!("{:?}", Money::<Usd>::from_minor(1250)),
        "Money(1250 USD)"
    );
    assert_eq!(format!("{:?}", Usd::currency()), "USD");
}

#[test]
fn errors_convert_into_autumn_errors() {
    use crate::error::AutumnError;

    let overflow = AutumnError::from(MoneyError::Overflow);
    assert_eq!(overflow.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    let mismatch = AutumnError::from(MoneyError::CurrencyMismatch {
        expected: "USD",
        found: "EUR",
    });
    assert_eq!(
        mismatch.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY
    );
}

/// AC-1 says "no `f64` anywhere in the value". This reads the module source and
/// pins that, so a later edit that reaches for a float fails here rather than
/// in production.
#[test]
fn the_value_module_holds_no_floating_point() {
    let source = include_str!("mod.rs");
    for (number, line) in source.lines().enumerate() {
        let code = line.split("//").next().unwrap_or(line);
        assert!(
            !code.contains("f64") && !code.contains("f32"),
            "line {} uses floating point: {line}",
            number.saturating_add(1)
        );
    }
}
