//! Obligations and their status.

use std::time::Duration;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use super::{BusinessCalendar, BusinessDuration};

/// A deadline in business time on one subject.
///
/// The key of an obligation is `"<name>/<subject>"`. It must be unique.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obligation {
    name: String,
    subject: String,
    within: BusinessDuration,
    calendar: String,
    zone: Option<Tz>,
    started_at: Option<DateTime<Utc>>,
    met_at: Option<DateTime<Utc>>,
}

impl Obligation {
    /// The calendar name that [`Obligation::new`] uses.
    pub const DEFAULT_CALENDAR: &'static str = "default";

    /// Make an obligation called `name` on `subject`.
    ///
    /// The budget is zero and the calendar is [`Self::DEFAULT_CALENDAR`]. The
    /// start is the time of [`Sla::track`](super::Sla::track).
    #[must_use]
    pub fn new(name: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            subject: subject.into(),
            within: BusinessDuration::ZERO,
            calendar: Self::DEFAULT_CALENDAR.to_owned(),
            zone: None,
            started_at: None,
            met_at: None,
        }
    }

    /// Set the budget.
    #[must_use]
    pub fn within(mut self, budget: BusinessDuration) -> Self {
        self.within = budget;
        self
    }

    /// Set the calendar name.
    #[must_use]
    pub fn calendar(mut self, name: impl Into<String>) -> Self {
        self.calendar = name.into();
        self
    }

    /// Set the time zone.
    #[must_use]
    pub fn zone(mut self, zone: Tz) -> Self {
        self.zone = Some(zone);
        self
    }

    /// Set the time zone from a value, such as an IANA name.
    ///
    /// A value that is not a time zone keeps the fallback zone.
    #[must_use]
    pub fn zone_from<Z: ObligationZone + ?Sized>(mut self, zone: &Z) -> Self {
        self.zone = zone.obligation_zone().or(self.zone);
        self
    }

    /// Set the start instant.
    #[must_use]
    pub fn starting_at(mut self, at: DateTime<Utc>) -> Self {
        self.started_at = Some(at);
        self
    }

    /// Set the instant when the subject met the obligation.
    #[must_use]
    pub fn met_at(mut self, at: impl Into<Option<DateTime<Utc>>>) -> Self {
        self.met_at = at.into();
        self
    }

    /// The obligation name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The subject, such as `"ticket:42"`.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// The unique key, `"<name>/<subject>"`.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}/{}", self.name, self.subject)
    }

    /// The budget.
    #[must_use]
    pub const fn budget(&self) -> BusinessDuration {
        self.within
    }

    /// The calendar name.
    #[must_use]
    pub fn calendar_name(&self) -> &str {
        &self.calendar
    }

    /// The time zone, if set.
    #[must_use]
    pub const fn time_zone(&self) -> Option<Tz> {
        self.zone
    }

    /// The start instant, if set.
    #[must_use]
    pub const fn started_at(&self) -> Option<DateTime<Utc>> {
        self.started_at
    }

    /// The met instant, if set.
    #[must_use]
    pub const fn met(&self) -> Option<DateTime<Utc>> {
        self.met_at
    }

    /// Calculate the status at `now` on `calendar` in `zone`.
    ///
    /// This is a pure function. An unset start is `now`.
    #[must_use]
    pub fn status_with(
        &self,
        calendar: &BusinessCalendar,
        zone: Tz,
        now: DateTime<Utc>,
    ) -> ObligationStatus {
        let started_at = self.started_at.unwrap_or(now);
        let budget = self.within.resolve(calendar);
        let due_at = calendar.deadline(started_at, budget, zone);
        // A met instant after `now` is not known yet.
        let met_at = self.met_at.filter(|met| *met <= now);
        let elapsed = calendar.working_time(started_at, met_at.unwrap_or(now), zone);
        let breached = match (due_at, met_at) {
            (Some(due), Some(met)) => met > due,
            (Some(due), None) => now >= due,
            (None, _) => false,
        };
        let state = if breached {
            ObligationState::Breached
        } else if met_at.is_some() {
            ObligationState::Met
        } else if calendar.is_working(now, zone) {
            ObligationState::Running
        } else {
            ObligationState::Paused
        };
        let resumes_at = (state == ObligationState::Paused)
            .then(|| calendar.next_working_instant(now, zone))
            .flatten();
        ObligationStatus {
            key: self.key(),
            state,
            zone,
            started_at,
            due_at,
            met_at,
            escalated_at: None,
            resumes_at,
            budget,
            elapsed,
            remaining: if breached {
                Duration::ZERO
            } else {
                budget.saturating_sub(elapsed)
            },
        }
    }

    /// Set the met instant in place.
    pub(super) const fn set_met(&mut self, at: DateTime<Utc>) {
        self.met_at = Some(at);
    }
}

/// The state of an obligation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObligationState {
    /// Open, and the clock runs now.
    Running,
    /// Open, and the clock stops now (outside working time).
    Paused,
    /// Met on or before the deadline.
    Met,
    /// The deadline is past and the obligation was not met in time.
    Breached,
}

impl ObligationState {
    /// Whether the state is [`Self::Breached`].
    #[must_use]
    pub const fn is_breached(self) -> bool {
        matches!(self, Self::Breached)
    }

    /// Whether the obligation is open ([`Self::Running`] or [`Self::Paused`]).
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Running | Self::Paused)
    }
}

/// The status of an obligation at one instant.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ObligationStatus {
    /// The unique key, `"<name>/<subject>"`.
    pub key: String,
    /// The state.
    pub state: ObligationState,
    /// The time zone that the calendar hours use.
    pub zone: Tz,
    /// The start instant.
    pub started_at: DateTime<Utc>,
    /// The deadline. `None` when the calendar has no working time.
    pub due_at: Option<DateTime<Utc>>,
    /// The met instant.
    pub met_at: Option<DateTime<Utc>>,
    /// The instant when the escalation was claimed.
    pub escalated_at: Option<DateTime<Utc>>,
    /// The next working instant while [`ObligationState::Paused`].
    pub resumes_at: Option<DateTime<Utc>>,
    /// The budget as working time.
    pub budget: Duration,
    /// The working time used.
    pub elapsed: Duration,
    /// The working time that is left. Zero when breached.
    pub remaining: Duration,
}

/// A value that can give the time zone of an obligation.
///
/// `#[obligation(zone = field)]` uses it, so the field can be a [`Tz`], an
/// IANA name, or an `Option` of one.
pub trait ObligationZone {
    /// The time zone, or `None` to use the fallback zone.
    fn obligation_zone(&self) -> Option<Tz>;
}

impl ObligationZone for Tz {
    fn obligation_zone(&self) -> Option<Tz> {
        Some(*self)
    }
}

impl ObligationZone for crate::time_zone::TimeZone {
    fn obligation_zone(&self) -> Option<Tz> {
        Some(self.tz())
    }
}

impl ObligationZone for str {
    fn obligation_zone(&self) -> Option<Tz> {
        crate::time_zone::parse_iana(self)
    }
}

impl ObligationZone for String {
    fn obligation_zone(&self) -> Option<Tz> {
        self.as_str().obligation_zone()
    }
}

impl<Z: ObligationZone> ObligationZone for Option<Z> {
    fn obligation_zone(&self) -> Option<Tz> {
        self.as_ref().and_then(ObligationZone::obligation_zone)
    }
}

impl<Z: ObligationZone + ?Sized> ObligationZone for &Z {
    fn obligation_zone(&self) -> Option<Tz> {
        (**self).obligation_zone()
    }
}
