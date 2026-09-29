//! Which columns hold a collaborative document.
//!
//! The `#[model]` macro registers one [`CollaborativeColumnDescriptor`] per
//! `#[collaborative]` field. Surfaces with no compile-time view of the model
//! — the admin plugin, a session hub keyed by strings, an operator report —
//! read the registry instead of the type.

/// One `#[collaborative]` column, registered by the `#[model]` macro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollaborativeColumnDescriptor {
    /// Rust name of the model type.
    pub model: &'static str,
    /// Table the column lives on.
    pub table: &'static str,
    /// Column (Rust field) name.
    pub column: &'static str,
}

inventory::collect!(CollaborativeColumnDescriptor);

/// Every `#[collaborative]` column registered across the binary.
#[must_use]
pub fn registered_collaborative_columns() -> Vec<&'static CollaborativeColumnDescriptor> {
    inventory::iter::<CollaborativeColumnDescriptor>
        .into_iter()
        .collect()
}

/// The `#[collaborative]` column names on one table.
#[must_use]
pub fn collaborative_columns_for_table(table: &str) -> Vec<&'static str> {
    registered_collaborative_columns()
        .into_iter()
        .filter(|d| d.table == table)
        .map(|d| d.column)
        .collect()
}
