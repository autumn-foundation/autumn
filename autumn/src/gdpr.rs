//! GDPR/CCPA data-export and account-erasure registry.
//!
//! Provides the [`GdprRegistry`] for registering application models and their
//! erasure strategies, and the [`ExportArchive`] type used by the data-export
//! background job.
//!
//! ## Usage
//!
//! Register each `#[repository]` model that holds user data when constructing
//! the application state:
//!
//! ```rust,no_run
//! use autumn_web::gdpr::{ErasureStrategy, GdprRegistry, ModelRegistration};
//!
//! let registry = GdprRegistry::new()
//!     .register(ModelRegistration::hard_delete("posts"))
//!     .register(ModelRegistration::anonymize("comments"))
//!     .register(ModelRegistration::retain(
//!         "invoices",
//!         "financial records retained for 7 years (HMRC requirement)",
//!     ));
//! ```

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How a model's data should be handled during account erasure (GDPR Article 17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErasureStrategy {
    /// Permanently delete all rows belonging to the user.
    HardDelete,
    /// Replace identifying columns with deterministic tombstone values so
    /// referential integrity is maintained while PII is removed.
    Anonymize,
    /// Retain rows under legal hold. The retention reason is captured in the
    /// audit log and surfaced in the export archive manifest.
    Retain,
}

/// Registration for a single model in the GDPR export/erasure registry.
#[derive(Debug, Clone)]
pub struct ModelRegistration {
    /// Database table name for this model.
    pub table: String,
    /// Strategy applied during account erasure.
    pub erasure_strategy: ErasureStrategy,
    /// Required when `erasure_strategy == Retain`. Documented in the audit log.
    pub retain_reason: Option<String>,
}

impl ModelRegistration {
    /// Register a model for hard deletion during erasure.
    #[must_use]
    pub fn hard_delete(table: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            erasure_strategy: ErasureStrategy::HardDelete,
            retain_reason: None,
        }
    }

    /// Register a model for anonymization during erasure.
    #[must_use]
    pub fn anonymize(table: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            erasure_strategy: ErasureStrategy::Anonymize,
            retain_reason: None,
        }
    }

    /// Register a model for legal-hold retention during erasure.
    ///
    /// `reason` is written to the audit log and included in the export archive
    /// manifest so the user can see why certain data was retained.
    #[must_use]
    pub fn retain(table: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            erasure_strategy: ErasureStrategy::Retain,
            retain_reason: Some(reason.into()),
        }
    }
}

/// Registry mapping model names to their GDPR export and erasure registrations.
///
/// Build once at application start-up and store in [`crate::AppState`]:
///
/// ```rust,no_run
/// use autumn_web::gdpr::{GdprRegistry, ModelRegistration};
///
/// let registry = GdprRegistry::new()
///     .register(ModelRegistration::hard_delete("posts"))
///     .register(ModelRegistration::anonymize("comments"));
/// ```
#[derive(Debug, Clone, Default)]
pub struct GdprRegistry {
    registrations: Vec<ModelRegistration>,
}

impl GdprRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a model for GDPR export and erasure.
    #[must_use]
    pub fn register(mut self, registration: ModelRegistration) -> Self {
        self.registrations.push(registration);
        self
    }

    /// Returns all registered table names.
    #[must_use]
    pub fn registered_tables(&self) -> Vec<&str> {
        self.registrations
            .iter()
            .map(|r| r.table.as_str())
            .collect()
    }

    /// Look up the registration for a specific table.
    #[must_use]
    pub fn get(&self, table: &str) -> Option<&ModelRegistration> {
        self.registrations.iter().find(|r| r.table == table)
    }

    /// Returns `true` when at least one model is registered.
    #[must_use]
    pub const fn is_populated(&self) -> bool {
        !self.registrations.is_empty()
    }

    /// Returns all registrations with the [`ErasureStrategy::Retain`] strategy.
    #[must_use]
    pub fn retained_tables(&self) -> Vec<&ModelRegistration> {
        self.registrations
            .iter()
            .filter(|r| r.erasure_strategy == ErasureStrategy::Retain)
            .collect()
    }
}

/// Metadata attached to every data export archive.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExportManifest {
    /// The user identifier this export belongs to.
    pub user_id: String,
    /// RFC-3339 timestamp when the export was generated.
    pub generated_at: String,
    /// Autumn framework version that generated this export.
    pub framework_version: String,
    /// Tables included in this export.
    pub tables_included: Vec<String>,
    /// Tables retained under legal hold, with reasons.
    pub tables_retained: Vec<RetainedTable>,
}

/// A table retained under legal hold rather than erased.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetainedTable {
    /// The database table name.
    pub table: String,
    /// Human-readable reason for retention (captured in audit log).
    pub reason: String,
}

/// The complete data export archive for a single user.
///
/// Serializes to a single JSON document with a `manifest` section and a
/// `tables` map of per-table record arrays. Only fields annotated with
/// `#[export]` are included (allowlist, not blocklist).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExportArchive {
    /// Metadata describing the export.
    pub manifest: ExportManifest,
    /// Per-table exported records, keyed by table name.
    ///
    /// Each value is the array of JSON objects for that table. Fields not
    /// annotated with `#[export]` are excluded by the repository layer.
    pub tables: HashMap<String, Vec<Value>>,
}

impl ExportArchive {
    /// Create an empty archive for the given user.
    #[must_use]
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            manifest: ExportManifest {
                user_id: user_id.into(),
                generated_at: crate::time::ambient_now().to_rfc3339(),
                framework_version: env!("CARGO_PKG_VERSION").to_owned(),
                ..Default::default()
            },
            tables: HashMap::new(),
        }
    }

    /// Add a table's records to the archive.
    pub fn add_table(&mut self, table: impl Into<String>, records: Vec<Value>) {
        let table = table.into();
        self.manifest.tables_included.push(table.clone());
        self.tables.insert(table, records);
    }

    /// Record a retained table in the manifest.
    pub fn add_retained(&mut self, table: impl Into<String>, reason: impl Into<String>) {
        self.manifest.tables_retained.push(RetainedTable {
            table: table.into(),
            reason: reason.into(),
        });
    }

    /// Serialize the archive to a JSON string.
    ///
    /// # Errors
    ///
    /// Returns an error when serde fails (not expected in practice for this type).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ErasureStrategy ───────────────────────────────────────────────────────

    #[test]
    fn erasure_strategy_hard_delete_serializes_as_snake_case() {
        let s = serde_json::to_string(&ErasureStrategy::HardDelete).unwrap();
        assert_eq!(s, r#""hard_delete""#);
    }

    #[test]
    fn erasure_strategy_anonymize_serializes() {
        let s = serde_json::to_string(&ErasureStrategy::Anonymize).unwrap();
        assert_eq!(s, r#""anonymize""#);
    }

    #[test]
    fn erasure_strategy_retain_serializes() {
        let s = serde_json::to_string(&ErasureStrategy::Retain).unwrap();
        assert_eq!(s, r#""retain""#);
    }

    #[test]
    fn erasure_strategy_round_trips_through_json() {
        for strategy in [
            ErasureStrategy::HardDelete,
            ErasureStrategy::Anonymize,
            ErasureStrategy::Retain,
        ] {
            let json = serde_json::to_string(&strategy).unwrap();
            let decoded: ErasureStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, strategy);
        }
    }

    // ── ModelRegistration ─────────────────────────────────────────────────────

    #[test]
    fn model_registration_hard_delete_sets_strategy() {
        let r = ModelRegistration::hard_delete("posts");
        assert_eq!(r.table, "posts");
        assert_eq!(r.erasure_strategy, ErasureStrategy::HardDelete);
        assert!(r.retain_reason.is_none());
    }

    #[test]
    fn model_registration_anonymize_sets_strategy() {
        let r = ModelRegistration::anonymize("comments");
        assert_eq!(r.table, "comments");
        assert_eq!(r.erasure_strategy, ErasureStrategy::Anonymize);
        assert!(r.retain_reason.is_none());
    }

    #[test]
    fn model_registration_retain_sets_strategy_and_reason() {
        let r = ModelRegistration::retain("invoices", "7-year financial retention");
        assert_eq!(r.table, "invoices");
        assert_eq!(r.erasure_strategy, ErasureStrategy::Retain);
        assert_eq!(
            r.retain_reason.as_deref(),
            Some("7-year financial retention")
        );
    }

    // ── GdprRegistry ─────────────────────────────────────────────────────────

    #[test]
    fn registry_empty_by_default() {
        let registry = GdprRegistry::new();
        assert!(!registry.is_populated());
        assert!(registry.registered_tables().is_empty());
    }

    #[test]
    fn registry_register_adds_model() {
        let registry = GdprRegistry::new().register(ModelRegistration::hard_delete("posts"));
        assert!(registry.is_populated());
        assert_eq!(registry.registered_tables(), vec!["posts"]);
    }

    #[test]
    fn registry_get_returns_registration_for_known_table() {
        let registry = GdprRegistry::new()
            .register(ModelRegistration::hard_delete("posts"))
            .register(ModelRegistration::anonymize("comments"));
        let reg = registry.get("posts").expect("posts must be registered");
        assert_eq!(reg.erasure_strategy, ErasureStrategy::HardDelete);
    }

    #[test]
    fn registry_get_returns_none_for_unknown_table() {
        let registry = GdprRegistry::new().register(ModelRegistration::hard_delete("posts"));
        assert!(registry.get("orders").is_none());
    }

    #[test]
    fn registry_retained_tables_filters_correctly() {
        let registry = GdprRegistry::new()
            .register(ModelRegistration::hard_delete("posts"))
            .register(ModelRegistration::retain("invoices", "financial hold"))
            .register(ModelRegistration::anonymize("comments"))
            .register(ModelRegistration::retain("contracts", "legal hold"));
        let retained = registry.retained_tables();
        assert_eq!(retained.len(), 2);
        assert!(retained.iter().any(|r| r.table == "invoices"));
        assert!(retained.iter().any(|r| r.table == "contracts"));
    }

    #[test]
    fn registry_registered_tables_returns_all_table_names() {
        let registry = GdprRegistry::new()
            .register(ModelRegistration::hard_delete("posts"))
            .register(ModelRegistration::anonymize("comments"))
            .register(ModelRegistration::retain("invoices", "hold"));
        let tables = registry.registered_tables();
        assert_eq!(tables.len(), 3);
        assert!(tables.contains(&"posts"));
        assert!(tables.contains(&"comments"));
        assert!(tables.contains(&"invoices"));
    }

    // ── ExportArchive ─────────────────────────────────────────────────────────

    #[test]
    fn export_archive_new_sets_user_id() {
        let archive = ExportArchive::new("user-42");
        assert_eq!(archive.manifest.user_id, "user-42");
    }

    #[test]
    fn export_archive_add_table_populates_tables_and_manifest() {
        let mut archive = ExportArchive::new("u1");
        archive.add_table(
            "posts",
            vec![serde_json::json!({"id": 1, "title": "Hello"})],
        );
        assert_eq!(archive.tables.len(), 1);
        assert!(archive.tables.contains_key("posts"));
        assert!(
            archive
                .manifest
                .tables_included
                .contains(&"posts".to_owned())
        );
    }

    #[test]
    fn export_archive_add_retained_populates_manifest() {
        let mut archive = ExportArchive::new("u1");
        archive.add_retained("invoices", "7-year financial hold");
        assert_eq!(archive.manifest.tables_retained.len(), 1);
        assert_eq!(archive.manifest.tables_retained[0].table, "invoices");
        assert_eq!(
            archive.manifest.tables_retained[0].reason,
            "7-year financial hold"
        );
    }

    #[test]
    fn export_archive_to_json_is_valid_json() {
        let mut archive = ExportArchive::new("u1");
        archive.add_table("posts", vec![serde_json::json!({"id": 1})]);
        let json = archive.to_json().expect("serialization must succeed");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("must parse as valid JSON");
        assert!(
            parsed.get("manifest").is_some(),
            "archive JSON must have a manifest key"
        );
        assert!(
            parsed.get("tables").is_some(),
            "archive JSON must have a tables key"
        );
    }

    #[test]
    fn export_archive_json_contains_user_id_in_manifest() {
        let archive = ExportArchive::new("user-99");
        let json = archive.to_json().unwrap();
        assert!(
            json.contains("user-99"),
            "JSON must contain the user_id: {json}"
        );
    }

    #[test]
    fn export_archive_json_contains_framework_version() {
        let archive = ExportArchive::new("u1");
        let json = archive.to_json().unwrap();
        assert!(
            json.contains("framework_version"),
            "JSON must include framework_version: {json}"
        );
    }
}
