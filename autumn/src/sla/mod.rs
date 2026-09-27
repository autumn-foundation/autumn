//! Calendar-aware SLA obligations (issue #1826).
//!
//! An obligation is a deadline in business time: "respond within 2 business
//! days". The clock of an obligation runs only in the working time of a
//! [`BusinessCalendar`]. It stops on weekends, on holidays and outside working
//! hours.
//!
//! The engine reads time only from the injected clock. It does not read the
//! system clock. Thus a test can move a quarter of business time forward in
//! less than one second.
//!
//! # Parts
//!
//! - [`BusinessCalendar`]: working hours, weekends and holidays.
//! - [`BusinessDuration`]: a budget such as `"2 business days"`.
//! - [`Obligation`]: a deadline on one subject (for example, one ticket).
//! - [`ObligationStatus`]: the remaining budget and the breach state.
//! - [`SlaPlugin`]: installs the calendars, the store and the breach handlers.
//! - [`Sla`]: the handler extractor. Use it to track, meet and read obligations.
//! - [`SlaBreach`]: the typed escalation payload.
//!
//! # Example
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use autumn_web::sla::{BusinessCalendar, Obligation, Sla, SlaPlugin, WorkingHours};
//!
//! #[post("/tickets/{id}")]
//! async fn open_ticket(sla: Sla, Path(id): Path<i64>) -> AutumnResult<String> {
//!     let obligation = Obligation::new("first_response", format!("ticket:{id}"))
//!         .within("2 business days".parse()?)
//!         .calendar("support");
//!     let status = sla.track(&obligation).await?;
//!     Ok(format!("due at {:?}", status.due_at))
//! }
//!
//! # fn plugin() -> Result<SlaPlugin, autumn_web::sla::SlaError> {
//! let plugin = SlaPlugin::new()
//!     .calendar("support", BusinessCalendar::weekdays("09:00-17:00".parse()?))
//!     .on_breach("first_response", |_state, breach| async move {
//!         tracing::warn!(key = %breach.key, "first response is late");
//!         Ok(())
//!     });
//! # Ok(plugin) }
//! ```
//!
//! # Exactly once
//!
//! [`Sla::track`] puts a check job on the job queue at the deadline. At the
//! deadline the check job reads the store. If the obligation was not met in
//! time, it claims the escalation in the store. Then it puts one
//! [`ESCALATE_JOB`] on the queue. The claim is the lock: a second check does
//! not claim again. The unique key of [`ESCALATE_JOB`] stops a duplicate
//! while one is on the queue.
//!
//! The default [`MemoryObligationStore`] is local to one process. For more than
//! one replica, all replicas must use one shared [`ObligationStore`].
//!
//! See `docs/guide/sla.md` for the guide.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]
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

mod calendar;
mod duration;
mod obligation;
mod runtime;
mod store;

#[cfg(test)]
mod tests;

pub use calendar::{BusinessCalendar, WorkingHours};
pub use duration::BusinessDuration;
pub use obligation::{Obligation, ObligationState, ObligationStatus, ObligationZone};
pub use runtime::{CHECK_JOB, ESCALATE_JOB, Sla, SlaBreach, SlaPlugin};
pub use store::{MemoryObligationStore, ObligationRecord, ObligationStore, StoreFuture};

/// An error from the SLA engine.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlaError {
    /// The text is not a valid working-hours window, such as `"09:00-17:00"`.
    InvalidHours(String),
    /// The text is not a valid business duration, such as `"2 business days"`.
    InvalidDuration(String),
    /// No calendar has this name. Add it with [`SlaPlugin::calendar`].
    UnknownCalendar(String),
    /// The obligation has no deadline in one hundred years. The calendar has
    /// no working time, or the budget is too large.
    NoDeadline(String),
    /// The app has no [`SlaPlugin`].
    NotInstalled,
    /// The app has no job runtime, so a check job cannot go on the queue.
    NoJobRuntime,
    /// The job backend did not take a job.
    Job(String),
    /// The [`ObligationStore`] failed.
    Store(String),
}

impl std::fmt::Display for SlaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidHours(text) => {
                write!(f, "invalid working hours {text:?}: use \"HH:MM-HH:MM\"")
            }
            Self::InvalidDuration(text) => write!(
                f,
                "invalid business duration {text:?}: use a form such as \"2 business days\""
            ),
            Self::UnknownCalendar(name) => write!(
                f,
                "unknown business calendar {name:?}: add it with SlaPlugin::calendar"
            ),
            Self::NoDeadline(key) => write!(
                f,
                "obligation {key:?} has no deadline: check the calendar and the budget"
            ),
            Self::NotInstalled => f.write_str("SLA engine is not installed: add SlaPlugin"),
            Self::NoJobRuntime => {
                f.write_str("SLA engine needs the job runtime to schedule a breach check")
            }
            Self::Job(message) => write!(f, "SLA job enqueue failed: {message}"),
            Self::Store(message) => write!(f, "obligation store failed: {message}"),
        }
    }
}

impl std::error::Error for SlaError {}
