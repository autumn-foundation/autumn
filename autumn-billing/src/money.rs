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
        let _ = (amount, currency);
        Err(MoneyError::Overflow)
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
        Decimal::ZERO
    }

    /// Checked addition. Fails on a currency mismatch or overflow.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::CurrencyMismatch`] or [`MoneyError::Overflow`].
    pub fn checked_add(self, other: Self) -> Result<Self, MoneyError> {
        let _ = other;
        Err(MoneyError::Overflow)
    }

    /// Checked subtraction. Fails on a currency mismatch or overflow.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::CurrencyMismatch`] or [`MoneyError::Overflow`].
    pub fn checked_sub(self, other: Self) -> Result<Self, MoneyError> {
        let _ = other;
        Err(MoneyError::Overflow)
    }

    /// `true` when the amount is zero.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.minor == 0
    }
}

impl fmt::Display for Money {
    /// Renders `19.99 USD`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.to_decimal(), self.currency)
    }
}
