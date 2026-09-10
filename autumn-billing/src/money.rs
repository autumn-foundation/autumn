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
//! Exact money: integer minor units plus an explicit currency.
//!
//! No floating point type appears in this module. Conversions to and from
//! [`Decimal`] are exact or they fail.

use std::fmt;
use std::str::FromStr;

use autumn_web::reexports::rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A money error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum MoneyError {
    /// The currency code is not three ASCII letters.
    #[error("invalid currency code {0:?}")]
    InvalidCurrency(String),
    /// Two values have different currencies.
    #[error("currency mismatch: {0} vs {1}")]
    CurrencyMismatch(Currency, Currency),
    /// The decimal has more fraction digits than the currency allows.
    #[error("{0} has more fraction digits than {1} allows")]
    ExcessScale(Decimal, Currency),
    /// The value does not fit in `i64` minor units.
    #[error("money value overflows")]
    Overflow,
}

/// An ISO 4217 currency code.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Currency([u8; 3]);

impl Currency {
    /// US dollar.
    pub const USD: Self = Self(*b"USD");
    /// Euro.
    pub const EUR: Self = Self(*b"EUR");
    /// Pound sterling.
    pub const GBP: Self = Self(*b"GBP");
    /// Japanese yen.
    pub const JPY: Self = Self(*b"JPY");

    /// Parse a three-letter code. Case is normalized to upper case.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::InvalidCurrency`] when `code` is not three ASCII letters.
    pub fn new(code: &str) -> Result<Self, MoneyError> {
        let bytes = code.as_bytes();
        let [a, b, c] = bytes else {
            return Err(MoneyError::InvalidCurrency(code.to_owned()));
        };
        if !bytes.iter().all(u8::is_ascii_alphabetic) {
            return Err(MoneyError::InvalidCurrency(code.to_owned()));
        }
        Ok(Self([
            a.to_ascii_uppercase(),
            b.to_ascii_uppercase(),
            c.to_ascii_uppercase(),
        ]))
    }

    /// The upper-case code.
    #[must_use]
    pub fn code(&self) -> &str {
        // The constructor only accepts ASCII letters.
        std::str::from_utf8(&self.0).unwrap_or("???")
    }

    /// Number of fraction digits (minor units per major unit = 10^exponent).
    #[must_use]
    pub const fn exponent(&self) -> u32 {
        match &self.0 {
            // Zero-decimal currencies (Stripe list).
            b"BIF" | b"CLP" | b"DJF" | b"GNF" | b"JPY" | b"KMF" | b"KRW" | b"MGA" | b"PYG"
            | b"RWF" | b"UGX" | b"VND" | b"VUV" | b"XAF" | b"XOF" | b"XPF" => 0,
            // Three-decimal currencies.
            b"BHD" | b"IQD" | b"JOD" | b"KWD" | b"LYD" | b"OMR" | b"TND" => 3,
            _ => 2,
        }
    }
}

impl fmt::Debug for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Currency({})", self.code())
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl FromStr for Currency {
    type Err = MoneyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl Serialize for Currency {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let code = String::deserialize(deserializer)?;
        Self::new(&code).map_err(serde::de::Error::custom)
    }
}

/// An exact amount of money in minor units of one currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Money {
    /// Amount in minor units (cents for USD).
    minor: i64,
    /// Currency of the amount.
    currency: Currency,
}

impl Money {
    /// Build from minor units.
    #[must_use]
    pub const fn from_minor(minor: i64, currency: Currency) -> Self {
        Self { minor, currency }
    }

    /// Zero in `currency`.
    #[must_use]
    pub const fn zero(currency: Currency) -> Self {
        Self::from_minor(0, currency)
    }

    /// Build from a decimal major-unit amount. The conversion is exact.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::ExcessScale`] when `amount` has more fraction
    /// digits than the currency allows, and [`MoneyError::Overflow`] when the
    /// result does not fit in `i64`.
    pub fn from_decimal(amount: Decimal, currency: Currency) -> Result<Self, MoneyError> {
        // Work on the integer mantissa so no rounding can happen.
        let exponent = currency.exponent();
        let scale = amount.scale();
        let mantissa = amount.mantissa();
        let minor = if scale <= exponent {
            // Fewer fraction digits than the currency: scale up.
            let factor = 10_i128
                .checked_pow(exponent.saturating_sub(scale))
                .ok_or(MoneyError::Overflow)?;
            mantissa.checked_mul(factor).ok_or(MoneyError::Overflow)?
        } else {
            // More fraction digits: the extra digits must all be zero.
            let divisor = 10_i128
                .checked_pow(scale.saturating_sub(exponent))
                .ok_or(MoneyError::Overflow)?;
            let remainder = mantissa.checked_rem(divisor).ok_or(MoneyError::Overflow)?;
            if remainder != 0 {
                return Err(MoneyError::ExcessScale(amount, currency));
            }
            mantissa.checked_div(divisor).ok_or(MoneyError::Overflow)?
        };
        let minor = i64::try_from(minor).map_err(|_| MoneyError::Overflow)?;
        Ok(Self { minor, currency })
    }

    /// The amount in minor units.
    #[must_use]
    pub const fn minor(&self) -> i64 {
        self.minor
    }

    /// The currency.
    #[must_use]
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// The amount as an exact decimal in major units.
    #[must_use]
    pub fn to_decimal(&self) -> Decimal {
        // The exponent is at most 3, far below the 28-digit scale limit.
        Decimal::new(self.minor, self.currency.exponent())
    }

    /// Checked addition. Fails on a currency mismatch or overflow.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::CurrencyMismatch`] or [`MoneyError::Overflow`].
    pub fn checked_add(self, other: Self) -> Result<Self, MoneyError> {
        self.same_currency(other)?;
        let minor = self
            .minor
            .checked_add(other.minor)
            .ok_or(MoneyError::Overflow)?;
        Ok(Self { minor, ..self })
    }

    /// Checked subtraction. Fails on a currency mismatch or overflow.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::CurrencyMismatch`] or [`MoneyError::Overflow`].
    pub fn checked_sub(self, other: Self) -> Result<Self, MoneyError> {
        self.same_currency(other)?;
        let minor = self
            .minor
            .checked_sub(other.minor)
            .ok_or(MoneyError::Overflow)?;
        Ok(Self { minor, ..self })
    }

    /// Fail when `other` has a different currency.
    fn same_currency(self, other: Self) -> Result<(), MoneyError> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(MoneyError::CurrencyMismatch(self.currency, other.currency))
        }
    }

    /// `true` when the amount is zero.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.minor == 0
    }
}

impl fmt::Display for Money {
    /// Renders `19.99 USD`, `500 JPY`, `1.250 KWD`: always `exponent`
    /// fraction digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `to_decimal` keeps scale = exponent, so the digit count is fixed.
        write!(f, "{} {}", self.to_decimal(), self.currency)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("valid decimal literal")
    }

    #[test]
    fn exponent_table() {
        assert_eq!(Currency::USD.exponent(), 2);
        assert_eq!(Currency::EUR.exponent(), 2);
        assert_eq!(Currency::JPY.exponent(), 0);
        assert_eq!(Currency::new("kwd").unwrap().exponent(), 3);
        assert_eq!(Currency::new("BHD").unwrap().exponent(), 3);
        assert_eq!(Currency::new("XYZ").unwrap().exponent(), 2);
    }

    #[test]
    fn currency_new_normalizes_case() {
        assert_eq!(Currency::new("usd").unwrap(), Currency::USD);
        assert_eq!(Currency::new("Usd").unwrap().code(), "USD");
        assert_eq!(Currency::from_str("jpy").unwrap(), Currency::JPY);
        assert_eq!(format!("{}", Currency::EUR), "EUR");
        assert_eq!(format!("{:?}", Currency::GBP), "Currency(GBP)");
    }

    #[test]
    fn currency_new_rejects_bad_codes() {
        for bad in ["US", "US1", "usdx", "", "U$D"] {
            assert_eq!(
                Currency::new(bad),
                Err(MoneyError::InvalidCurrency(bad.to_owned())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn from_decimal_round_trips() {
        let cases = [
            ("19.99", Currency::USD, 1999),
            ("19.990", Currency::USD, 1999),
            ("0.01", Currency::USD, 1),
            ("-4.50", Currency::EUR, -450),
            ("500", Currency::JPY, 500),
            ("500.0", Currency::JPY, 500),
            ("1.250", Currency::new("KWD").unwrap(), 1250),
            ("0", Currency::USD, 0),
        ];
        for (text, currency, minor) in cases {
            let money = Money::from_decimal(dec(text), currency).unwrap();
            assert_eq!(money.minor(), minor, "{text} {currency}");
            assert_eq!(money.currency(), currency);
            assert_eq!(money.to_decimal(), dec(text), "{text} {currency}");
            assert_eq!(
                Money::from_decimal(money.to_decimal(), currency).unwrap(),
                money
            );
        }
    }

    #[test]
    fn from_decimal_rejects_excess_scale() {
        let kwd = Currency::new("KWD").unwrap();
        for (text, currency) in [
            ("19.995", Currency::USD),
            ("0.001", Currency::USD),
            ("500.5", Currency::JPY),
            ("1.2505", kwd),
        ] {
            assert_eq!(
                Money::from_decimal(dec(text), currency),
                Err(MoneyError::ExcessScale(dec(text), currency)),
                "{text} {currency}"
            );
        }
    }

    #[test]
    fn from_decimal_rejects_i64_overflow() {
        let too_big = Decimal::from(i64::MAX)
            .checked_add(Decimal::ONE)
            .expect("fits in Decimal");
        assert_eq!(
            Money::from_decimal(too_big, Currency::JPY),
            Err(MoneyError::Overflow)
        );
        // i64::MAX major units in a 2-decimal currency needs 100x more minor units.
        assert_eq!(
            Money::from_decimal(Decimal::from(i64::MAX), Currency::USD),
            Err(MoneyError::Overflow)
        );
        assert_eq!(
            Money::from_decimal(Decimal::MAX, Currency::USD),
            Err(MoneyError::Overflow)
        );
        assert_eq!(
            Money::from_decimal(Decimal::MIN, Currency::USD),
            Err(MoneyError::Overflow)
        );
        // Exactly i64::MAX minor units still fits.
        let max = Money::from_decimal(Decimal::from(i64::MAX), Currency::JPY).unwrap();
        assert_eq!(max.minor(), i64::MAX);
        let min = Money::from_decimal(Decimal::from(i64::MIN), Currency::JPY).unwrap();
        assert_eq!(min.minor(), i64::MIN);
    }

    #[test]
    fn to_decimal_is_exact() {
        assert_eq!(
            Money::from_minor(1999, Currency::USD).to_decimal(),
            Decimal::new(1999, 2)
        );
        assert_eq!(
            Money::from_minor(500, Currency::JPY).to_decimal(),
            Decimal::new(500, 0)
        );
        assert_eq!(
            Money::from_minor(i64::MAX, Currency::USD).to_decimal(),
            Decimal::new(i64::MAX, 2)
        );
        assert!(Money::zero(Currency::USD).is_zero());
        assert!(!Money::from_minor(1, Currency::USD).is_zero());
    }

    #[test]
    fn checked_add_and_sub() {
        let a = Money::from_minor(1999, Currency::USD);
        let b = Money::from_minor(1, Currency::USD);
        assert_eq!(
            a.checked_add(b).unwrap(),
            Money::from_minor(2000, Currency::USD)
        );
        assert_eq!(
            a.checked_sub(b).unwrap(),
            Money::from_minor(1998, Currency::USD)
        );
        assert_eq!(
            b.checked_sub(a).unwrap(),
            Money::from_minor(-1998, Currency::USD)
        );
    }

    #[test]
    fn checked_ops_reject_currency_mismatch() {
        let usd = Money::from_minor(1, Currency::USD);
        let eur = Money::from_minor(1, Currency::EUR);
        assert_eq!(
            usd.checked_add(eur),
            Err(MoneyError::CurrencyMismatch(Currency::USD, Currency::EUR))
        );
        assert_eq!(
            eur.checked_sub(usd),
            Err(MoneyError::CurrencyMismatch(Currency::EUR, Currency::USD))
        );
    }

    #[test]
    fn checked_ops_reject_overflow() {
        let max = Money::from_minor(i64::MAX, Currency::USD);
        let min = Money::from_minor(i64::MIN, Currency::USD);
        let one = Money::from_minor(1, Currency::USD);
        assert_eq!(max.checked_add(one), Err(MoneyError::Overflow));
        assert_eq!(min.checked_sub(one), Err(MoneyError::Overflow));
        assert_eq!(max.checked_add(min).unwrap().minor(), -1);
    }

    #[test]
    fn display_uses_exponent_fraction_digits() {
        let kwd = Currency::new("KWD").unwrap();
        assert_eq!(
            Money::from_minor(1999, Currency::USD).to_string(),
            "19.99 USD"
        );
        assert_eq!(
            Money::from_minor(1900, Currency::USD).to_string(),
            "19.00 USD"
        );
        assert_eq!(Money::from_minor(5, Currency::USD).to_string(), "0.05 USD");
        assert_eq!(
            Money::from_minor(-5, Currency::USD).to_string(),
            "-0.05 USD"
        );
        assert_eq!(Money::from_minor(500, Currency::JPY).to_string(), "500 JPY");
        assert_eq!(Money::from_minor(1250, kwd).to_string(), "1.250 KWD");
        assert_eq!(Money::zero(kwd).to_string(), "0.000 KWD");
    }

    #[test]
    fn serde_round_trip() {
        let money = Money::from_minor(1999, Currency::USD);
        let json = serde_json::to_string(&money).unwrap();
        assert_eq!(json, r#"{"minor":1999,"currency":"USD"}"#);
        let back: Money = serde_json::from_str(&json).unwrap();
        assert_eq!(back, money);

        assert_eq!(serde_json::to_string(&Currency::JPY).unwrap(), r#""JPY""#);
        let lower: Currency = serde_json::from_str(r#""eur""#).unwrap();
        assert_eq!(lower, Currency::EUR);
        let bad = serde_json::from_str::<Currency>(r#""EURO""#).unwrap_err();
        assert!(bad.to_string().contains("invalid currency code"), "{bad}");
    }

    #[test]
    fn money_error_display() {
        assert_eq!(
            MoneyError::CurrencyMismatch(Currency::USD, Currency::EUR).to_string(),
            "currency mismatch: USD vs EUR"
        );
        assert_eq!(
            MoneyError::ExcessScale(dec("1.005"), Currency::USD).to_string(),
            "1.005 has more fraction digits than USD allows"
        );
    }

    /// Every source file under `src/` is free of floating point type names.
    #[test]
    fn crate_has_no_float_types() {
        fn visit(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    visit(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }

        fn is_word_byte(b: u8) -> bool {
            b.is_ascii_alphanumeric() || b == b'_'
        }

        // Built at runtime so this test does not trip its own scan.
        fn float_tokens() -> [String; 2] {
            [format!("f{}", 32), format!("f{}", 64)]
        }

        fn has_float_token(text: &str) -> Option<usize> {
            let bytes = text.as_bytes();
            for needle in float_tokens() {
                let mut from = 0;
                while let Some(pos) = text[from..].find(needle.as_str()) {
                    let start = from + pos;
                    let end = start + needle.len();
                    let before = start.checked_sub(1).map(|i| bytes[i]);
                    let after = bytes.get(end).copied();
                    if !before.is_some_and(is_word_byte) && !after.is_some_and(is_word_byte) {
                        return Some(text[..start].lines().count());
                    }
                    from = end;
                }
            }
            None
        }

        let [f32_token, f64_token] = float_tokens();
        assert_eq!(
            has_float_token(&format!("let x: {f64_token} = 1.0;")),
            Some(1)
        );
        assert_eq!(
            has_float_token(&format!("\nlet y: {f32_token} = 1.0;")),
            Some(2)
        );
        assert_eq!(
            has_float_token(&format!("let {f64_token}x = 1; let x{f32_token} = 2;")),
            None
        );

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        visit(&src, &mut files);
        assert!(
            !files.is_empty(),
            "no source files found under {}",
            src.display()
        );
        let mut offenders = Vec::new();
        for path in files {
            let text = std::fs::read_to_string(&path).expect("read source file");
            if let Some(line) = has_float_token(&text) {
                offenders.push(format!("{}:{line}", path.display()));
            }
        }
        assert!(
            offenders.is_empty(),
            "float types are forbidden in autumn-billing: {offenders:?}"
        );
    }
}
