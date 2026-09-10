//! Provider-neutral mirror records.
//!
//! The provider stays the source of truth. These records are the local,
//! read-optimized copy the gate and the routes read.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::money::Money;
use crate::plan::PlanId;

/// An opaque identifier issued by the provider (`cus_…`, `sub_…`, `price_…`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    /// Build a provider id.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProviderId {
    fn from(id: &str) -> Self {
        Self::new(id)
    }
}

impl From<String> for ProviderId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Lifecycle status of a subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SubscriptionStatus {
    /// Created, first payment not complete.
    Incomplete,
    /// First payment window expired.
    IncompleteExpired,
    /// In a trial period.
    Trialing,
    /// Paid and current.
    Active,
    /// Collection paused.
    Paused,
    /// The latest invoice failed; dunning in progress.
    PastDue,
    /// Dunning exhausted; not paid.
    Unpaid,
    /// Ended.
    Canceled,
}

impl SubscriptionStatus {
    /// Stable `snake_case` name (also the stored form).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Incomplete => "incomplete",
            Self::IncompleteExpired => "incomplete_expired",
            Self::Trialing => "trialing",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::PastDue => "past_due",
            Self::Unpaid => "unpaid",
            Self::Canceled => "canceled",
        }
    }

    /// Parse the stored form.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "incomplete" => Self::Incomplete,
            "incomplete_expired" => Self::IncompleteExpired,
            "trialing" => Self::Trialing,
            "active" => Self::Active,
            "paused" => Self::Paused,
            "past_due" => Self::PastDue,
            "unpaid" => Self::Unpaid,
            "canceled" => Self::Canceled,
            _ => return None,
        })
    }

    /// Precedence for same-instant events: a higher rank never yields to a
    /// lower one. Terminal states rank highest.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Incomplete => 0,
            Self::Paused => 1,
            Self::Trialing => 2,
            Self::Active => 3,
            Self::PastDue => 4,
            Self::Unpaid => 5,
            Self::IncompleteExpired => 6,
            Self::Canceled => 7,
        }
    }

    /// `true` for a subscription the provider still bills (not ended).
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(
            self,
            Self::Trialing | Self::Active | Self::PastDue | Self::Paused | Self::Incomplete
        )
    }

    /// `true` for a state that cannot be left by a later event on the same
    /// subscription.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Canceled | Self::IncompleteExpired)
    }
}

/// Status of an invoice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InvoiceStatus {
    /// Not finalized.
    Draft,
    /// Awaiting payment.
    Open,
    /// Paid in full.
    Paid,
    /// Will not be paid.
    Uncollectible,
    /// Voided.
    Void,
}

impl InvoiceStatus {
    /// Stable `snake_case` name (also the stored form).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Open => "open",
            Self::Paid => "paid",
            Self::Uncollectible => "uncollectible",
            Self::Void => "void",
        }
    }

    /// Parse the stored form.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "draft" => Self::Draft,
            "open" => Self::Open,
            "paid" => Self::Paid,
            "uncollectible" => Self::Uncollectible,
            "void" => Self::Void,
            _ => return None,
        })
    }

    /// Precedence for same-instant events.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Draft => 0,
            Self::Open => 1,
            Self::Uncollectible => 2,
            Self::Void => 3,
            Self::Paid => 4,
        }
    }
}

/// A mirrored customer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Customer {
    /// Local id (UUID string).
    pub id: String,
    /// Application user id from the session, when linked.
    pub user_id: Option<String>,
    /// Provider name (`stripe`).
    pub provider: String,
    /// Provider customer id.
    pub provider_customer_id: ProviderId,
    /// Email known to the provider.
    pub email: Option<String>,
    /// Creation time (app clock).
    pub created_at: DateTime<Utc>,
    /// Last update time (app clock).
    pub updated_at: DateTime<Utc>,
}

/// A mirrored subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Subscription {
    /// Local id (UUID string).
    pub id: String,
    /// Local customer id.
    pub customer_id: String,
    /// Provider subscription id.
    pub provider_subscription_id: ProviderId,
    /// Provider price id of the first item.
    pub provider_price_id: Option<ProviderId>,
    /// Catalog plan resolved from the price id, when known.
    pub plan_id: Option<PlanId>,
    /// Status.
    pub status: SubscriptionStatus,
    /// Seat quantity.
    pub quantity: i64,
    /// End of the current billing period.
    pub current_period_end: Option<DateTime<Utc>>,
    /// Cancels at period end.
    pub cancel_at_period_end: bool,
    /// Provider time of the last event applied.
    pub last_event_at: DateTime<Utc>,
    /// Creation time (app clock).
    pub created_at: DateTime<Utc>,
    /// Last update time (app clock).
    pub updated_at: DateTime<Utc>,
}

/// A mirrored invoice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Invoice {
    /// Local id (UUID string).
    pub id: String,
    /// Local customer id.
    pub customer_id: String,
    /// Local subscription id, when the invoice belongs to one.
    pub subscription_id: Option<String>,
    /// Provider invoice id.
    pub provider_invoice_id: ProviderId,
    /// Status.
    pub status: InvoiceStatus,
    /// Amount due.
    pub amount_due: Money,
    /// Amount paid.
    pub amount_paid: Money,
    /// Provider payment attempts so far.
    pub attempt_count: i64,
    /// Provider's own next attempt time, when reported.
    pub next_payment_attempt: Option<DateTime<Utc>>,
    /// Provider time of the last event applied.
    pub last_event_at: DateTime<Utc>,
    /// Creation time (app clock).
    pub created_at: DateTime<Utc>,
    /// Last update time (app clock).
    pub updated_at: DateTime<Utc>,
}

/// State of a dunning schedule row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DunningState {
    /// A retry is scheduled at `next_attempt_at`.
    Pending,
    /// A retry is in flight.
    Running,
    /// The invoice was paid.
    Recovered,
    /// All retries failed.
    Exhausted,
    /// The subscription ended before recovery.
    Canceled,
}

impl DunningState {
    /// Stable `snake_case` name (also the stored form).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Recovered => "recovered",
            Self::Exhausted => "exhausted",
            Self::Canceled => "canceled",
        }
    }

    /// Parse the stored form.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "recovered" => Self::Recovered,
            "exhausted" => Self::Exhausted,
            "canceled" => Self::Canceled,
            _ => return None,
        })
    }
}

/// The dunning schedule for one invoice. The store row is the truth; the
/// retry job only reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DunningAttempt {
    /// Local invoice id (primary key).
    pub invoice_id: String,
    /// Local customer id.
    pub customer_id: String,
    /// Local subscription id.
    pub subscription_id: Option<String>,
    /// Number of the next retry to run (1-based).
    pub attempt: i64,
    /// When the next retry is due.
    pub next_attempt_at: DateTime<Utc>,
    /// State.
    pub state: DunningState,
    /// Last update time (app clock).
    pub updated_at: DateTime<Utc>,
}
