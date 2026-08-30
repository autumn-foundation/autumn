//! Compile-time data classification (issue #1654).
//!
//! Autumn's other protections for sensitive data are *name*-based and run at
//! runtime: [`log::filter`](crate::log::filter) scrubs a key denylist,
//! [`http_client`](crate::http_client) redacts three header names,
//! [`gdpr`](crate::gdpr) keys erasure off table-name strings. Every one of them
//! is one rename away from silently letting personal data through, because the
//! classification lives in the developer's memory rather than in a type.
//!
//! This module moves one classification tier -- **personal data** -- into the
//! type system, and gates one sink -- the [`Json`](crate::extract::Json)
//! response -- on it.
//!
//! # The shape of the guarantee
//!
//! A `#[model]` column annotated `#[classified]` is generated as
//! [`Classified<String, Marker>`](Classified) instead of `String`. The wrapper
//! has **no** [`Serialize`](serde::Serialize) impl, no `Display`, no `Deref` and
//! no `into_inner`, so there is no expression that puts the value somewhere a
//! serializer can reach it. The model itself consequently loses its `Serialize`
//! derive, so the whole record cannot be handed to a sink either. The only way
//! out of the wrapper is [`Classified::declassify`], which consumes the value
//! (move semantics make a release a single event) and takes a
//! [`Declassification`] -- a boundary that was *declared*, names a purpose and a
//! reason, and is registered into the build-time data-flow manifest.
//!
//! ```ignore
//! #[autumn_web::model(table = "customers")]
//! pub struct Customer {
//!     pub id: i32,
//!     pub name: String,
//!     #[classified]
//!     pub email: String,
//! }
//!
//! autumn_web::declassify! {
//!     /// Support agents need the customer's email address to answer the ticket.
//!     pub SUPPORT_LOOKUP: CustomerEmailClassified => JsonResponse,
//!     purpose = "support_lookup",
//!     reason = "Support agents need the email address to answer the ticket.",
//! }
//!
//! // Json(customer)                       -- compile error: the model is classified
//! // Json(View { email: customer.email }) -- compile error: the field is classified
//! let email = customer.email.declassify(&SUPPORT_LOOKUP); // released, recorded
//! ```
//!
//! # What this is not
//!
//! The threat model is **drift detection, not an adversarial author** -- the
//! same posture `docs/guide/security-posture-manifest.md` states for the
//! security manifest. Two things follow from that, and both are deliberate:
//! an author who reaches for [`Declassification::__declare`] directly instead
//! of [`declassify!`] is lying to their own manifest, and the boolean surface
//! the wrapper must keep ([`PartialEq`], the `validator` rules) is an oracle
//! someone could loop over. Neither is reachable by accident, and an author who
//! wanted the value could simply declare a boundary. What this module closes is
//! every path that hands a classified value -- or a serializable view of one --
//! to a sink *without anyone meaning to*.
//!
//! See `docs/guide/data-classification.md`.

pub mod manifest;

#[cfg(feature = "db")]
mod diesel_types;
#[cfg(feature = "db")]
pub use diesel_types::ClassifiedText;

mod validate;

use std::marker::PhantomData;
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// A data classification tier.
///
/// The first slice supports exactly one tier on purpose (issue #1654 "Out of
/// Scope"): a second tier is a second variant here plus a second `#[classified]`
/// spelling, and nothing else in the design changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Classification {
    /// Data identifying, or attributable to, a natural person.
    PersonalData,
}

impl Classification {
    /// The stable manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PersonalData => "personal_data",
        }
    }
}

impl std::fmt::Display for Classification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A place classified data can leave the process.
///
/// The first slice gates the JSON response serializer only. Log/tracing events,
/// outbound HTTP bodies and analytics emission are follow-up slices; each is a
/// new variant here plus a new marker trait alongside [`JsonSink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Sink {
    /// The [`Json`](crate::extract::Json) response body.
    JsonResponse,
}

impl Sink {
    /// The stable manifest spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JsonResponse => "json_response",
        }
    }

    /// Every sink this slice can prove about, in manifest order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::JsonResponse]
    }
}

impl std::fmt::Display for Sink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The identity of one classified `#[model]` column, as a type.
///
/// `#[model]` generates one zero-sized marker per `#[classified]` column
/// (`Customer::email` becomes `CustomerEmailClassified`) and implements this trait on
/// it. The marker does three jobs at once: it names the field inside the
/// compiler diagnostic when a leak is attempted, it keys a
/// [`Declassification`] to exactly one column, and it turns a release into a
/// *build-time* manifest edge rather than a runtime observation.
pub trait ClassifiedField: 'static {
    /// The model type's name, e.g. `"Customer"`.
    const MODEL: &'static str;
    /// The column's Rust field name, e.g. `"email"`.
    const FIELD: &'static str;
    /// The tier the column was annotated with.
    const CLASSIFICATION: Classification;
}

mod sealed {
    /// Sealed: outside this crate there is no way to name, let alone implement,
    /// this trait, so [`super::ReleasedForSink`] cannot be implemented either.
    pub trait NeverReleased {}
}

/// The proof obligation a classified value can never discharge.
///
/// [`Classified<T, F>`](Classified) implements [`Serialize`] only for an `F`
/// that implements this trait, and nothing implements it. The impl exists at all
/// so the failure is reported *with autumn's own diagnostic*, naming the field,
/// rather than as serde's bare "trait bound not satisfied".
///
/// **Sealed.** The field markers `#[model]` generates live in the *application's*
/// crate, so without the private supertrait one safe line
/// (`impl ReleasedForSink for CustomerEmailClassified {}`) would satisfy the orphan
/// rule and turn the `Serialize` impl back on for that column -- silently, with
/// no boundary and no manifest row.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is classified personal data and cannot be serialized into a sink",
    label = "classified personal data would be released here",
    note = "autumn gates every sink (today: the `Json` response body) on data that has passed a declassification boundary",
    note = "declare a boundary with `autumn_web::declassify!` and release the value first: `value.declassify(&YOUR_BOUNDARY)`",
    note = "see docs/guide/data-classification.md"
)]
pub trait ReleasedForSink: sealed::NeverReleased {}

/// A value carrying a data classification.
///
/// `T` is the underlying value; `F` is the [`ClassifiedField`] marker naming the
/// `#[model]` column it came from. Deliberately missing: `Serialize`, `Display`,
/// `Deref`, `AsRef`, `Hash`, `into_inner`. The only exit is
/// [`declassify`](Self::declassify).
///
/// # Residual surface
///
/// [`PartialEq`] is kept because the generated repository code needs it (record
/// correlation, `UpdateDraft`'s changed-field check), and the `validator`
/// delegations answer their rules over the real value. Both are *boolean*
/// oracles: an author who writes a loop around `validate_regex` or `==` can
/// narrow the value down without a boundary. That is deliberate, and it is the
/// same posture `docs/guide/security-posture-manifest.md` states for the
/// security manifest -- these gates detect drift, not an author attacking their
/// own application, who could simply declare a boundary. What is closed is every
/// path that hands the value, or a serializable view of it, to a sink by
/// accident.
pub struct Classified<T, F: ClassifiedField> {
    value: T,
    field: PhantomData<fn() -> F>,
}

impl<T, F: ClassifiedField> Classified<T, F> {
    /// Classify a value.
    ///
    /// Entering the classification is free and unrecorded; only *leaving* it is
    /// a boundary.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            value,
            field: PhantomData,
        }
    }

    /// Release the value for the purpose the boundary declares.
    ///
    /// Consumes the classified value, so a release is a single event rather than
    /// a permanent widening, and emits the auditable
    /// [`DeclassificationRecord`].
    #[must_use = "declassifying releases personal data; use the released value or do not release it"]
    pub fn declassify(self, boundary: &Declassification<F>) -> T {
        record_release(&boundary.record());
        self.value
    }

    /// Release a *copy* of the value, leaving the classified original in place.
    ///
    /// Records exactly as [`declassify`](Self::declassify) does. Use it when the
    /// record is borrowed (a `&Customer` from a repository read) and moving the
    /// column out is not possible.
    #[must_use = "declassifying releases personal data; use the released value or do not release it"]
    pub fn declassify_cloned(&self, boundary: &Declassification<F>) -> T
    where
        T: Clone,
    {
        record_release(&boundary.record());
        self.value.clone()
    }

    /// The tier this column was classified at.
    #[must_use]
    pub const fn classification() -> Classification {
        F::CLASSIFICATION
    }

    /// The model this column belongs to.
    #[must_use]
    pub const fn model() -> &'static str {
        F::MODEL
    }

    /// The column's field name.
    #[must_use]
    pub const fn field() -> &'static str {
        F::FIELD
    }

    /// Borrow the value for a check that cannot copy it out.
    ///
    /// Crate-private on purpose: a public borrow would be an unaudited
    /// declassification for any `T: Copy` (and a `.clone()` away for the rest).
    /// The `validator` delegations in [`validate`](super::validate) are the only
    /// consumers.
    pub(crate) const fn inner(&self) -> &T {
        &self.value
    }

    /// Hand the value to the Diesel column wrapper on the way to the database.
    ///
    /// Crate-private, and its only caller is
    /// [`ClassifiedText`](crate::classify::ClassifiedText), which is itself
    /// opaque -- so this is not a way back to a `String`. Persisting a row is
    /// not a gated sink; handing the value to a serializer is.
    #[cfg(feature = "db")]
    pub(crate) fn into_column_value(self) -> T {
        self.value
    }
}

impl<T, F: ClassifiedField> From<T> for Classified<T, F> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: Clone, F: ClassifiedField> Clone for Classified<T, F> {
    fn clone(&self) -> Self {
        Self::new(self.value.clone())
    }
}

impl<T: Default, F: ClassifiedField> Default for Classified<T, F> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: PartialEq, F: ClassifiedField> PartialEq for Classified<T, F> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: Eq, F: ClassifiedField> Eq for Classified<T, F> {}

/// Redacted: the whole point is that the plaintext has no unaudited exit, and
/// `Debug` output reaches panic messages, error pages and logs.
impl<T, F: ClassifiedField> std::fmt::Debug for Classified<T, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<classified>")
    }
}

/// Deserialization is unrestricted: taking classified data *in* is what an
/// application does. Only releasing it is gated.
impl<'de, T: Deserialize<'de>, F: ClassifiedField> Deserialize<'de> for Classified<T, F> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::new)
    }
}

/// Unreachable by construction: nothing implements [`ReleasedForSink`].
///
/// The impl exists so that an attempted leak is reported against autumn's
/// diagnostic (which names the field and says what to do) instead of serde's.
impl<T: Serialize, F: ClassifiedField + ReleasedForSink> Serialize for Classified<T, F> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.value.serialize(serializer)
    }
}

/// A declared declassification boundary: one column, one sink, one purpose.
///
/// Declare it with [`declassify!`](crate::declassify), which is also what
/// registers the manifest edge. `F` ties the boundary to exactly one classified
/// column, so one column's approved purpose cannot release another's data.
pub struct Declassification<F: ClassifiedField> {
    purpose: &'static str,
    sink: Sink,
    reason: &'static str,
    field: PhantomData<fn() -> F>,
}

impl<F: ClassifiedField> Declassification<F> {
    /// Construct a boundary. **Internal**: use
    /// [`declassify!`](crate::declassify), which also registers the boundary in
    /// the data-flow manifest. A boundary built here releases data that the
    /// manifest will not list.
    #[doc(hidden)]
    #[must_use]
    pub const fn __declare(purpose: &'static str, sink: Sink, reason: &'static str) -> Self {
        Self {
            purpose,
            sink,
            reason,
            field: PhantomData,
        }
    }

    /// The declared purpose, e.g. `"support_lookup"`.
    #[must_use]
    pub const fn purpose(&self) -> &'static str {
        self.purpose
    }

    /// The sink the release is approved for.
    #[must_use]
    pub const fn sink(&self) -> Sink {
        self.sink
    }

    /// Why the release is justified, in the declarer's own words.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        self.reason
    }

    const fn record(&self) -> DeclassificationRecord {
        DeclassificationRecord {
            model: F::MODEL,
            field: F::FIELD,
            classification: F::CLASSIFICATION,
            purpose: self.purpose,
            sink: self.sink,
            reason: self.reason,
        }
    }
}

impl<F: ClassifiedField> std::fmt::Debug for Declassification<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Declassification")
            .field("model", &F::MODEL)
            .field("field", &F::FIELD)
            .field("purpose", &self.purpose)
            .field("sink", &self.sink)
            .finish_non_exhaustive()
    }
}

/// The auditable record one release emits.
///
/// Carries no value -- recording the released plaintext would reintroduce the
/// leak the type system just closed. It records *what* was released, *where to*
/// and *why*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclassificationRecord {
    /// The model the column belongs to.
    pub model: &'static str,
    /// The column's field name.
    pub field: &'static str,
    /// The tier the column was classified at.
    pub classification: Classification,
    /// The declared purpose of the release.
    pub purpose: &'static str,
    /// The sink the release was approved for.
    pub sink: Sink,
    /// The declarer's justification.
    pub reason: &'static str,
}

type Observer = Arc<dyn Fn(&DeclassificationRecord) + Send + Sync>;

fn observers() -> &'static Mutex<Vec<(u64, Observer)>> {
    static OBSERVERS: OnceLock<Mutex<Vec<(u64, Observer)>>> = OnceLock::new();
    OBSERVERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Removes the observer it was returned with when dropped.
#[derive(Debug)]
#[must_use = "the observer is removed as soon as this guard is dropped"]
pub struct ReleaseObserverGuard(u64);

impl Drop for ReleaseObserverGuard {
    fn drop(&mut self) {
        let mut list = observers()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        list.retain(|(id, _)| *id != self.0);
    }
}

/// Observe every declassification for as long as the guard lives.
///
/// Every release also emits a `tracing` event on the `autumn::declassification`
/// target; this hook is for applications that persist the record (an audit
/// table, a compliance export) and for tests.
pub fn capture_releases<F>(observer: F) -> ReleaseObserverGuard
where
    F: Fn(&DeclassificationRecord) + Send + Sync + 'static,
{
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Recover from poisoning rather than silently handing back a live-looking
    // guard for an observer that was never registered: the list holds only
    // `Arc`s, so a panic while another observer ran left nothing inconsistent.
    let mut list = observers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    list.push((id, Arc::new(observer)));
    drop(list);
    ReleaseObserverGuard(id)
}

fn record_release(record: &DeclassificationRecord) {
    tracing::info!(
        target: "autumn::declassification",
        model = record.model,
        field = record.field,
        classification = record.classification.as_str(),
        purpose = record.purpose,
        sink = record.sink.as_str(),
        reason = record.reason,
        "declassified",
    );
    // Clone the handles out before calling any of them: an observer that
    // declassifies (or installs another observer) would otherwise deadlock on
    // the same lock.
    let snapshot: Vec<Observer> = observers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|(_, o)| Arc::clone(o))
        .collect();
    for observer in snapshot {
        observer(record);
    }
}

/// The gate on the [`Json`](crate::extract::Json) response sink.
///
/// Blanket-implemented for everything that serializes, so no ordinary handler
/// notices it exists. A `#[model]` with a `#[classified]` column has no
/// `Serialize` impl and therefore no `JsonSink` impl, which is what turns the
/// leak into a build failure.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be released into autumn's `Json` response sink",
    label = "classified personal data would be released here",
    note = "an autumn `#[model]` with `#[classified]` columns has no `Serialize` impl: the classification is carried by the type, so the whole record cannot reach a sink",
    note = "release each classified column at a declared boundary and respond with the released view, e.g. `Json(SupportView::from_released(customer.email.declassify(&YOUR_BOUNDARY)))`",
    note = "declare the boundary with `autumn_web::declassify!`; see docs/guide/data-classification.md"
)]
pub trait JsonSink: Serialize {}

// `do_not_recommend` keeps rustc from descending into the blanket impl and
// reporting serde's `T: Serialize` instead: the developer needs autumn's
// message, which says what to do about it.
#[diagnostic::do_not_recommend]
impl<T: Serialize + ?Sized> JsonSink for T {}

/// Declare a declassification boundary and register it in the data-flow
/// manifest.
///
/// ```ignore
/// autumn_web::declassify! {
///     /// Support agents need the customer's email address to answer the ticket.
///     pub SUPPORT_LOOKUP: CustomerEmailClassified => JsonResponse,
///     purpose = "support_lookup",
///     reason = "Support agents need the email address to answer the ticket.",
/// }
/// ```
///
/// The field marker (`CustomerEmailClassified`) is generated by `#[model]` for the
/// `#[classified]` column. The sink name is a [`Sink`] variant. Both the
/// `purpose` and the `reason` are string literals so the manifest can carry them
/// without running the app.
#[macro_export]
macro_rules! declassify {
    (
        $(#[$meta:meta])*
        $vis:vis $name:ident : $field:ty => $sink:ident,
        purpose = $purpose:literal,
        reason = $reason:literal $(,)?
    ) => {
        // A boundary whose justification is three spaces is the one nobody can
        // review -- the same BLANK rule `acknowledge_stale` enforces.
        const _: () = assert!(
            !$crate::classify::reason_is_blank($purpose),
            "declassify! requires a non-blank purpose",
        );
        const _: () = assert!(
            !$crate::classify::reason_is_blank($reason),
            "declassify! requires a non-blank reason",
        );

        $(#[$meta])*
        $vis static $name: $crate::classify::Declassification<$field> =
            $crate::classify::Declassification::__declare(
                $purpose,
                $crate::classify::Sink::$sink,
                $reason,
            );

        $crate::reexports::inventory::submit! {
            $crate::classify::manifest::DeclassificationDescriptor {
                model: <$field as $crate::classify::ClassifiedField>::MODEL,
                field: <$field as $crate::classify::ClassifiedField>::FIELD,
                classification: <$field as $crate::classify::ClassifiedField>::CLASSIFICATION,
                purpose: $purpose,
                sink: $crate::classify::Sink::$sink,
                reason: $reason,
            }
        }
    };
}

/// Whether a declared justification is blank (empty or all whitespace).
///
/// `const` so [`declassify!`] can reject it at compile time. Delegates to the
/// cache-coherence gate's rule rather than carrying a second copy: two spellings
/// of "blank" that could disagree is exactly the drift these gates exist to
/// catch. That one is Unicode-aware, so a non-breaking space is still blank.
#[doc(hidden)]
#[must_use]
pub const fn reason_is_blank(reason: &str) -> bool {
    crate::cache::coherence::reason_is_blank(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestField;
    impl ClassifiedField for TestField {
        const MODEL: &'static str = "Customer";
        const FIELD: &'static str = "email";
        const CLASSIFICATION: Classification = Classification::PersonalData;
    }

    /// A marker of its own for the observer tests. The observer registry is
    /// process-wide and the test binary runs these in parallel, so a test that
    /// counts releases has to be able to tell its own apart from a sibling's.
    struct ObservedField;
    impl ClassifiedField for ObservedField {
        const MODEL: &'static str = "ObserverCustomer";
        const FIELD: &'static str = "email";
        const CLASSIFICATION: Classification = Classification::PersonalData;
    }

    static OBSERVED_BOUNDARY: Declassification<ObservedField> = Declassification::__declare(
        "support_lookup",
        Sink::JsonResponse,
        "Support agents need the email address to answer the ticket.",
    );

    static BOUNDARY: Declassification<TestField> = Declassification::__declare(
        "support_lookup",
        Sink::JsonResponse,
        "Support agents need the email address to answer the ticket.",
    );

    #[test]
    fn debug_never_renders_the_classified_value() {
        let c: Classified<String, TestField> = "ada@example.com".to_string().into();
        assert_eq!(format!("{c:?}"), "<classified>");
    }

    #[test]
    fn declassify_returns_the_value_and_records_the_release() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let _guard = capture_releases(move |r: &DeclassificationRecord| {
            sink.lock().expect("observer").push(r.clone());
        });

        let c: Classified<String, ObservedField> = "ada@example.com".to_string().into();
        assert_eq!(c.declassify(&OBSERVED_BOUNDARY), "ada@example.com");

        let records = seen.lock().expect("observer").clone();
        let mine: Vec<_> = records
            .iter()
            .filter(|r| r.model == ObservedField::MODEL)
            .collect();
        assert_eq!(mine.len(), 1, "{records:?}");
        assert_eq!(mine[0].purpose, "support_lookup");
        assert_eq!(mine[0].sink, Sink::JsonResponse);
        assert_eq!(mine[0].classification, Classification::PersonalData);
    }

    #[test]
    fn declassify_cloned_leaves_the_original_classified() {
        let c: Classified<String, TestField> = "ada@example.com".to_string().into();
        assert_eq!(c.declassify_cloned(&BOUNDARY), "ada@example.com");
        // Still classified: the wrapper was borrowed, not consumed.
        assert_eq!(format!("{c:?}"), "<classified>");
    }

    #[test]
    fn the_observer_is_removed_when_its_guard_drops() {
        struct GuardField;
        impl ClassifiedField for GuardField {
            const MODEL: &'static str = "GuardCustomer";
            const FIELD: &'static str = "email";
            const CLASSIFICATION: Classification = Classification::PersonalData;
        }
        static GUARD_BOUNDARY: Declassification<GuardField> =
            Declassification::__declare("support_lookup", Sink::JsonResponse, "Because.");

        let seen = Arc::new(Mutex::new(0_usize));
        {
            let sink = Arc::clone(&seen);
            // Count only this test's own releases: the registry is process-wide.
            let _guard = capture_releases(move |r: &DeclassificationRecord| {
                if r.model == GuardField::MODEL {
                    *sink.lock().expect("observer") += 1;
                }
            });
            let c: Classified<String, GuardField> = "a".to_string().into();
            let _ = c.declassify(&GUARD_BOUNDARY);
            assert_eq!(*seen.lock().expect("observer"), 1);
        }
        let after_guard = *seen.lock().expect("observer");
        let c: Classified<String, GuardField> = "b".to_string().into();
        let _ = c.declassify(&GUARD_BOUNDARY);
        assert_eq!(*seen.lock().expect("observer"), after_guard);
    }

    #[test]
    fn deserialize_accepts_classified_data_but_serialize_is_absent() {
        let c: Classified<String, TestField> =
            serde_json::from_str("\"ada@example.com\"").expect("deserialize");
        assert_eq!(c.declassify(&BOUNDARY), "ada@example.com");
        // There is no `Serialize` impl to call -- pinned by the trybuild
        // fixtures in `tests/compile-fail/classified_*.rs`.
    }

    #[test]
    fn classification_and_identity_come_from_the_marker() {
        assert_eq!(
            Classified::<String, TestField>::classification(),
            Classification::PersonalData
        );
        assert_eq!(Classified::<String, TestField>::model(), "Customer");
        assert_eq!(Classified::<String, TestField>::field(), "email");
    }

    #[test]
    fn boundary_exposes_its_declaration() {
        assert_eq!(BOUNDARY.purpose(), "support_lookup");
        assert_eq!(BOUNDARY.sink(), Sink::JsonResponse);
        assert!(BOUNDARY.reason().contains("Support agents"));
    }

    #[test]
    fn equality_sees_through_the_wrapper_but_hashing_is_not_offered() {
        // `PartialEq` is what the generated repository correlation and
        // `UpdateDraft` changed-field check need. `Hash` is deliberately absent:
        // a stable digest of a low-entropy personal value is `Serialize`, and so
        // is an offline-brute-forceable view of the value itself.
        let a: Classified<String, TestField> = "x".to_string().into();
        let b: Classified<String, TestField> = "x".to_string().into();
        let c: Classified<String, TestField> = "y".to_string().into();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn unicode_whitespace_is_still_a_blank_justification() {
        assert!(reason_is_blank("\u{00a0}\u{2003}"));
    }

    #[test]
    fn blank_justifications_are_rejected() {
        assert!(reason_is_blank(""));
        assert!(reason_is_blank("   \t\n"));
        assert!(!reason_is_blank("support_lookup"));
    }

    #[test]
    fn stable_spellings() {
        assert_eq!(Classification::PersonalData.as_str(), "personal_data");
        assert_eq!(Sink::JsonResponse.as_str(), "json_response");
        assert_eq!(Sink::all(), &[Sink::JsonResponse]);
    }
}
