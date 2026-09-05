//! Plugin-owned, tenant-scoped tables for a sandboxed plugin (issue #1632).
//!
//! A plugin granted `db` can read and write rows. It cannot read or write the
//! host application's rows, and it cannot see another tenant's — and neither of
//! those is a check that could be forgotten, because neither is expressible:
//!
//! * **No raw SQL crosses.** There is no field in the wire protocol that carries
//!   a statement, a fragment, an operator or an order-by. A guest names a table,
//!   a row id and an equality filter, and the host writes the statement.
//! * **The guest cannot name a physical table.** It names a *logical* table,
//!   which must appear in `[grants].tables`, and the host derives
//!   `plugin_<plugin>_<table>` from it. The application's `users` table has no
//!   logical name that maps to it.
//! * **The guest cannot name a tenant.** Every operation carries the ambient
//!   tenant, taken from the request rather than from the frame. `db-query` with
//!   an empty filter returns this tenant's rows because that is the only kind of
//!   row the statement can select.
//!
//! # What a store implementation is handed, and what it must do
//!
//! A [`PluginStore`] receives a [`Scope`] — a physical table name and a tenant —
//! plus a [`PluginRow`] of scalars. The framework ships
//! [`MemoryPluginStore`], a faithful model of the scoping; a durable
//! implementation over SQL is the operator's to supply, and this module owes it
//! two guarantees and one obligation.
//!
//! The guarantees: [`Scope::table`] is a bare `[a-z][a-z0-9_]*` identifier that
//! cannot close a quote or open a statement, because [`physical_table`] derives
//! it from two names that were each refused unless they matched that shape and
//! re-checks the result rather than trusting that they were (it is public, and
//! `SandboxHost::from_module` takes a manifest an embedder built by hand). And
//! every column name in a row has passed the same check, in
//! [`check_row`](super::check_row).
//!
//! The obligation: **row values are never identifiers and must never be
//! concatenated into a statement.** They are guest-chosen strings and numbers,
//! and nothing here constrains their content — bind them.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};

use super::{
    CallResult, CallValue, CapabilityCall, CapabilityRuntime, DenialReason, PluginRow, check_row,
};

/// The prefix every sandboxed plugin's physical table carries.
///
/// A literal an operator can grep the schema for, and a namespace an
/// application's own tables cannot collide with unless they were named to.
pub const TABLE_PREFIX: &str = "plugin_";

/// The column every plugin row is scoped by.
pub const TENANT_COLUMN: &str = "tenant_id";

/// The column carrying the store-assigned row id.
pub const ID_COLUMN: &str = "row_id";

/// Column names a plugin may not set, because the host owns them.
///
/// Only [`TENANT_COLUMN`]. A row that could carry its own `tenant_id` would be
/// a row that chooses its tenant, which is the containment property written
/// backwards, so it is a hard refusal.
///
/// [`ID_COLUMN`] is *not* here, and that is deliberate rather than an omission:
/// a row comes back from `db-get` carrying its own `row_id`, so the natural
/// read-modify-write echoes it, and refusing that would make the obvious code
/// the wrong code. It is stripped on the way in instead — the id is the row's
/// address and travels in its own field, so a value for it in the row conveys
/// nothing and can override nothing.
pub const RESERVED_COLUMNS: &[&str] = &[TENANT_COLUMN];

/// `PostgreSQL`'s identifier ceiling. A derived name past it would be silently
/// truncated by the server, and two logical tables could then collide.
const MAX_IDENTIFIER_LEN: usize = 63;

/// The physical table one logical table maps to, or `None` if either half is
/// not an identifier this build is willing to concatenate.
///
/// Both halves are re-checked here rather than assumed. `SandboxHost::from_module`
/// is public and takes a manifest an embedder filled in by hand, so "validation
/// already ran" is an invariant a caller can step around — and this is the
/// function whose output goes into a statement.
///
/// # Why the plugin name is escaped rather than folded
///
/// The obvious derivation — lower-case the plugin name and map its punctuation
/// to `_` — is **not injective**, and two different plugins landing on one table
/// is a cross-plugin read and write. It fails in two ways at once:
///
/// | Plugin | Table | Folded name |
/// | --- | --- | --- |
/// | `shop` | `orders_v2` | `plugin_shop_orders_v2` |
/// | `shop_orders` | `v2` | `plugin_shop_orders_v2` |
/// | `my-shop` / `my.shop` / `my_shop` / `My_Shop` | `orders` | `plugin_my_shop_orders` |
///
/// The first row is the one that matters: it has nothing to do with punctuation
/// and everything to do with the separator, so no amount of tidying the plugin
/// name fixes it. A hostile author picks a *name* — which nothing constrains
/// beyond `[A-Za-z0-9._-]` — that shifts the boundary onto a victim plugin's
/// table. Both manifests validate, both consent screens are truthful, and
/// `AppBuilder` sees two distinct plugin names so it mounts both.
///
/// So each half is escaped into a disjoint alphabet and joined by a separator
/// that neither half can contain. `_` and every character outside `[a-z0-9]`
/// become `_<two hex digits>`, which makes the escape injective; `__` is then
/// the one two-character sequence no escaped name can produce, so the split
/// point is unambiguous. `plugin_shop__orders_5fv2` and
/// `plugin_shop_5forders__v2` are different tables, as they must be.
#[must_use]
pub fn physical_table(plugin: &str, table: &str) -> Option<String> {
    if !super::super::grants::is_grantable_ident(table) {
        return None;
    }
    let plugin = escape_identifier_segment(plugin)?;
    // `__` is the separator precisely because `escape_identifier_segment` can
    // never emit it: it emits `_` only as the first character of a three-byte
    // `_xx` escape, and `x` is a hex digit rather than `_`.
    let name = format!("{TABLE_PREFIX}{plugin}__{table}");
    (name.len() <= MAX_IDENTIFIER_LEN).then_some(name)
}

/// Escape one name into `[a-z0-9_]` injectively, or `None` if it is not a name
/// this build derives from at all.
///
/// Injective because the escape is total on everything outside `[a-z0-9]`,
/// including `_` itself: without escaping `_`, the plugin `a_b` and the plugin
/// `a-b` would both come out `a_b`.
fn escape_identifier_segment(name: &str) -> Option<String> {
    // The manifest's own name charset. Checked rather than assumed, for the
    // same reason the table name is.
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return None;
    }
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            out.push(char::from(byte));
        } else {
            // Upper case included: `Shop` and `shop` are different plugin names
            // and must be different tables, but SQL folds unquoted identifiers.
            out.push('_');
            let _ = write!(out, "{byte:02x}");
        }
    }
    // A derived name must still start with a letter: a plugin named `1shop`
    // would otherwise produce a table beginning with a digit. `TABLE_PREFIX`
    // supplies that, so only the total length is left to check, in the caller.
    Some(out)
}

/// Whether `plugin` can own `table` at all — i.e. whether a physical name for
/// the pair fits inside this build's identifier ceiling.
///
/// Exposed so [`SandboxManifest`](crate::plugin_sandbox::SandboxManifest)
/// validation can refuse at *load* a manifest whose `db` grant could only ever be denied at
/// the first call. A capability an operator approved on the consent screen and
/// the runtime can never honour is worse than one that was never offered.
#[must_use]
pub fn is_derivable(plugin: &str, table: &str) -> bool {
    physical_table(plugin, table).is_some()
}

/// Why a store could not do what it was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreError {
    /// The row id does not exist for this plugin, table and tenant.
    NotFound,
    /// The backend failed. The string is for the log and the guest.
    Backend(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("no such row"),
            Self::Backend(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for StoreError {}

/// One already-scoped operation's coordinates.
///
/// A store implementation receives the *physical* table and the tenant, and has
/// no way to reach anything else: it is storage, not policy, and there is no
/// argument through which a caller could ask it for another tenant's rows
/// without saying so in this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Scope {
    /// The derived physical table name.
    pub table: String,
    /// The tenant every row is filtered by and stamped with.
    pub tenant: String,
}

/// Somewhere a sandboxed plugin's rows live.
///
/// Synchronous for the same reason [`OutboundHttp`](super::OutboundHttp) is: the
/// interpreter is, and it already runs on a blocking worker.
pub trait PluginStore: Send + Sync + 'static {
    /// Insert `row`, returning the id assigned to it.
    ///
    /// # Implementor contract
    ///
    /// Three obligations, and the sandbox cannot enforce any of them from the
    /// outside — they are why this trait is a trust boundary the operator
    /// crosses deliberately, like a database driver:
    ///
    /// 1. **Every read and every write MUST be filtered and stamped by
    ///    [`Scope::tenant`].** This is the whole of the tenant containment: the
    ///    wire has no tenant field precisely so that this argument is the only
    ///    place a tenant can come from, and an implementation that ignores it
    ///    leaks every row to every tenant with nothing else to catch it.
    /// 2. **Only [`Scope::table`] may be touched.** It is a bare
    ///    `[a-z][a-z0-9_]*` identifier derived by [`physical_table`]; do not
    ///    derive another name from it.
    /// 3. **Row *values* are guest-chosen and MUST be bound, never
    ///    concatenated.** Column *names* have been checked
    ///    ([`check_row`](super::check_row)); values have not, and nothing here
    ///    constrains their content.
    ///
    /// [`get`](Self::get) and [`query`](Self::query) must also return each row
    /// carrying its [`ID_COLUMN`], which is how the guest addresses it again.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the write did not happen.
    fn insert(&self, scope: &Scope, row: PluginRow) -> Result<String, StoreError>;

    /// The row with this id, if this scope owns one.
    ///
    /// The returned row must carry its [`ID_COLUMN`]; see the contract on
    /// [`insert`](Self::insert).
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the read did not happen. A missing row is
    /// `Ok(None)`, not an error: "no such row" is an answer.
    fn get(&self, scope: &Scope, row_id: &str) -> Result<Option<PluginRow>, StoreError>;

    /// Rows matching every column in `filter`, capped at `limit`.
    ///
    /// Each returned row must carry its [`ID_COLUMN`]; see the contract on
    /// [`insert`](Self::insert).
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the read did not happen.
    fn query(
        &self,
        scope: &Scope,
        filter: &PluginRow,
        limit: usize,
    ) -> Result<Vec<PluginRow>, StoreError>;

    /// Replace the row with this id.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] if this scope owns no such row.
    fn update(&self, scope: &Scope, row_id: &str, row: PluginRow) -> Result<(), StoreError>;

    /// Delete the row with this id.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] if this scope owns no such row.
    fn delete(&self, scope: &Scope, row_id: &str) -> Result<(), StoreError>;
}

/// Answer one `db-*` call. Capability, scope and quota are already checked.
#[allow(
    clippy::too_many_lines,
    reason = "one match arm per operation; splitting it would hide the shared scope derivation"
)]
pub(super) fn perform(
    runtime: &mut CapabilityRuntime,
    call: &CapabilityCall,
    table: &str,
) -> CallResult {
    let id = call.id();
    let Some(store) = runtime.services.db.clone() else {
        return CallResult::denied(
            id,
            DenialReason::Unavailable,
            "this host has no plugin store wired for sandboxed plugins",
        );
    };
    let Some(physical) = physical_table(&runtime.plugin, table) else {
        return CallResult::denied(
            id,
            DenialReason::Malformed,
            format!("no physical table can be derived for {table:?} under this plugin's name"),
        );
    };
    let scope = Scope {
        table: physical,
        tenant: runtime.tenant().to_owned(),
    };
    let row_limit = runtime.quotas().db_rows as usize;

    match call {
        CapabilityCall::DbInsert { row, .. } => match validated_row(row) {
            Err(result) => result(id),
            Ok(row) => match store.insert(&scope, row) {
                Ok(row_id) => CallResult::Ok {
                    id,
                    value: CallValue::RowId { row_id },
                },
                Err(err) => CallResult::denied(id, DenialReason::BackendError, err.to_string()),
            },
        },
        CapabilityCall::DbGet { row_id, .. } => {
            if let Err(result) = check_row_id(row_id) {
                return result(id);
            }
            match store.get(&scope, row_id) {
                Ok(found) => CallResult::Ok {
                    id,
                    value: CallValue::Rows {
                        rows: found.into_iter().collect(),
                    },
                },
                Err(err) => CallResult::denied(id, DenialReason::BackendError, err.to_string()),
            }
        }
        CapabilityCall::DbQuery { filter, limit, .. } => match validated_row(filter) {
            Err(result) => result(id),
            Ok(filter) => {
                // A limit of zero means "as many as the quota allows", which is
                // what a guest that omits the field gets. Anything larger is
                // clamped rather than refused: the quota is the operator's
                // ceiling, and a plugin asking past it should get the ceiling.
                let want = if *limit == 0 {
                    row_limit
                } else {
                    (*limit as usize).min(row_limit)
                };
                match store.query(&scope, &filter, want) {
                    Ok(mut rows) => {
                        rows.truncate(want);
                        CallResult::Ok {
                            id,
                            value: CallValue::Rows { rows },
                        }
                    }
                    Err(err) => CallResult::denied(id, DenialReason::BackendError, err.to_string()),
                }
            }
        },
        CapabilityCall::DbUpdate { row_id, row, .. } => {
            if let Err(result) = check_row_id(row_id) {
                return result(id);
            }
            match validated_row(row) {
                Err(result) => result(id),
                Ok(row) => match store.update(&scope, row_id, row) {
                    Ok(()) => CallResult::Ok {
                        id,
                        value: CallValue::Done,
                    },
                    Err(err) => CallResult::denied(id, DenialReason::BackendError, err.to_string()),
                },
            }
        }
        CapabilityCall::DbDelete { row_id, .. } => {
            if let Err(result) = check_row_id(row_id) {
                return result(id);
            }
            match store.delete(&scope, row_id) {
                Ok(()) => CallResult::Ok {
                    id,
                    value: CallValue::Done,
                },
                Err(err) => CallResult::denied(id, DenialReason::BackendError, err.to_string()),
            }
        }
        _ => CallResult::denied(id, DenialReason::Malformed, "not a db call"),
    }
}

/// A row with every column checked, or the denial to answer with.
///
/// The closure defers only the correlation id, so each call site reads as one
/// expression rather than repeating the id into six error constructions.
type Denial = Box<dyn FnOnce(u64) -> CallResult>;

fn validated_row(row: &PluginRow) -> Result<PluginRow, Denial> {
    if let Err(detail) = check_row(row) {
        return Err(Box::new(move |id| {
            CallResult::denied(id, DenialReason::Malformed, detail)
        }));
    }
    for column in row.keys() {
        if RESERVED_COLUMNS.contains(&column.as_str()) {
            let column = column.clone();
            return Err(Box::new(move |id| {
                CallResult::denied(
                    id,
                    DenialReason::NotInGrant,
                    format!(
                        "column {column:?} is the host's: a row that could set it would be a row \
                         that chooses its own tenant or identity"
                    ),
                )
            }));
        }
    }
    let mut row = row.clone();
    // Stripped rather than refused; see `RESERVED_COLUMNS`.
    row.remove(ID_COLUMN);
    Ok(row)
}

fn check_row_id(row_id: &str) -> Result<(), Denial> {
    if row_id.is_empty() || row_id.len() > super::MAX_ROW_ID_BYTES {
        let max = super::MAX_ROW_ID_BYTES;
        return Err(Box::new(move |id| {
            CallResult::denied(
                id,
                DenialReason::Malformed,
                format!("a row id must be 1..={max} bytes"),
            )
        }));
    }
    Ok(())
}

// ── An in-process store ──────────────────────────────────────────────────

/// A [`PluginStore`] held in a map.
///
/// Exists so the containment properties above can be proven in an ordinary
/// `cargo test` — an adversarial corpus that needs Postgres is a corpus that
/// runs on somebody else's schedule — and so a single-process deployment has
/// something to wire.
///
/// It is a faithful model of the **scoping**, not of the durability or of the
/// performance: rows are keyed by `(table, tenant, row_id)` exactly as a scoped
/// statement would filter them, so a query that would return another tenant's
/// rows here would return them there too — but `query` is a linear scan of every
/// row in the map rather than an index seek, and nothing here is written to
/// disk. It is **bounded**, for the reason [`MemoryKvStore`](super::kv::MemoryKvStore)
/// is: a plugin declares its own `db_writes` quota, so an unbounded store makes
/// the host's memory the real ceiling.
#[derive(Debug)]
pub struct MemoryPluginStore {
    rows: Mutex<HashMap<(String, String, String), PluginRow>>,
    next: Mutex<u64>,
    capacity: usize,
}

/// Rows a [`MemoryPluginStore::new`] store will hold, across every table and
/// tenant.
pub const DEFAULT_STORE_CAPACITY: usize = 10_000;

impl Default for MemoryPluginStore {
    fn default() -> Self {
        Self {
            rows: Mutex::new(HashMap::new()),
            next: Mutex::new(0),
            capacity: DEFAULT_STORE_CAPACITY,
        }
    }
}

impl MemoryPluginStore {
    /// An empty store holding at most [`DEFAULT_STORE_CAPACITY`] rows.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// An empty store holding at most `capacity` rows.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            rows: Mutex::new(HashMap::new()),
            next: Mutex::new(0),
            capacity,
        })
    }

    /// Put a row in directly, bypassing the plugin path.
    ///
    /// For a test that needs a host-application row, or another tenant's row, to
    /// exist before it proves a plugin cannot reach it.
    pub fn seed(&self, table: &str, tenant: &str, row_id: &str, row: PluginRow) {
        self.rows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                (table.to_owned(), tenant.to_owned(), row_id.to_owned()),
                row,
            );
    }

    /// Every `(table, tenant, row_id)` currently stored, sorted.
    #[must_use]
    pub fn keys(&self) -> Vec<(String, String, String)> {
        let rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        let mut keys: Vec<_> = rows.keys().cloned().collect();
        keys.sort();
        keys
    }
}

impl PluginStore for MemoryPluginStore {
    fn insert(&self, scope: &Scope, row: PluginRow) -> Result<String, StoreError> {
        let mut rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        if rows.len() >= self.capacity {
            return Err(StoreError::Backend(format!(
                "this host's in-memory plugin store is full at {} rows",
                self.capacity
            )));
        }
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        *next = next.saturating_add(1);
        let row_id = format!("r{next}");
        drop(next);
        rows.insert(
            (scope.table.clone(), scope.tenant.clone(), row_id.clone()),
            row,
        );
        Ok(row_id)
    }

    fn get(&self, scope: &Scope, row_id: &str) -> Result<Option<PluginRow>, StoreError> {
        Ok(self
            .rows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(scope.table.clone(), scope.tenant.clone(), row_id.to_owned()))
            .cloned()
            .map(|row| with_id(row, row_id)))
    }

    fn query(
        &self,
        scope: &Scope,
        filter: &PluginRow,
        limit: usize,
    ) -> Result<Vec<PluginRow>, StoreError> {
        let rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        // Sorted by row id so a query is a deterministic function of the store,
        // which is what lets a test assert on the rows rather than on their set.
        let mut matched: Vec<(String, PluginRow)> = rows
            .iter()
            .filter(|((table, tenant, _), _)| *table == scope.table && *tenant == scope.tenant)
            .filter(|(_, row)| {
                filter
                    .iter()
                    .all(|(column, want)| row.get(column) == Some(want))
            })
            .map(|((_, _, row_id), row)| (row_id.clone(), row.clone()))
            .collect();
        matched.sort_by(|(left, _), (right, _)| left.cmp(right));
        matched.truncate(limit);
        Ok(matched
            .into_iter()
            .map(|(row_id, row)| with_id(row, &row_id))
            .collect())
    }

    fn update(&self, scope: &Scope, row_id: &str, row: PluginRow) -> Result<(), StoreError> {
        let key = (scope.table.clone(), scope.tenant.clone(), row_id.to_owned());
        let mut rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        if !rows.contains_key(&key) {
            return Err(StoreError::NotFound);
        }
        rows.insert(key, row);
        Ok(())
    }

    fn delete(&self, scope: &Scope, row_id: &str) -> Result<(), StoreError> {
        let key = (scope.table.clone(), scope.tenant.clone(), row_id.to_owned());
        self.rows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key)
            .map(|_| ())
            .ok_or(StoreError::NotFound)
    }
}

/// Stamp the store-assigned id onto a row on the way out.
///
/// The guest needs it to address the row again, and it is the host's column, so
/// it is added here rather than being something a store has to remember to
/// include.
fn with_id(mut row: PluginRow, row_id: &str) -> PluginRow {
    row.insert(
        ID_COLUMN.to_owned(),
        super::PluginValue::Text(row_id.to_owned()),
    );
    row
}
