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
//! Typed money and an embedded double-entry ledger (issue #1837).
//!
//! Autumn formatted money for display ([`crate::format::number_to_currency`])
//! but had no money *value*: nothing carried a currency, and nothing stopped an
//! app from adding dollars to euros. This module adds the primitive.
//!
//! # The value
//!
//! [`Money<C>`] is an amount in one currency. The currency is a type parameter,
//! so a cross-currency operation does not compile:
//!
//! ```rust
//! use autumn_web::money::{Eur, Money, Usd};
//!
//! let fee = Money::<Usd>::from_minor(250);
//! let tip = Money::<Usd>::from_minor(125);
//! assert_eq!(fee.checked_add(tip)?.minor(), 375);
//! # let _ = Money::<Eur>::ZERO;
//! # Ok::<(), autumn_web::money::MoneyError>(())
//! ```
//!
//! Adding two currencies is a type error, not a run-time check. This example
//! is compiled, and the build fails if it ever starts to compile:
//!
//! ```compile_fail
//! use autumn_web::money::{Eur, Money, Usd};
//!
//! let dollars = Money::<Usd>::from_minor(250);
//! let euros = Money::<Eur>::from_minor(100);
//! let _ = dollars.checked_add(euros);
//! ```
//!
//! The amount is an `i64` count of **minor units** (cents for `USD`, yen for
//! `JPY`, fils for `BHD`). There is no `f64` in the value or in any operation on
//! it. Every operation that can leave the `i64` range is `checked_` and returns
//! [`MoneyError::Overflow`]; the module has no operator impls, because an
//! operator cannot report an overflow.
//!
//! # Runtime currencies
//!
//! A ledger row knows its currency only when it is read, so the same value has a
//! runtime-tagged form, [`AnyMoney`]. It rejects a cross-currency operation with
//! [`MoneyError::CurrencyMismatch`] instead of a compile error.
//!
//! ```rust
//! use autumn_web::money::{AnyMoney, CurrencyCode, MoneyError, Usd};
//!
//! let usd = AnyMoney::new(500, CurrencyCode::parse("USD")?);
//! let eur = AnyMoney::new(500, CurrencyCode::parse("EUR")?);
//! assert!(matches!(
//!     usd.checked_add(eur),
//!     Err(MoneyError::CurrencyMismatch { .. })
//! ));
//!
//! // Back to the typed form, checked once.
//! let typed = usd.try_into_typed::<Usd>()?;
//! assert_eq!(typed.minor(), 500);
//! # Ok::<(), MoneyError>(())
//! ```
//!
//! # Rounding
//!
//! Conversion from [`rust_decimal::Decimal`] is explicit about rounding — there
//! is no default that silently loses a cent:
//!
//! ```rust
//! use autumn_web::money::{Money, Rounding, Usd};
//! use rust_decimal::Decimal;
//!
//! let price: Decimal = "1.005".parse().unwrap();
//! assert_eq!(Money::<Usd>::from_decimal(price, Rounding::HalfUp)?.minor(), 101);
//! assert_eq!(Money::<Usd>::from_decimal(price, Rounding::HalfEven)?.minor(), 100);
//!
//! // Or refuse to round at all.
//! assert!(Money::<Usd>::from_decimal_exact(price).is_err());
//! # Ok::<(), autumn_web::money::MoneyError>(())
//! ```
//!
//! [`Money::allocate`] splits an amount by weights and loses nothing: the parts
//! always sum back to the whole.
//!
//! # The ledger
//!
//! See [`ledger`] for the double-entry store that posts these values.

#[cfg(feature = "db")]
pub mod ledger;

#[cfg(test)]
mod tests;

use core::fmt;
use core::marker::PhantomData;

use rust_decimal::Decimal;
use rust_decimal::RoundingStrategy;
use rust_decimal::prelude::ToPrimitive as _;

/// The largest ISO 4217 minor-unit exponent this module supports.
///
/// ISO 4217 uses 0 to 4. Each currency below is checked against this bound at
/// compile time, which is what lets [`scale_for`] use a closed table instead of
/// `i64::pow`, which can overflow.
const MAX_EXPONENT: u32 = 4;

/// The number of minor units in one major unit.
///
/// The `_` arm cannot be reached: `currencies!` asserts every exponent is at
/// most [`MAX_EXPONENT`] at compile time.
const fn scale_for(exponent: u32) -> i64 {
    match exponent {
        0 => 1,
        1 => 10,
        2 => 100,
        3 => 1_000,
        _ => 10_000,
    }
}

// ── Currency ────────────────────────────────────────────────────────────────

/// One currency, as a type.
///
/// Implemented by the zero-sized markers below ([`Usd`], [`Eur`], …). The type
/// parameter of [`Money<C>`] is what makes a cross-currency operation a compile
/// error.
pub trait Currency:
    Copy + Clone + fmt::Debug + PartialEq + Eq + core::hash::Hash + Send + Sync + 'static
{
    /// The ISO 4217 alphabetic code, for example `"USD"`.
    const CODE: &'static str;
    /// The number of decimal digits in the minor unit. 2 for `USD`, 0 for
    /// `JPY`, 3 for `BHD`.
    const EXPONENT: u32;
    /// The display symbol, for example `"$"`.
    const SYMBOL: &'static str;

    /// The same currency in its runtime-tagged form.
    #[must_use]
    fn currency() -> CurrencyCode {
        CurrencyCode {
            code: Self::CODE,
            exponent: Self::EXPONENT,
            symbol: Self::SYMBOL,
        }
    }
}

/// A currency known only at run time.
///
/// Carried by [`AnyMoney`] and by every ledger row, because the currency of a
/// stored posting is a `TEXT` column, not a type. Two codes are equal when their
/// ISO 4217 codes are equal.
#[derive(Clone, Copy)]
pub struct CurrencyCode {
    code: &'static str,
    exponent: u32,
    symbol: &'static str,
}

impl CurrencyCode {
    /// The ISO 4217 alphabetic code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        self.code
    }

    /// The number of decimal digits in the minor unit.
    #[must_use]
    pub const fn exponent(self) -> u32 {
        self.exponent
    }

    /// The display symbol.
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        self.symbol
    }

    /// The number of minor units in one major unit.
    #[must_use]
    pub const fn scale(self) -> i64 {
        scale_for(self.exponent)
    }

    /// Look up a currency by its ISO 4217 code. The match ignores case.
    ///
    /// # Errors
    ///
    /// Returns [`MoneyError::UnknownCurrency`] when the code is not in
    /// [`CurrencyCode::known`].
    pub fn parse(code: &str) -> Result<Self, MoneyError> {
        KNOWN
            .iter()
            .find(|known| known.code.eq_ignore_ascii_case(code))
            .copied()
            .ok_or_else(|| MoneyError::UnknownCurrency {
                code: code.to_owned(),
            })
    }

    /// Every currency this build knows.
    #[must_use]
    pub const fn known() -> &'static [Self] {
        KNOWN
    }
}

impl PartialEq for CurrencyCode {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code
    }
}
impl Eq for CurrencyCode {}
impl core::hash::Hash for CurrencyCode {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.code.hash(state);
    }
}
impl PartialOrd for CurrencyCode {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for CurrencyCode {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.code.cmp(other.code)
    }
}
impl fmt::Debug for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code)
    }
}
impl fmt::Display for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code)
    }
}

/// Give a currency marker its [`Currency`] impl and its row in [`KNOWN`], from
/// one list, so the two cannot disagree.
///
/// The markers themselves are written out above rather than emitted here: a
/// reader writes `autumn_web::money::Usd`, and `scripts/check-docs-symbols.sh`
/// reads the source to check that such a path resolves. A macro-emitted
/// `pub struct` is invisible to it, and to anyone grepping for the name.
macro_rules! currencies {
    ($( $ty:ident => ($code:literal, $exp:literal, $sym:literal) ),* $(,)?) => {
        $(
            impl Currency for $ty {
                const CODE: &'static str = $code;
                const EXPONENT: u32 = $exp;
                const SYMBOL: &'static str = $sym;
            }

            impl $ty {
                /// This currency, runtime-tagged.
                ///
                /// Inherent as well as on [`Currency`], so `Usd::currency()`
                /// works without importing the trait.
                #[must_use]
                pub const fn currency() -> CurrencyCode {
                    CurrencyCode { code: $code, exponent: $exp, symbol: $sym }
                }
            }

            // Keeps `scale_for`'s closed table total. Spelled as a set rather
            // than `<= MAX_EXPONENT`, because a comparison between two
            // constants is folded away and reported as useless.
            const _: () = assert!(matches!($exp, 0..=MAX_EXPONENT));
        )*

        /// Every currency in the table, sorted by code.
        static KNOWN: &[CurrencyCode] = &[
            $( CurrencyCode { code: $code, exponent: $exp, symbol: $sym } ),*
        ];
    };
}

/// UAE dirham. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Aed;

/// Australian dollar. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Aud;

/// Bahraini dinar. 3 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Bhd;

/// Brazilian real. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Brl;

/// Canadian dollar. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cad;

/// Swiss franc. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chf;

/// Chilean peso. 0 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Clp;

/// Chinese yuan. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cny;

/// Czech koruna. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Czk;

/// Danish krone. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Dkk;

/// Euro. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Eur;

/// Pound sterling. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Gbp;

/// Hong Kong dollar. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Hkd;

/// Hungarian forint. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Huf;

/// Israeli new shekel. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ils;

/// Indian rupee. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Inr;

/// Icelandic krona. 0 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Isk;

/// Jordanian dinar. 3 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Jod;

/// Japanese yen. 0 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Jpy;

/// South Korean won. 0 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Krw;

/// Kuwaiti dinar. 3 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Kwd;

/// Mexican peso. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Mxn;

/// Norwegian krone. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Nok;

/// New Zealand dollar. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Nzd;

/// Omani rial. 3 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Omr;

/// Polish zloty. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pln;

/// Saudi riyal. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sar;

/// Swedish krona. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sek;

/// Singapore dollar. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Sgd;

/// Turkish lira. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Try;

/// New Taiwan dollar. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Twd;

/// United States dollar. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Usd;

/// Vietnamese dong. 0 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Vnd;

/// South African rand. 2 minor digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Zar;

currencies! {
    Aed => ("AED", 2, "د.إ"),
    Aud => ("AUD", 2, "$"),
    Bhd => ("BHD", 3, ".د.ب"),
    Brl => ("BRL", 2, "R$"),
    Cad => ("CAD", 2, "$"),
    Chf => ("CHF", 2, "CHF"),
    Clp => ("CLP", 0, "$"),
    Cny => ("CNY", 2, "¥"),
    Czk => ("CZK", 2, "Kč"),
    Dkk => ("DKK", 2, "kr"),
    Eur => ("EUR", 2, "€"),
    Gbp => ("GBP", 2, "£"),
    Hkd => ("HKD", 2, "$"),
    Huf => ("HUF", 2, "Ft"),
    Ils => ("ILS", 2, "₪"),
    Inr => ("INR", 2, "₹"),
    Isk => ("ISK", 0, "kr"),
    Jod => ("JOD", 3, "د.ا"),
    Jpy => ("JPY", 0, "¥"),
    Krw => ("KRW", 0, "₩"),
    Kwd => ("KWD", 3, "د.ك"),
    Mxn => ("MXN", 2, "$"),
    Nok => ("NOK", 2, "kr"),
    Nzd => ("NZD", 2, "$"),
    Omr => ("OMR", 3, "ر.ع."),
    Pln => ("PLN", 2, "zł"),
    Sar => ("SAR", 2, "﷼"),
    Sek => ("SEK", 2, "kr"),
    Sgd => ("SGD", 2, "$"),
    Try => ("TRY", 2, "₺"),
    Twd => ("TWD", 2, "NT$"),
    Usd => ("USD", 2, "$"),
    Vnd => ("VND", 0, "₫"),
    Zar => ("ZAR", 2, "R"),
}

// ── Rounding ────────────────────────────────────────────────────────────────

/// How to turn a value with more precision than the currency into minor units.
///
/// There is no implicit choice: every conversion that can round names one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Rounding {
    /// Ties go away from zero. `1.005` becomes `1.01`, `-1.005` becomes
    /// `-1.01`. The common commercial rule, and the default.
    #[default]
    HalfUp,
    /// Ties go to the even digit. `1.005` becomes `1.00`, `1.015` becomes
    /// `1.02`. Removes the upward bias of [`Rounding::HalfUp`] over many values.
    HalfEven,
    /// Ties go toward zero. `1.005` becomes `1.00`.
    HalfDown,
    /// Always toward zero. `1.009` becomes `1.00`, `-1.009` becomes `-1.00`.
    TowardZero,
    /// Always away from zero. `1.001` becomes `1.01`.
    AwayFromZero,
    /// Always down. `1.009` becomes `1.00`, `-1.001` becomes `-1.01`.
    Floor,
    /// Always up. `1.001` becomes `1.01`, `-1.009` becomes `-1.00`.
    Ceiling,
}

impl Rounding {
    const fn strategy(self) -> RoundingStrategy {
        match self {
            Self::HalfUp => RoundingStrategy::MidpointAwayFromZero,
            Self::HalfEven => RoundingStrategy::MidpointNearestEven,
            Self::HalfDown => RoundingStrategy::MidpointTowardZero,
            Self::TowardZero => RoundingStrategy::ToZero,
            Self::AwayFromZero => RoundingStrategy::AwayFromZero,
            Self::Floor => RoundingStrategy::ToNegativeInfinity,
            Self::Ceiling => RoundingStrategy::ToPositiveInfinity,
        }
    }
}

// ── Errors ──────────────────────────────────────────────────────────────────

/// What can go wrong with a money value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MoneyError {
    /// The result is outside the `i64` minor-unit range.
    Overflow,
    /// Two amounts in different currencies were combined.
    CurrencyMismatch {
        /// The currency that was expected.
        expected: &'static str,
        /// The currency that was supplied.
        found: &'static str,
    },
    /// The ISO 4217 code is not in [`CurrencyCode::known`].
    UnknownCurrency {
        /// The code that was supplied.
        code: String,
    },
    /// The value has more precision than the currency, and the caller refused
    /// to round.
    Inexact {
        /// The value that does not fit.
        value: String,
        /// The currency it was converted to.
        currency: &'static str,
    },
    /// An allocation was given no weights, a negative weight, or weights that
    /// sum to zero.
    InvalidWeights,
}

impl fmt::Display for MoneyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow => f.write_str("money amount is out of range"),
            Self::CurrencyMismatch { expected, found } => {
                write!(f, "currency mismatch: expected {expected}, found {found}")
            }
            Self::UnknownCurrency { code } => write!(f, "unknown currency code: {code}"),
            Self::Inexact { value, currency } => {
                write!(f, "{value} has more precision than {currency} allows")
            }
            Self::InvalidWeights => f.write_str("allocation weights must be positive"),
        }
    }
}

impl std::error::Error for MoneyError {}

impl MoneyError {
    /// The HTTP status a handler returns for this error.
    ///
    /// `AutumnError`'s blanket `From<E: Error>` impl forecloses a dedicated
    /// `From`, so `error.rs` reads this through a downcast, the same way
    /// `ConstelaError` and `PushError` are mapped.
    ///
    /// Every variant is 422 but one: an overflow is a value the server built
    /// and could not hold, which is a fault worth an alert rather than a
    /// rejection worth showing the caller.
    #[must_use]
    pub const fn http_status(&self) -> axum::http::StatusCode {
        match *self {
            Self::Overflow => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            _ => axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        }
    }
}

// ── Money ───────────────────────────────────────────────────────────────────

/// An amount in currency `C`, as a count of minor units.
///
/// See the [module documentation](self) for the reasoning. `C` is a
/// [`Currency`] marker such as [`Usd`].
#[derive(Clone, Copy)]
pub struct Money<C: Currency> {
    minor: i64,
    currency: PhantomData<C>,
}

impl<C: Currency> Money<C> {
    /// Nothing, in currency `C`.
    pub const ZERO: Self = Self {
        minor: 0,
        currency: PhantomData,
    };

    /// Build from a count of minor units (cents for `USD`).
    #[must_use]
    pub const fn from_minor(minor: i64) -> Self {
        Self {
            minor,
            currency: PhantomData,
        }
    }

    /// The count of minor units.
    #[must_use]
    pub const fn minor(self) -> i64 {
        self.minor
    }

    /// This value's currency, runtime-tagged.
    #[must_use]
    pub fn currency() -> CurrencyCode {
        C::currency()
    }

    /// Build from a count of major units. `from_major(3)` is `$3.00` in `USD`.
    ///
    /// # Errors
    ///
    /// [`MoneyError::Overflow`] when the minor-unit count leaves `i64`.
    pub fn from_major(major: i64) -> Result<Self, MoneyError> {
        major
            .checked_mul(scale_for(C::EXPONENT))
            .map(Self::from_minor)
            .ok_or(MoneyError::Overflow)
    }

    /// Add two amounts in the same currency.
    ///
    /// # Errors
    ///
    /// [`MoneyError::Overflow`] when the sum leaves `i64`.
    pub fn checked_add(self, other: Self) -> Result<Self, MoneyError> {
        self.minor
            .checked_add(other.minor)
            .map(Self::from_minor)
            .ok_or(MoneyError::Overflow)
    }

    /// Subtract two amounts in the same currency.
    ///
    /// # Errors
    ///
    /// [`MoneyError::Overflow`] when the difference leaves `i64`.
    pub fn checked_sub(self, other: Self) -> Result<Self, MoneyError> {
        self.minor
            .checked_sub(other.minor)
            .map(Self::from_minor)
            .ok_or(MoneyError::Overflow)
    }

    /// Reverse the sign.
    ///
    /// # Errors
    ///
    /// [`MoneyError::Overflow`] for `i64::MIN`, which has no positive form.
    pub fn checked_neg(self) -> Result<Self, MoneyError> {
        self.minor
            .checked_neg()
            .map(Self::from_minor)
            .ok_or(MoneyError::Overflow)
    }

    /// Drop the sign.
    ///
    /// # Errors
    ///
    /// [`MoneyError::Overflow`] for `i64::MIN`.
    pub fn checked_abs(self) -> Result<Self, MoneyError> {
        self.minor
            .checked_abs()
            .map(Self::from_minor)
            .ok_or(MoneyError::Overflow)
    }

    /// Multiply by a whole number, for example a quantity.
    ///
    /// # Errors
    ///
    /// [`MoneyError::Overflow`] when the product leaves `i64`.
    pub fn checked_mul(self, factor: i64) -> Result<Self, MoneyError> {
        self.minor
            .checked_mul(factor)
            .map(Self::from_minor)
            .ok_or(MoneyError::Overflow)
    }

    /// Add up amounts. An empty input gives [`Money::ZERO`].
    ///
    /// # Errors
    ///
    /// [`MoneyError::Overflow`] when a partial sum leaves `i64`.
    pub fn try_sum<I: IntoIterator<Item = Self>>(values: I) -> Result<Self, MoneyError> {
        values.into_iter().try_fold(Self::ZERO, Self::checked_add)
    }

    /// True when the amount is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.minor == 0
    }

    /// True when the amount is below zero.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.minor < 0
    }

    /// True when the amount is above zero.
    #[must_use]
    pub const fn is_positive(self) -> bool {
        self.minor > 0
    }

    /// The same amount, with the currency carried as data.
    #[must_use]
    pub fn to_any(self) -> AnyMoney {
        AnyMoney {
            minor: self.minor,
            currency: C::currency(),
        }
    }

    /// The amount as a decimal in major units. `1250` cents becomes `12.50`.
    #[must_use]
    pub fn to_decimal(self) -> Decimal {
        Decimal::new(self.minor, C::EXPONENT)
    }

    /// Convert from a decimal in major units, rounding as told.
    ///
    /// # Errors
    ///
    /// [`MoneyError::Overflow`] when the minor-unit count leaves `i64`.
    pub fn from_decimal(value: Decimal, rounding: Rounding) -> Result<Self, MoneyError> {
        let scale = Decimal::from(scale_for(C::EXPONENT));
        let scaled = value.checked_mul(scale).ok_or(MoneyError::Overflow)?;
        scaled
            .round_dp_with_strategy(0, rounding.strategy())
            .to_i64()
            .map(Self::from_minor)
            .ok_or(MoneyError::Overflow)
    }

    /// Convert from a decimal in major units, and refuse to round.
    ///
    /// # Errors
    ///
    /// [`MoneyError::Inexact`] when the value has more precision than the
    /// currency, and [`MoneyError::Overflow`] when it leaves `i64`.
    pub fn from_decimal_exact(value: Decimal) -> Result<Self, MoneyError> {
        let converted = Self::from_decimal(value, Rounding::TowardZero)?;
        if converted.to_decimal() == value.normalize() {
            Ok(converted)
        } else {
            Err(MoneyError::Inexact {
                value: value.to_string(),
                currency: C::CODE,
            })
        }
    }

    /// Split the amount by `weights`, and lose nothing.
    ///
    /// The parts always sum back to `self`. Each part is the largest whole
    /// number of minor units at or below its exact share; the minor units left
    /// over go one each to the parts with the largest remainders, lowest index
    /// first. This is the largest-remainder method.
    ///
    /// ```rust
    /// use autumn_web::money::{Money, Usd};
    ///
    /// // A dollar in three does not divide.
    /// let parts = Money::<Usd>::from_minor(100).allocate(&[1, 1, 1])?;
    /// assert_eq!(
    ///     parts.iter().map(|p| p.minor()).collect::<Vec<_>>(),
    ///     vec![34, 33, 33]
    /// );
    /// assert_eq!(Money::<Usd>::try_sum(parts)?.minor(), 100);
    /// # Ok::<(), autumn_web::money::MoneyError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`MoneyError::InvalidWeights`] when `weights` is empty, holds a negative
    /// weight, or sums to zero. [`MoneyError::Overflow`] when the weights sum
    /// past `i64`.
    pub fn allocate(self, weights: &[i64]) -> Result<Vec<Self>, MoneyError> {
        if weights.is_empty() || weights.iter().any(|weight| *weight < 0) {
            return Err(MoneyError::InvalidWeights);
        }
        let total_weight: i64 = weights
            .iter()
            .try_fold(0_i64, |sum, weight| sum.checked_add(*weight))
            .ok_or(MoneyError::Overflow)?;
        if total_weight == 0 {
            return Err(MoneyError::InvalidWeights);
        }

        // i128 so the numerator cannot overflow: an i64 amount times an i64
        // weight needs 128 bits.
        let amount = i128::from(self.minor);
        let divisor = i128::from(total_weight);

        // `div_euclid` floors, so the remainder is never negative and the same
        // distribution step serves a negative amount.
        let mut shares: Vec<(i64, i128)> = Vec::with_capacity(weights.len());
        let mut floor_total: i128 = 0;
        for weight in weights {
            let numerator = amount
                .checked_mul(i128::from(*weight))
                .ok_or(MoneyError::Overflow)?;
            let share = numerator.div_euclid(divisor);
            let remainder = numerator.rem_euclid(divisor);
            floor_total = floor_total.checked_add(share).ok_or(MoneyError::Overflow)?;
            let share = i64::try_from(share).map_err(|_| MoneyError::Overflow)?;
            shares.push((share, remainder));
        }

        // Whatever flooring held back, handed out one minor unit at a time.
        let mut leftover = amount
            .checked_sub(floor_total)
            .ok_or(MoneyError::Overflow)?;
        let mut order: Vec<usize> = (0..shares.len()).collect();
        order.sort_by(|left, right| {
            let left_remainder = shares.get(*left).map_or(0, |share| share.1);
            let right_remainder = shares.get(*right).map_or(0, |share| share.1);
            right_remainder.cmp(&left_remainder).then(left.cmp(right))
        });
        for index in order {
            if leftover <= 0 {
                break;
            }
            if let Some(share) = shares.get_mut(index) {
                share.0 = share.0.checked_add(1).ok_or(MoneyError::Overflow)?;
                leftover = leftover.saturating_sub(1);
            }
        }

        Ok(shares
            .into_iter()
            .map(|(share, _)| Self::from_minor(share))
            .collect())
    }

    /// Split the amount into `parts` equal shares. Sugar for
    /// [`Money::allocate`] with equal weights.
    ///
    /// # Errors
    ///
    /// The same as [`Money::allocate`]. `parts` of zero is
    /// [`MoneyError::InvalidWeights`].
    pub fn split(self, parts: usize) -> Result<Vec<Self>, MoneyError> {
        self.allocate(&vec![1_i64; parts])
    }
}

impl<C: Currency> PartialEq for Money<C> {
    fn eq(&self, other: &Self) -> bool {
        self.minor == other.minor
    }
}
impl<C: Currency> Eq for Money<C> {}
impl<C: Currency> PartialOrd for Money<C> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<C: Currency> Ord for Money<C> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.minor.cmp(&other.minor)
    }
}
impl<C: Currency> core::hash::Hash for Money<C> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.minor.hash(state);
        C::CODE.hash(state);
    }
}
impl<C: Currency> Default for Money<C> {
    fn default() -> Self {
        Self::ZERO
    }
}
impl<C: Currency> fmt::Debug for Money<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Money({} {})", self.minor, C::CODE)
    }
}
impl<C: Currency> fmt::Display for Money<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_amount(f, self.minor, C::EXPONENT)?;
        write!(f, " {}", C::CODE)
    }
}

/// Write `minor` as a major-unit decimal, with exactly `exponent` fraction
/// digits.
///
/// `unsigned_abs` rather than `abs`, because `i64::MIN` has no positive form.
fn write_amount(f: &mut fmt::Formatter<'_>, minor: i64, exponent: u32) -> fmt::Result {
    let scale = scale_for(exponent).unsigned_abs();
    let magnitude = minor.unsigned_abs();
    let whole = magnitude.checked_div(scale).unwrap_or(magnitude);
    let fraction = magnitude.checked_rem(scale).unwrap_or(0);
    if minor < 0 {
        f.write_str("-")?;
    }
    write!(f, "{whole}")?;
    if exponent > 0 {
        write!(f, ".{fraction:0width$}", width = exponent as usize)?;
    }
    Ok(())
}

// ── AnyMoney ────────────────────────────────────────────────────────────────

/// An amount whose currency is known only at run time.
///
/// The form a ledger row takes. Operations that [`Money<C>`] rejects at compile
/// time, this rejects with [`MoneyError::CurrencyMismatch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AnyMoney {
    minor: i64,
    currency: CurrencyCode,
}

impl AnyMoney {
    /// Build from a count of minor units and a currency.
    #[must_use]
    pub const fn new(minor: i64, currency: CurrencyCode) -> Self {
        Self { minor, currency }
    }

    /// Nothing, in `currency`.
    #[must_use]
    pub const fn zero(currency: CurrencyCode) -> Self {
        Self { minor: 0, currency }
    }

    /// The count of minor units.
    #[must_use]
    pub const fn minor(self) -> i64 {
        self.minor
    }

    /// This value's currency.
    #[must_use]
    pub const fn currency(self) -> CurrencyCode {
        self.currency
    }

    /// True when the amount is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.minor == 0
    }

    /// True when the amount is below zero.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.minor < 0
    }

    /// The amount as a decimal in major units.
    #[must_use]
    pub fn to_decimal(self) -> Decimal {
        Decimal::new(self.minor, self.currency.exponent)
    }

    /// Add two amounts.
    ///
    /// # Errors
    ///
    /// [`MoneyError::CurrencyMismatch`] when the currencies differ, and
    /// [`MoneyError::Overflow`] when the sum leaves `i64`.
    pub fn checked_add(self, other: Self) -> Result<Self, MoneyError> {
        self.require_same_currency(other)?;
        self.minor
            .checked_add(other.minor)
            .map(|minor| Self::new(minor, self.currency))
            .ok_or(MoneyError::Overflow)
    }

    /// Subtract two amounts.
    ///
    /// # Errors
    ///
    /// The same as [`AnyMoney::checked_add`].
    pub fn checked_sub(self, other: Self) -> Result<Self, MoneyError> {
        self.require_same_currency(other)?;
        self.minor
            .checked_sub(other.minor)
            .map(|minor| Self::new(minor, self.currency))
            .ok_or(MoneyError::Overflow)
    }

    /// Reverse the sign.
    ///
    /// # Errors
    ///
    /// [`MoneyError::Overflow`] for `i64::MIN`.
    pub fn checked_neg(self) -> Result<Self, MoneyError> {
        self.minor
            .checked_neg()
            .map(|minor| Self::new(minor, self.currency))
            .ok_or(MoneyError::Overflow)
    }

    /// Add up amounts, all in `currency`. An empty input gives zero.
    ///
    /// # Errors
    ///
    /// The same as [`AnyMoney::checked_add`].
    pub fn try_sum<I: IntoIterator<Item = Self>>(
        currency: CurrencyCode,
        values: I,
    ) -> Result<Self, MoneyError> {
        values
            .into_iter()
            .try_fold(Self::zero(currency), |total, value| {
                total.checked_add(value)
            })
    }

    /// Recover the typed form.
    ///
    /// # Errors
    ///
    /// [`MoneyError::CurrencyMismatch`] when this value is not in `C`.
    pub fn try_into_typed<C: Currency>(self) -> Result<Money<C>, MoneyError> {
        if self.currency.code == C::CODE {
            Ok(Money::from_minor(self.minor))
        } else {
            Err(MoneyError::CurrencyMismatch {
                expected: C::CODE,
                found: self.currency.code,
            })
        }
    }

    fn require_same_currency(self, other: Self) -> Result<(), MoneyError> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(MoneyError::CurrencyMismatch {
                expected: self.currency.code,
                found: other.currency.code,
            })
        }
    }
}

impl<C: Currency> From<Money<C>> for AnyMoney {
    fn from(value: Money<C>) -> Self {
        value.to_any()
    }
}

impl fmt::Display for AnyMoney {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_amount(f, self.minor, self.currency.exponent)?;
        write!(f, " {}", self.currency.code)
    }
}
