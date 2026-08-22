//! Translatable model fields — one column, one value per locale (issue #1384).
//!
//! Autumn's *request-side* i18n stack (Fluent `t!`, `Accept-Language`
//! negotiation, [`I18nConfig::resolved_fallback_chain`](super::I18nConfig::resolved_fallback_chain))
//! stops at the chrome: a `#[model]` field stores exactly one language. This
//! module is the data-layer half. A field declared `#[translatable]` holds a
//! [`Translated`] map — an independent value per locale tag — persisted as a
//! JSON object in the field's own `TEXT` column (Mobility's "container"
//! backend), and read back resolved against the request's active locale with
//! **no locale argument anywhere in the handler**.
//!
//! # Resolution
//!
//! [`Translated::resolve`] mirrors [`Bundle::translate`](super::Bundle)'s
//! algorithm exactly, so "active locale" means the same thing for content as
//! it does for UI strings — one mental model, not two:
//!
//! 1. the ambient request locale (see [`with_locale`]), matched exactly;
//! 2. each entry of the fallback chain in order, matched exactly;
//! 3. **absence** — the single documented sentinel. `resolve` returns `None`,
//!    [`Display`](std::fmt::Display) renders the empty string. Never a panic,
//!    never a 500.
//!
//! Every read goes through `tokio::task_local`'s `try_with`, so resolution
//! outside a request (a job worker, a CLI task, a unit test) falls back to the
//! process-wide chain installed at boot rather than panicking.
//!
//! # Storage
//!
//! The column stays `TEXT` on both Postgres and `SQLite` and holds
//! `{"en":"Hello","es":"Hola"}`. Two consequences worth knowing:
//!
//! * The migration is an ordinary `ADD COLUMN … TEXT NOT NULL DEFAULT '{}'`,
//!   which `autumn migrate check` already classifies as *safe* — translatable
//!   storage introduces no new operation type.
//! * A value that is **not** a JSON object of strings is decoded as the
//!   default locale's translation (see [`Translated::decode_column`]), so an
//!   existing plain-text column can be declared `#[translatable]` with no data
//!   backfill. The flip side is that a column already holding a JSON
//!   string-map of *something else* would be read as translations; declare
//!   `#[translatable]` only on columns that hold human-readable text.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Last-resort locale when nothing has been installed and no request scope is
/// in effect — matches [`I18nConfig::default`](super::I18nConfig)'s
/// `default_locale`.
///
/// Reads and writes both consult it, so a value written with no configuration
/// in effect is readable back under the same conditions. An earlier revision
/// used it only on the write side, which made every field render empty in an
/// app that declared `#[translatable]` columns without also loading a Fluent
/// bundle.
const LAST_RESORT_LOCALE: &str = "en";

// ── Ambient request locale ───────────────────────────────────────────────────

/// The locale resolution context in effect for the current task.
///
/// Carries both the active locale tag *and* the fallback chain to walk, so a
/// scope is self-contained: nothing about content resolution depends on
/// process-global mutable state once a request scope is established.
#[derive(Clone, Debug)]
pub struct LocaleScope {
    /// The active tag, behind a lock so a *deeper* resolution can refine it.
    ///
    /// The app-wide layer runs outside some middleware the [`Locale`](super::Locale)
    /// extractor reads — most visibly the session on the SSG/ISG path, where
    /// registered layers are applied outside the static-first middleware and so
    /// outside `SessionLayer`. Publishing the extractor's own answer onto the
    /// scope (mirroring [`Current::set_actor`](crate::current::Current::set_actor))
    /// keeps the ambient locale and a hand-taken `Locale` parameter from ever
    /// disagreeing on those paths.
    tag: Arc<RwLock<String>>,
    chain: Arc<[String]>,
}

impl LocaleScope {
    /// Build a scope for `tag` walking `chain` on miss.
    ///
    /// An empty `chain` falls back to the process-wide default so a scope can
    /// never resolve to "nothing at all".
    #[must_use]
    pub fn new(tag: impl Into<String>, chain: Vec<String>) -> Self {
        Self {
            tag: Arc::new(RwLock::new(tag.into())),
            chain: if chain.is_empty() {
                effective_chain()
            } else {
                chain.into()
            },
        }
    }

    /// Build a scope for `tag` walking the process-wide installed chain.
    #[must_use]
    pub fn for_tag(tag: impl Into<String>) -> Self {
        Self {
            tag: Arc::new(RwLock::new(tag.into())),
            chain: effective_chain(),
        }
    }

    /// The active locale tag.
    #[must_use]
    pub fn tag(&self) -> String {
        self.tag
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Refine the active locale tag on this scope.
    pub fn set_tag(&self, tag: impl Into<String>) {
        let mut guard = self
            .tag
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = tag.into();
    }

    /// The fallback chain walked when the active locale has no translation.
    #[must_use]
    pub fn chain(&self) -> &[String] {
        &self.chain
    }
}

tokio::task_local! {
    /// Locale resolution context for the current request/task. Established by
    /// [`AmbientLocaleLayer`](super::AmbientLocaleLayer) on the request path
    /// and by [`with_locale`] everywhere else.
    static ACTIVE_LOCALE: LocaleScope;
}

/// The process-wide resolution defaults, installed at boot from the loaded
/// [`Bundle`](super::Bundle). Read only when no request scope is in effect.
///
/// Both halves are behind `Arc` so a read is a refcount bump rather than a
/// deep clone: `FromSql` consults the default locale once per decoded value,
/// which on a list page is once per translatable column per row.
struct LocaleDefaults {
    /// The app's default locale — the locale a write with no ambient scope
    /// lands in, and the one a legacy plain-text column is attributed to.
    default_locale: Arc<str>,
    /// The chain walked on a resolution miss.
    chain: Arc<[String]>,
}

static DEFAULTS: RwLock<Option<LocaleDefaults>> = RwLock::new(None);

/// Install the process-wide content resolution defaults.
///
/// Called once at boot with [`Bundle::default_locale`](super::Bundle::default_locale)
/// and [`Bundle::fallback_chain`](super::Bundle::fallback_chain) — i.e. exactly
/// [`I18nConfig::resolved_fallback_chain`](super::I18nConfig::resolved_fallback_chain),
/// the same chain UI strings walk.
///
/// The default locale is passed **explicitly** rather than read off the end of
/// the chain: `resolved_fallback_chain` appends the default only when it is
/// absent, so a config that already lists it (`fallback_chain = ["fr", "en"]`
/// with `default_locale = "fr"`) ends in `"en"`, and inferring from the tail
/// would file French prose under English.
///
/// Process-wide, like the attribute-encryption key ring and the default actor
/// token: two apps built in one process (a consolidated test binary, an
/// embedded second app) share these defaults, and the last one to boot wins.
/// It is only consulted **outside** a request scope — inside one, the scope
/// carries its own chain — so a served request is never affected.
pub fn install_locale_defaults(default_locale: &str, chain: Vec<String>) {
    let mut guard = DEFAULTS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(LocaleDefaults {
        default_locale: Arc::from(default_locale),
        chain: chain.into(),
    });
}

/// The process-wide fallback chain installed by [`install_locale_defaults`].
///
/// Empty when nothing has been installed; [`effective_chain`] is what
/// resolution actually walks.
#[must_use]
pub fn fallback_chain_snapshot() -> Vec<String> {
    DEFAULTS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map_or_else(Vec::new, |d| d.chain.to_vec())
}

/// The process-wide default locale, or [`LAST_RESORT_LOCALE`] when none was
/// installed.
#[must_use]
pub fn default_locale_snapshot() -> Arc<str> {
    DEFAULTS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map_or_else(
            || Arc::from(LAST_RESORT_LOCALE),
            |d| Arc::clone(&d.default_locale),
        )
}

/// The chain resolution actually walks when no scope supplies one: the
/// installed chain, or a one-link chain of the default locale.
///
/// Never empty. An app that declares `#[translatable]` columns without loading
/// a Fluent bundle installs nothing, and an empty chain would make every field
/// render as the empty-string sentinel while the row visibly holds a value.
fn effective_chain() -> Arc<[String]> {
    let guard = DEFAULTS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match guard.as_ref() {
        Some(d) if !d.chain.is_empty() => Arc::clone(&d.chain),
        Some(d) => Arc::from([d.default_locale.to_string()]),
        None => Arc::from([LAST_RESORT_LOCALE.to_owned()]),
    }
}

/// The active locale tag, or `None` outside any locale scope.
///
/// Never panics: a read with no scope in effect returns `None`.
#[must_use]
pub fn ambient_locale() -> Option<String> {
    ACTIVE_LOCALE.try_with(LocaleScope::tag).ok()
}

/// Refine the ambient locale on the scope currently in effect.
///
/// A no-op outside a scope, so it is always safe to call. Used by the
/// [`Locale`](super::Locale) extractor to publish its own answer — see
/// [`LocaleScope::tag`]'s note on why the layer's answer can be coarser.
pub fn publish_ambient_locale(tag: &str) {
    let _ = ACTIVE_LOCALE.try_with(|scope| scope.set_tag(tag));
}

/// The locale scope in effect, if any. Used by response-producing code that
/// needs to re-enter the request's scope later (e.g. a streaming body).
#[must_use]
pub fn ambient_locale_scope() -> Option<LocaleScope> {
    ACTIVE_LOCALE.try_with(Clone::clone).ok()
}

/// Run `fut` with `tag` as the ambient locale, walking the installed chain.
pub async fn with_locale<F>(tag: impl Into<String>, fut: F) -> F::Output
where
    F: std::future::Future,
{
    ACTIVE_LOCALE.scope(LocaleScope::for_tag(tag), fut).await
}

/// Run `fut` with `tag` as the ambient locale and an explicit fallback chain.
pub async fn with_locale_chain<F>(tag: impl Into<String>, chain: Vec<String>, fut: F) -> F::Output
where
    F: std::future::Future,
{
    ACTIVE_LOCALE.scope(LocaleScope::new(tag, chain), fut).await
}

/// Run `fut` inside an already-built [`LocaleScope`].
pub async fn with_locale_scope<F>(scope: LocaleScope, fut: F) -> F::Output
where
    F: std::future::Future,
{
    ACTIVE_LOCALE.scope(scope, fut).await
}

/// Synchronous sibling of [`with_locale`] (templates, `Display`, tests).
pub fn with_locale_sync<R>(tag: impl Into<String>, f: impl FnOnce() -> R) -> R {
    ACTIVE_LOCALE.sync_scope(LocaleScope::for_tag(tag), f)
}

/// Synchronous sibling of [`with_locale_chain`].
pub fn with_locale_chain_sync<R>(
    tag: impl Into<String>,
    chain: Vec<String>,
    f: impl FnOnce() -> R,
) -> R {
    ACTIVE_LOCALE.sync_scope(LocaleScope::new(tag, chain), f)
}

/// Synchronous sibling of [`with_locale_scope`].
pub fn with_locale_scope_sync<R>(scope: LocaleScope, f: impl FnOnce() -> R) -> R {
    ACTIVE_LOCALE.sync_scope(scope, f)
}

/// The locale a *write* with no explicit tag lands in.
///
/// The ambient request locale, else the last link of the installed chain
/// (which `resolved_fallback_chain` guarantees is the default locale), else
/// `"en"`.
#[must_use]
pub fn write_locale() -> String {
    if let Some(tag) = ambient_locale() {
        return tag;
    }
    default_locale_snapshot().to_string()
}

// ── The translatable value ───────────────────────────────────────────────────

/// A model field's content in every locale it has been translated into.
///
/// Declared on a model with `#[translatable]`:
///
/// ```rust,ignore
/// #[autumn_web::model(table = "posts")]
/// pub struct Post {
///     #[id]
///     pub id: i64,
///     #[translatable]
///     pub title: autumn_web::i18n::Translated,
/// }
/// ```
///
/// The value **is** the whole map, so a read-modify-write round trip
/// (`find` → [`set`](Self::set) → `update`) changes one locale and leaves the
/// others byte-for-byte intact. Rendering resolves against the request's
/// active locale on its own:
///
/// ```rust,ignore
/// html! { h1 { (post.title) } }   // Display → active locale, "" if untranslated
/// ```
#[derive(Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "db", derive(diesel::AsExpression, diesel::FromSqlRow))]
#[cfg_attr(feature = "db", diesel(sql_type = diesel::sql_types::Text))]
pub struct Translated {
    /// `locale tag -> value`. `BTreeMap` so `available_locales` and the stored
    /// JSON are deterministically ordered (a stable column value keeps record
    /// version-history diffs honest).
    values: BTreeMap<String, String>,
}

impl Translated {
    /// An empty map — no locale has a translation yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            values: BTreeMap::new(),
        }
    }

    /// Build from `(locale, value)` pairs.
    #[must_use]
    pub fn from_pairs<K, V, I>(pairs: I) -> Self
    where
        K: Into<String>,
        V: Into<String>,
        I: IntoIterator<Item = (K, V)>,
    {
        Self {
            values: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }

    /// A single translation filed under the **active** locale (see
    /// [`write_locale`]).
    ///
    /// The explicit "I mean one locale" constructor. `Deserialize` deliberately
    /// does **not** do this for a bare string — see the [`Deserialize`] impl.
    #[must_use]
    pub fn for_active(value: impl Into<String>) -> Self {
        Self::from_pairs([(write_locale(), value.into())])
    }

    /// The value stored for `locale` **exactly** — no fallback walk.
    #[must_use]
    pub fn get(&self, locale: &str) -> Option<&str> {
        self.values.get(locale).map(String::as_str)
    }

    /// Store `value` for `locale`, leaving every other locale untouched.
    pub fn set(&mut self, locale: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.values.insert(locale.into(), value.into());
        self
    }

    /// Store `value` for the **active** locale (see [`write_locale`]).
    pub fn set_active(&mut self, value: impl Into<String>) -> &mut Self {
        self.set(write_locale(), value)
    }

    /// Drop `locale`'s translation, returning it if there was one.
    pub fn remove(&mut self, locale: &str) -> Option<String> {
        self.values.remove(locale)
    }

    /// Merge `other`'s translations in, overwriting only the locales it
    /// carries and leaving every other locale untouched.
    ///
    /// Assigning a `Translated` **replaces** the whole container — the value is
    /// the entire set of translations, so `post.title = incoming` drops every
    /// locale `incoming` does not carry. When applying a partial update (a
    /// per-locale edit form, an API patch that names one language), merge it
    /// into the loaded container instead:
    ///
    /// ```rust,ignore
    /// let mut post = repo.find_by_id(id).await?.expect("post");
    /// post.title.merge_from(&incoming);   // every other locale survives
    /// ```
    pub fn merge_from(&mut self, other: &Self) -> &mut Self {
        for (locale, value) in &other.values {
            self.values.insert(locale.clone(), value.clone());
        }
        self
    }

    /// Resolve against the **active** locale, then the fallback chain.
    ///
    /// Returns `None` when the chain is exhausted — the single documented
    /// sentinel for "no translation". Never panics, in or out of a request.
    #[must_use]
    pub fn resolve(&self) -> Option<&str> {
        ACTIVE_LOCALE
            .try_with(|scope| self.lookup(Some(&scope.tag()), &scope.chain))
            .unwrap_or_else(|_| self.lookup(None, &effective_chain()))
    }

    /// Resolve against an explicit `locale`, then the fallback chain.
    ///
    /// Use when a locale is genuinely a parameter (a translation admin, a
    /// digest mailer looping over locales); handlers should prefer
    /// [`resolve`](Self::resolve), which needs no argument.
    #[must_use]
    pub fn resolve_in(&self, locale: &str) -> Option<&str> {
        ACTIVE_LOCALE
            .try_with(|scope| self.lookup(Some(locale), &scope.chain))
            .unwrap_or_else(|_| self.lookup(Some(locale), &effective_chain()))
    }

    /// Shared resolution core: exact match on `tag`, then each chain link in
    /// order. Mirrors `Bundle::lookup_template` so content and UI strings
    /// resolve identically.
    fn lookup<'a>(&'a self, tag: Option<&str>, chain: &[String]) -> Option<&'a str> {
        if let Some(tag) = tag
            && let Some(found) = self.values.get(tag)
        {
            return Some(found.as_str());
        }
        for fallback in chain {
            if Some(fallback.as_str()) == tag {
                continue;
            }
            if let Some(found) = self.values.get(fallback) {
                return Some(found.as_str());
            }
        }
        None
    }

    /// Every locale this field has been translated into, sorted by tag.
    ///
    /// The "needs translation" affordance: compare against the app's supported
    /// locales to render a badge.
    #[must_use]
    pub fn available_locales(&self) -> Vec<&str> {
        self.values.keys().map(String::as_str).collect()
    }

    /// Whether `locale` has its own translation (no fallback walk).
    #[must_use]
    pub fn is_translated(&self, locale: &str) -> bool {
        self.values.contains_key(locale)
    }

    /// Number of locales translated.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether no locale has a translation.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Iterate `(locale, value)` pairs in tag order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Encode to the column's stored representation: a JSON object.
    #[must_use]
    pub fn encode_column(&self) -> String {
        serde_json::to_string(&self.values).unwrap_or_else(|_| "{}".to_owned())
    }

    /// Decode a stored column value.
    ///
    /// A JSON object is the translation map when **every key looks like a
    /// locale tag and every value is a string**. Any other content — bare text
    /// from before the column was declared translatable, a JSON scalar, an
    /// object of some other shape, malformed JSON — is taken as
    /// `default_locale`'s translation, so a plain-text column upgrades in place
    /// with no backfill and a read can never fail.
    ///
    /// The key check is what keeps the upgrade path from misreading a column
    /// that already held an unrelated JSON string-map (`{"host": "db1"}`) as
    /// translations. It cannot be airtight — `{"en": "…"}` is a plausible
    /// non-translation map too — so declare `#[translatable]` only on columns
    /// that hold human-readable text.
    #[must_use]
    pub fn decode_column(raw: &str, default_locale: &str) -> Self {
        if raw.is_empty() {
            return Self::new();
        }
        if let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(raw) {
            let mut values = BTreeMap::new();
            for (k, v) in obj {
                match v {
                    serde_json::Value::String(s) if looks_like_locale_tag(&k) => {
                        values.insert(k, s);
                    }
                    // Not a translation map after all — treat the whole column
                    // as one locale's text rather than dropping content.
                    _ => return Self::from_pairs([(default_locale, raw)]),
                }
            }
            return Self { values };
        }
        Self::from_pairs([(default_locale, raw)])
    }
}

/// Whether `key` has the shape of a BCP-47 language tag, in either of the two
/// forms that can appear as a locale: a **langtag** (`en`, `es-MX`,
/// `zh-Hant-TW`) — a 2-3 letter primary subtag followed by `-`-joined
/// alphanumeric subtags of 1-8 characters — or a **singleton-led** tag, which
/// covers private use (`x-private`) and the grandfathered irregular forms
/// (`i-klingon`).
///
/// Deliberately a shape test, not a registry lookup: a private-use or
/// not-yet-registered tag must still round-trip. The rule this function exists
/// to uphold is that **whatever [`Translated::set`] accepts,
/// [`Translated::decode_column`] must recognise on the way back** — a key the
/// writer stores and the reader rejects makes the whole container decode as
/// legacy text, hiding every translation and letting the next save escape the
/// JSON into the column permanently.
///
/// # Known limits
///
/// BCP-47 also permits 4-8 letter primary subtags (reserved and registered
/// ranges, essentially unused in practice). They are **not** accepted here:
/// admitting them would also admit ordinary English words like `host` or
/// `name`, and this test's other job is to keep a column that already held an
/// unrelated JSON string-map from being mistaken for translations. Two-letter
/// keys are inherently ambiguous (`{"db": "primary"}` reads as a translation
/// map) — the test cannot be airtight, which is why `#[translatable]` belongs
/// only on columns that hold human-readable text.
fn looks_like_locale_tag(key: &str) -> bool {
    let mut parts = key.split('-');
    let Some(primary) = parts.next() else {
        return false;
    };
    let mut rest = parts.peekable();
    if primary.len() == 1 {
        // Singleton-led: `x-…` (private use) and `i-…` (grandfathered
        // irregular). A bare singleton with no subtag is not a tag.
        let Some(byte) = primary.bytes().next() else {
            return false;
        };
        if !matches!(byte.to_ascii_lowercase(), b'x' | b'i') || rest.peek().is_none() {
            return false;
        }
    } else if !(2..=3).contains(&primary.len()) || !primary.bytes().all(|b| b.is_ascii_alphabetic())
    {
        return false;
    }
    rest.all(|p| (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Renders the active locale's value, or the empty string when the fallback
/// chain is exhausted — the string form of [`Translated::resolve`]'s `None`.
impl fmt::Display for Translated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.resolve().unwrap_or(""))
    }
}

/// Shows every locale, not just the resolved one: a `Debug` that hid the other
/// translations would make a clobbering bug invisible in test output.
impl fmt::Debug for Translated {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.values.iter()).finish()
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for Translated {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self::from_pairs(iter)
    }
}

/// Lossless: emits the whole map. Record version history and durable
/// commit-hook payloads snapshot models through `serde` and reconstruct them,
/// so a `Serialize` that emitted only the resolved value would silently
/// destroy every other locale on replay.
impl Serialize for Translated {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.values.iter())
    }
}

/// The exact inverse of [`Serialize`]: a map of locale tag to value, and
/// **only** that.
///
/// A bare string is deliberately rejected. Accepting one would have to file it
/// under a single locale, and since assigning a `Translated` replaces the whole
/// container, `PUT /api/posts/1` with `{"title": "Hola"}` would then delete
/// every other language without a word — from a body that reads like it is
/// setting one field. Requiring `{"title": {"es": "Hola"}}` makes the
/// replacement visible in the payload; [`Translated::merge_from`] is the
/// non-destructive path, and [`Translated::for_active`] is still available in
/// Rust when a single-locale value is genuinely what you mean.
impl<'de> Deserialize<'de> for Translated {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TranslatedVisitor;

        impl<'de> Visitor<'de> for TranslatedVisitor {
            type Value = Translated;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(
                    "a map of locale tag to translated value, e.g. {\"en\": \"Hello\"} \
                     (a bare string is refused: it would replace every other locale)",
                )
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(Translated::new())
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(Translated::new())
            }

            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut values = BTreeMap::new();
                while let Some((k, v)) = map.next_entry::<String, String>()? {
                    values.insert(k, v);
                }
                Ok(Translated { values })
            }
        }

        deserializer.deserialize_map(TranslatedVisitor)
    }
}

// ── Diesel codec (`TEXT` on both backends) ───────────────────────────────────

#[cfg(feature = "db")]
mod db {
    use diesel::backend::Backend;
    use diesel::deserialize::{self, FromSql};
    use diesel::serialize::{self, IsNull, Output, ToSql};
    use diesel::sql_types::Text;

    use super::{Translated, default_locale_snapshot};

    impl ToSql<Text, diesel::sqlite::Sqlite> for Translated {
        fn to_sql<'b>(
            &'b self,
            out: &mut Output<'b, '_, diesel::sqlite::Sqlite>,
        ) -> serialize::Result {
            out.set_value(self.encode_column());
            Ok(IsNull::No)
        }
    }

    impl ToSql<Text, diesel::pg::Pg> for Translated {
        fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, diesel::pg::Pg>) -> serialize::Result {
            use std::io::Write as _;
            out.write_all(self.encode_column().as_bytes())?;
            Ok(IsNull::No)
        }
    }

    impl<DB> FromSql<Text, DB> for Translated
    where
        DB: Backend,
        String: FromSql<Text, DB>,
    {
        fn from_sql(bytes: DB::RawValue<'_>) -> deserialize::Result<Self> {
            let raw = String::from_sql(bytes)?;
            // One `Arc` refcount bump per decoded value, not a chain clone:
            // a list page decodes this once per translatable column per row.
            Ok(Self::decode_column(&raw, &default_locale_snapshot()))
        }
    }
}

// ── Registry ─────────────────────────────────────────────────────────────────

/// One `#[translatable]` column, registered by the `#[model]` macro.
///
/// Lets framework surfaces that have no compile-time view of the model (the
/// admin plugin, the scaffold's introspection, an app's own tooling) discover
/// which columns hold a translation container.
#[derive(Debug, Clone, Copy)]
pub struct TranslatableColumnDescriptor {
    /// Rust name of the model type.
    pub model: &'static str,
    /// Table the column lives on.
    pub table: &'static str,
    /// Column (Rust field) name.
    pub column: &'static str,
}

inventory::collect!(TranslatableColumnDescriptor);

/// Every `#[translatable]` column registered across the binary.
#[must_use]
pub fn registered_translatable_columns() -> Vec<&'static TranslatableColumnDescriptor> {
    inventory::iter::<TranslatableColumnDescriptor>
        .into_iter()
        .collect()
}

/// The `#[translatable]` column names on one table.
#[must_use]
pub fn translatable_columns_for_table(table: &str) -> Vec<&'static str> {
    registered_translatable_columns()
        .iter()
        .filter(|d| d.table == table)
        .map(|d| d.column)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::{
        Translated, fallback_chain_snapshot, install_locale_defaults, with_locale,
        with_locale_chain_sync,
    };
    use super::looks_like_locale_tag;

    fn chain(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Serializes the handful of tests that read or write the process-wide
    /// resolution defaults, so they cannot interleave with each other while the
    /// rest of the module runs in parallel.
    static DEFAULTS_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_defaults_guard<R>(f: impl FnOnce() -> R) -> R {
        let _guard = DEFAULTS_GUARD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f()
    }

    // ── AC1: independent value per locale tag ────────────────────────────────

    #[test]
    fn set_stores_an_independent_value_per_locale() {
        let mut t = Translated::new();
        t.set("en", "Hello");
        t.set("es", "Hola");
        assert_eq!(t.get("en"), Some("Hello"));
        assert_eq!(t.get("es"), Some("Hola"));
        assert_eq!(t.get("fr"), None);
    }

    // ── AC4: writing one locale never clobbers another ───────────────────────

    #[test]
    fn updating_one_locale_leaves_the_others_untouched() {
        let mut t = Translated::new();
        t.set("en", "Hello");
        t.set("es", "Hola");
        t.set("en", "Hello again");
        assert_eq!(t.get("en"), Some("Hello again"));
        assert_eq!(t.get("es"), Some("Hola"));
    }

    // ── AC2: resolution against the ambient locale, no argument passed ────────

    #[test]
    fn resolve_uses_the_ambient_locale() {
        let mut t = Translated::new();
        t.set("en", "Hello");
        t.set("es", "Hola");
        let got = with_locale_chain_sync("es", chain(&["en"]), || t.resolve().map(str::to_owned));
        assert_eq!(got.as_deref(), Some("Hola"));
    }

    #[tokio::test]
    async fn resolve_uses_the_ambient_locale_across_an_await() {
        let mut t = Translated::new();
        t.set("en", "Hello");
        t.set("fr", "Bonjour");
        let got = with_locale("fr", async { t.resolve().map(str::to_owned) }).await;
        assert_eq!(got.as_deref(), Some("Bonjour"));
    }

    // ── AC3: fallback chain walk, then a single documented sentinel ───────────

    #[test]
    fn resolve_walks_the_fallback_chain_when_the_active_locale_is_missing() {
        let mut t = Translated::new();
        t.set("en", "Hello");
        let got = with_locale_chain_sync("fr", chain(&["en"]), || t.resolve().map(str::to_owned));
        assert_eq!(got.as_deref(), Some("Hello"));
    }

    #[test]
    fn resolve_honours_chain_order() {
        let mut t = Translated::new();
        t.set("en", "Hello");
        t.set("pt", "Ola");
        let got = with_locale_chain_sync("pt-BR", chain(&["pt", "en"]), || {
            t.resolve().map(str::to_owned)
        });
        assert_eq!(got.as_deref(), Some("Ola"), "first chain link wins");
    }

    #[test]
    fn resolve_returns_none_when_the_chain_is_exhausted() {
        let mut t = Translated::new();
        t.set("de", "Hallo");
        let got = with_locale_chain_sync("fr", chain(&["en"]), || t.resolve().map(str::to_owned));
        assert_eq!(got, None, "exhausted chain is None, never a panic");
    }

    #[test]
    fn display_renders_the_empty_string_when_unresolved() {
        let t = Translated::new();
        let rendered = with_locale_chain_sync("fr", chain(&["en"]), || format!("[{t}]"));
        assert_eq!(rendered, "[]");
    }

    #[test]
    fn resolution_outside_any_request_scope_never_panics() {
        let mut t = Translated::new();
        t.set("en", "Hello");
        // No ambient scope at all: falls back to the installed default chain.
        let _ = t.resolve();
        assert!(t.resolve_in("en").is_some());
    }

    #[test]
    fn resolve_in_takes_an_explicit_locale_over_the_ambient_one() {
        let mut t = Translated::new();
        t.set("en", "Hello");
        t.set("es", "Hola");
        let got = with_locale_chain_sync("es", chain(&["en"]), || {
            t.resolve_in("en").map(str::to_owned)
        });
        assert_eq!(got.as_deref(), Some("Hello"));
    }

    // ── AC5: observing which locales a record carries ────────────────────────

    #[test]
    fn available_locales_are_reported_in_a_stable_order() {
        let mut t = Translated::new();
        t.set("es", "Hola");
        t.set("en", "Hello");
        assert_eq!(t.available_locales(), vec!["en", "es"]);
        assert!(t.is_translated("es"));
        assert!(!t.is_translated("de"));
        assert_eq!(t.len(), 2);
        assert!(!t.is_empty());
    }

    #[test]
    fn remove_drops_one_locale_only() {
        let mut t = Translated::new();
        t.set("en", "Hello");
        t.set("es", "Hola");
        assert_eq!(t.remove("en").as_deref(), Some("Hello"));
        assert_eq!(t.available_locales(), vec!["es"]);
    }

    // ── Wire format ──────────────────────────────────────────────────────────

    #[test]
    fn json_round_trips_losslessly() {
        let mut t = Translated::new();
        t.set("en", "Hello");
        t.set("es", "Hola");
        let json = serde_json::to_string(&t).expect("serialize");
        let back: Translated = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back, t,
            "serde must be lossless — version history depends on it"
        );
        assert!(json.contains("\"es\""));
    }

    /// A bare string is refused: accepting one would let
    /// `PUT /api/posts/1 {"title": "Hola"}` replace the whole container and
    /// delete every other language without a word.
    #[test]
    fn deserializing_a_bare_string_is_refused() {
        let err = serde_json::from_str::<Translated>("\"Hola\"").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("locale tag"), "{msg}");
    }

    #[test]
    fn merge_from_overwrites_only_the_locales_it_carries() {
        let mut stored = Translated::from_pairs([("en", "Hello"), ("es", "Hola")]);
        let incoming = Translated::from_pairs([("es", "Hola de nuevo"), ("fr", "Bonjour")]);
        stored.merge_from(&incoming);
        assert_eq!(stored.get("en"), Some("Hello"), "untouched locale survives");
        assert_eq!(stored.get("es"), Some("Hola de nuevo"));
        assert_eq!(stored.get("fr"), Some("Bonjour"));
        // The incoming container is unchanged by the merge.
        assert_eq!(incoming.available_locales(), vec!["es", "fr"]);
    }

    /// Assignment (what a `Patch::Set` does) REPLACES — the property
    /// `merge_from` exists to work around. Pinned so the distinction can't
    /// erode silently.
    #[test]
    fn assignment_replaces_the_whole_container() {
        let stored = Translated::from_pairs([("en", "Hello"), ("es", "Hola")]);
        let incoming = Translated::from_pairs([("es", "Hola de nuevo")]);
        // What a `Patch::Set` does: the field takes the incoming value whole.
        let after = incoming;
        assert_eq!(stored.get("en"), Some("Hello"));
        assert_eq!(after.get("en"), None, "assignment is replace, not merge");
    }

    #[test]
    fn locale_tag_shape_gates_the_translation_map_decoding() {
        for tag in [
            "en",
            "es-MX",
            "zh-Hant-TW",
            // BCP-47 private use and the grandfathered irregular forms are
            // singleton-led. `set` accepts them, so decoding must too.
            "x-private",
            "i-klingon",
            "X-Private",
        ] {
            assert!(looks_like_locale_tag(tag), "{tag} should be tag-shaped");
        }
        for not_a_tag in [
            "host",
            "a",
            "",
            "en_US",
            "x",
            "i",
            "q-",
            "-en",
            "en-toolongsubtag",
        ] {
            assert!(
                !looks_like_locale_tag(not_a_tag),
                "{not_a_tag} should not be tag-shaped"
            );
        }
        // An unrelated JSON string-map in a column that predates the attribute
        // is read as text, not mistaken for translations.
        let t = Translated::decode_column("{\"host\":\"db1\"}", "en");
        assert_eq!(t.get("en"), Some("{\"host\":\"db1\"}"));
    }

    /// The invariant behind `looks_like_locale_tag`: anything `set` accepts has
    /// to survive `encode_column` -> `decode_column`. A key the writer stores
    /// and the reader rejects makes the WHOLE container decode as legacy text —
    /// every translation goes unreachable, and the next save escapes the JSON
    /// into the column permanently.
    #[test]
    fn every_writable_locale_tag_survives_a_column_round_trip() {
        for tag in [
            "en",
            "es",
            "es-MX",
            "zh-Hant-TW",
            "pt-BR",
            "sgn-BE-FR",
            "x-private",
            "i-klingon",
        ] {
            let mut written = Translated::new();
            written.set(tag, "value");
            let decoded = Translated::decode_column(&written.encode_column(), "en");
            assert_eq!(
                decoded.get(tag),
                Some("value"),
                "`{tag}` was writable but did not decode back"
            );
            assert_eq!(decoded, written, "round trip must be lossless for `{tag}`");
        }
    }

    /// Reads and writes must agree with no configuration installed: an app
    /// that declares `#[translatable]` columns without loading a Fluent bundle
    /// must not render every field as the empty sentinel.
    #[test]
    fn a_write_with_no_configuration_is_readable_back() {
        with_defaults_guard(|| {
            let mut t = Translated::new();
            t.set_active("Hello");
            assert_eq!(
                t.resolve(),
                Some("Hello"),
                "the write-side last resort must be reachable from the read side"
            );
            assert_eq!(t.to_string(), "Hello");
        });
    }

    /// The ambient scope is refinable, so a deeper `Locale` extraction can
    /// correct a coarser answer published by the layer.
    #[test]
    fn publishing_a_locale_refines_the_scope_in_place() {
        let t = Translated::from_pairs([("en", "Hello"), ("es", "Hola")]);
        let got = with_locale_chain_sync("en", chain(&["en"]), || {
            super::super::publish_ambient_locale("es");
            t.resolve().map(str::to_owned)
        });
        assert_eq!(got.as_deref(), Some("Hola"));
    }

    #[test]
    fn publishing_outside_a_scope_is_inert() {
        // Must never panic; there is simply nothing to refine.
        super::super::publish_ambient_locale("es");
    }

    /// A scope built with an explicitly empty chain still resolves through the
    /// process default rather than collapsing to the sentinel.
    #[test]
    fn an_empty_chain_falls_back_to_the_process_default() {
        with_defaults_guard(|| {
            let t = Translated::from_pairs([("en", "Hello")]);
            let got = with_locale_chain_sync("de", Vec::new(), || t.resolve().map(str::to_owned));
            assert_eq!(got.as_deref(), Some("Hello"));
        });
    }

    #[test]
    fn decoding_a_legacy_plain_text_column_keeps_the_value() {
        // A column that predates `#[translatable]` holds bare text, not JSON.
        let t = Translated::decode_column("Hello", "en");
        assert_eq!(t.get("en"), Some("Hello"));
        assert!(!t.encode_column().is_empty());
    }

    #[test]
    fn decoding_a_json_object_of_non_strings_falls_back_to_plain_text() {
        let t = Translated::decode_column("{\"en\":42}", "en");
        assert_eq!(t.get("en"), Some("{\"en\":42}"));
    }

    #[test]
    fn decoding_an_empty_column_yields_no_translations() {
        assert!(Translated::decode_column("", "en").is_empty());
        assert!(Translated::decode_column("{}", "en").is_empty());
    }

    #[test]
    fn encode_column_emits_a_json_object() {
        let mut t = Translated::new();
        t.set("en", "Hello");
        assert_eq!(t.encode_column(), "{\"en\":\"Hello\"}");
    }

    // ── Default chain plumbing ───────────────────────────────────────────────

    /// The default locale is stored EXPLICITLY, not inferred from the chain's
    /// tail: `resolved_fallback_chain` appends the default only when absent, so
    /// `fallback_chain = ["fr", "en"]` with `default_locale = "fr"` ends in
    /// `"en"` — and inferring would file French prose under English.
    #[test]
    fn installed_defaults_keep_the_default_locale_independent_of_the_chain_tail() {
        with_defaults_guard(|| {
            install_locale_defaults("fr", chain(&["fr", "en"]));
            assert_eq!(fallback_chain_snapshot(), chain(&["fr", "en"]));
            assert_eq!(&*super::super::default_locale_snapshot(), "fr");
            assert_eq!(super::super::write_locale(), "fr");
            // A legacy plain-text column is attributed to the default locale.
            let legacy =
                Translated::decode_column("Bonjour", &super::super::default_locale_snapshot());
            assert_eq!(legacy.get("fr"), Some("Bonjour"));
            // Restore the framework default for the rest of this binary.
            install_locale_defaults("en", chain(&["en"]));
        });
    }

    #[test]
    fn for_active_seeds_the_ambient_locale() {
        let t = with_locale_chain_sync("es", chain(&["en"]), || Translated::for_active("Hola"));
        assert_eq!(t.get("es"), Some("Hola"));
    }
}
