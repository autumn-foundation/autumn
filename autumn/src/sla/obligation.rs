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
    /// Set the budget with [`within`](Self::within): [`Sla::track`](super::Sla::track)
    /// refuses a zero budget. The calendar is [`Self::DEFAULT_CALENDAR`]. The
    /// start is the time of `track`.
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
    pub const fn within(mut self, budget: BusinessDuration) -> Self {
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
    pub const fn zone(mut self, zone: Tz) -> Self {
        self.zone = Some(zone);
        self
    }

    /// Set the time zone from a value, such as an IANA name.
    ///
    /// If the value is not a valid time zone, the zone does not change.
    #[must_use]
    pub fn zone_from<Z: ObligationZone + ?Sized>(mut self, zone: &Z) -> Self {
        self.zone = zone.obligation_zone().or(self.zone);
        self
    }

    /// Set the start instant.
    #[must_use]
    pub const fn starting_at(mut self, at: DateTime<Utc>) -> Self {
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
        } else if now >= started_at && calendar.is_working(now, zone) {
            ObligationState::Running
        } else {
            // Outside working time, or the clock has not started yet.
            ObligationState::Paused
        };
        let resumes_at = (state == ObligationState::Paused)
            .then(|| calendar.next_working_instant(now.max(started_at), zone))
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

/// The state of an obligation. It serializes in `snake_case`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ObligationState {
    /// Open, and the clock runs now.
    Running,
    /// Open, and the clock stops now: outside working time, or before the
    /// start.
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
///
/// It serializes to JSON for an API. The zone is its IANA name. Durations
/// are whole seconds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct ObligationStatus {
    /// The unique key, `"<name>/<subject>"`.
    pub key: String,
    /// The state.
    pub state: ObligationState,
    /// The time zone that the calendar hours use.
    #[serde(serialize_with = "zone_name")]
    pub zone: Tz,
    /// The start instant.
    pub started_at: DateTime<Utc>,
    /// The deadline. `None` when there is no deadline in one hundred years.
    pub due_at: Option<DateTime<Utc>>,
    /// The met instant.
    pub met_at: Option<DateTime<Utc>>,
    /// The instant when the escalation was claimed.
    pub escalated_at: Option<DateTime<Utc>>,
    /// The next working instant while [`ObligationState::Paused`].
    pub resumes_at: Option<DateTime<Utc>>,
    /// The budget as working time.
    #[serde(serialize_with = "whole_seconds")]
    pub budget: Duration,
    /// The working time used.
    #[serde(serialize_with = "whole_seconds")]
    pub elapsed: Duration,
    /// The working time that is left. Zero when breached.
    #[serde(serialize_with = "whole_seconds")]
    pub remaining: Duration,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde `serialize_with` passes a reference"
)]
fn zone_name<S: serde::Serializer>(zone: &Tz, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(zone.name())
}

fn whole_seconds<S: serde::Serializer>(
    duration: &Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_u64(duration.as_secs())
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
