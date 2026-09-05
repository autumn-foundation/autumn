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
    ///
    /// The *derived* tenant segment, not the raw id — see
    /// [`tenant_segment`](super::tenant_segment). A raw id cannot say whether
    /// there was one, so a deployment with a tenant named `-` would share a
    /// namespace with its single-tenant requests. Store it as it arrives; do
    /// not decode it.
    pub tenant: String,
}

/// Somewhere a sandboxed plugin's rows live.
///
/// Synchronous for the same reason [`OutboundHttp`](super::OutboundHttp) is: the
/// interpreter is, and it already runs on a blocking worker.
/// One page of a `db-query`.
///
/// A bare `Vec` could not carry the one thing the guest needs and cannot infer:
/// whether the host's byte ceiling stopped the answer short. A short page and a
/// small table look identical from the guest, and a plugin paging through its
/// own table would read the first as the second.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct QueryPage {
    /// The rows, each carrying its [`ID_COLUMN`].
    pub rows: Vec<PluginRow>,
    /// Whether a matching row was left out because `max_bytes` was reached.
    ///
    /// Not set for rows left out by `limit`: the guest chose that number and
    /// gets a full page back, so it already knows to ask again.
    pub truncated: bool,
}

impl QueryPage {
    /// A page that carries every row that matched.
    #[must_use]
    pub const fn complete(rows: Vec<PluginRow>) -> Self {
        Self {
            rows,
            truncated: false,
        }
    }
}

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

    /// Rows matching every column in `filter`, capped at `limit` rows and
    /// `max_bytes` of row weight, resuming after `after`.
    ///
    /// **Rows come back in ascending [`ID_COLUMN`] order, and `after` excludes
    /// every id at or below it.** That ordering is the whole of paging: a page
    /// can end early on either cap, and the guest continues by passing the last
    /// `row_id` it saw. An implementation free to return matches in any order
    /// would make `after` meaningless and the rows behind a truncated page
    /// unreachable.
    ///
    /// Each returned row must carry its [`ID_COLUMN`]; see the contract on
    /// [`insert`](Self::insert).
    ///
    /// # Implementor contract
    ///
    /// **Stop reading once the rows gathered so far would exceed `max_bytes`**
    /// ([`row_weight`](super::row_weight) is the measure), rather than
    /// gathering `limit` rows and letting the caller discard the excess. Both
    /// numbers are guest-influenced — `limit` comes from the frame and the
    /// quota, and row size from what the plugin previously stored — so an
    /// implementation that materialises first has already spent the memory the
    /// ceiling exists to deny. Returning fewer rows than `limit` is always
    /// allowed — say so in [`QueryPage::truncated`], which is what the guest
    /// reads to tell "that is all of them" from "that is all that fits".
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the read did not happen.
    fn query(
        &self,
        scope: &Scope,
        filter: &PluginRow,
        limit: usize,
        max_bytes: usize,
        after: Option<&str>,
    ) -> Result<QueryPage, StoreError>;

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
    runtime: &CapabilityRuntime,
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
        tenant: runtime.tenant_key(),
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
                Ok(found) => {
                    // Through the same bound as a query, though a single row is
                    // under `MAX_ROW_BYTES` and therefore always survives it: a
                    // store is an embedder's implementation, and a row it
                    // returns larger than one it would have accepted is exactly
                    // the case that must not reach the reply queue.
                    let (rows, truncated) = super::bounded_rows(found.into_iter().collect());
                    CallResult::Ok {
                        id,
                        value: CallValue::Rows { rows, truncated },
                    }
                }
                Err(err) => CallResult::denied(id, DenialReason::BackendError, err.to_string()),
            }
        }
        CapabilityCall::DbQuery {
            filter,
            limit,
            after,
            ..
        } => match validated_filter(filter) {
            Err(result) => result(id),
            Ok(filter) => {
                if let Some(after) = after
                    && let Err(result) = check_row_id(after)
                {
                    return result(id);
                }
                // A limit of zero means "as many as the quota allows", which is
                // what a guest that omits the field gets. Anything larger is
                // clamped rather than refused: the quota is the operator's
                // ceiling, and a plugin asking past it should get the ceiling.
                let want = if *limit == 0 {
                    row_limit
                } else {
                    (*limit as usize).min(row_limit)
                };
                match store.query(
                    &scope,
                    &filter,
                    want,
                    super::MAX_RESULT_BYTES,
                    after.as_deref(),
                ) {
                    Ok(page) => {
                        let mut rows = page.rows;
                        rows.truncate(want);
                        // The store's own report, OR'd with a re-check of what
                        // it handed back: the trait is an embedder's to
                        // implement, so "the host does not hold more than this"
                        // cannot rest on someone else's loop honouring the
                        // budget it was given.
                        let (rows, cut) = super::bounded_rows(rows);
                        CallResult::Ok {
                            id,
                            value: CallValue::Rows {
                                rows,
                                truncated: page.truncated || cut,
                            },
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
    // The host's own column comes off *before* the guest's ceilings are applied
    // to what is left. `db-get` hands back the row plus its `row_id`, and the
    // documented read-modify-write is to change a field and send that map to
    // `db-update` — so a row stored at exactly `MAX_ROW_COLUMNS`, or near
    // `MAX_ROW_BYTES`, would be refused on the way back in for carrying a column
    // the host added itself. Counting it would make the ceilings a function of
    // where the row had been rather than of what the plugin wrote.
    let mut row = row.clone();
    row.remove(ID_COLUMN);
    let row = &row;
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
    // Already stripped above, before the ceilings ran; see `RESERVED_COLUMNS`
    // for why it is stripped rather than refused.
    Ok(row.clone())
}

/// The same checks for a `db-query` *filter*, where stripping is not an option.
///
/// [`validated_row`] strips [`ID_COLUMN`] because a row echoing the id it was
/// read with is the obvious code and means nothing on a write. On a filter it
/// means everything: stripping it turns "the row with this id" into "every row
/// this tenant has", so a guest narrowing a query would silently widen it — the
/// one direction a containment bug can go and still look like it worked.
/// Refused instead, naming the call that does take an id.
fn validated_filter(row: &PluginRow) -> Result<PluginRow, Denial> {
    if row.contains_key(ID_COLUMN) {
        return Err(Box::new(move |id| {
            CallResult::denied(
                id,
                DenialReason::Malformed,
                format!(
                    "a query filter may not carry {ID_COLUMN:?}; use `db-get` to read one row                      by its id"
                ),
            )
        }));
    }
    validated_row(row)
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
    /// The rows and the bytes they hold, under one lock — see `MemoryJobSink`
    /// for why the total lives beside the data it describes.
    rows: Mutex<StoredRows>,
    next: Mutex<u64>,
    capacity: usize,
    byte_capacity: usize,
}

/// The stored rows and their running weight.
#[derive(Debug, Default)]
struct StoredRows {
    map: HashMap<(String, String, String), PluginRow>,
    bytes: usize,
}

/// Rows a [`MemoryPluginStore::new`] store will hold, across every table and
/// tenant.
pub const DEFAULT_STORE_CAPACITY: usize = 10_000;

/// Bytes of rows a [`MemoryPluginStore::new`] store will hold.
///
/// A row count is not a memory bound: every accepted row may carry
/// [`MAX_ROW_BYTES`](super::MAX_ROW_BYTES), so `DEFAULT_STORE_CAPACITY` rows is
/// gigabytes, reachable in under a minute at the default call rate. The unit an
/// operator budgets in is bytes, so that is the ceiling this store enforces
/// alongside its row count.
pub const DEFAULT_STORE_BYTE_CAPACITY: usize = 64 * 1024 * 1024;

impl Default for MemoryPluginStore {
    fn default() -> Self {
        Self {
            rows: Mutex::new(StoredRows::default()),
            next: Mutex::new(0),
            capacity: DEFAULT_STORE_CAPACITY,
            byte_capacity: DEFAULT_STORE_BYTE_CAPACITY,
        }
    }
}

impl MemoryPluginStore {
    /// What one stored row costs: its value *and* its key.
    ///
    /// The key is three owned strings — physical table, tenant, row id — cloned
    /// once per row. A ceiling that summed only values therefore charged an
    /// empty row nothing while it retained a tenant id per row, and a tenant id
    /// arrives in a header with no length validation of its own.
    fn entry_weight(key: &(String, String, String), row: &PluginRow) -> usize {
        /// A `HashMap` bucket plus three `String` headers, near enough.
        const PER_ENTRY: usize = 128;
        key.0
            .len()
            .saturating_add(key.1.len())
            .saturating_add(key.2.len())
            .saturating_add(super::row_weight(row))
            .saturating_add(PER_ENTRY)
    }

    /// An empty store holding at most [`DEFAULT_STORE_CAPACITY`] rows.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// An empty store holding at most `capacity` rows.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            rows: Mutex::new(StoredRows::default()),
            next: Mutex::new(0),
            capacity,
            byte_capacity: DEFAULT_STORE_BYTE_CAPACITY,
        })
    }

    /// An empty store bounded by both a row count and a total size.
    #[must_use]
    pub fn with_capacities(capacity: usize, byte_capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            rows: Mutex::new(StoredRows::default()),
            next: Mutex::new(0),
            capacity,
            byte_capacity,
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
            .map
            .insert(
                (table.to_owned(), tenant.to_owned(), row_id.to_owned()),
                row,
            );
    }

    /// Every `(table, tenant, row_id)` currently stored, sorted.
    #[must_use]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the body is one critical section over the shared map; releasing early would \
                  either split a ceiling check from the write it guards or let the snapshot \
                  this returns be torn"
    )]
    pub fn keys(&self) -> Vec<(String, String, String)> {
        let rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        let mut keys: Vec<_> = rows.map.keys().cloned().collect();
        keys.sort();
        keys
    }
}

impl PluginStore for MemoryPluginStore {
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the body is one critical section over the shared map; releasing early would \
                  either split a ceiling check from the write it guards or let the snapshot \
                  this returns be torn"
    )]
    fn insert(&self, scope: &Scope, row: PluginRow) -> Result<String, StoreError> {
        let mut rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        if rows.map.len() >= self.capacity {
            return Err(StoreError::Backend(format!(
                "this host's in-memory plugin store is full at {} rows",
                self.capacity
            )));
        }
        // And by size, which is the ceiling that actually bounds memory: a row
        // count times `MAX_ROW_BYTES` is gigabytes.
        // Against the running total rather than a fresh scan: re-weighing every
        // row on each insert made a full store the expensive case, and the scan
        // ran while holding this lock.
        let incoming_key = (
            scope.table.clone(),
            scope.tenant.clone(),
            String::from("r0000000000"),
        );
        let incoming = Self::entry_weight(&incoming_key, &row);
        if rows.bytes.saturating_add(incoming) > self.byte_capacity {
            return Err(StoreError::Backend(format!(
                "this host's in-memory plugin store is full at {} bytes",
                self.byte_capacity
            )));
        }
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        *next = next.saturating_add(1);
        let row_id = format!("r{next}");
        drop(next);
        rows.bytes = rows.bytes.saturating_add(incoming);
        rows.map.insert(
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
            .map
            .get(&(scope.table.clone(), scope.tenant.clone(), row_id.to_owned()))
            .cloned()
            .map(|row| with_id(row, row_id)))
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the body is one critical section over the shared map; releasing early would \
                  either split a ceiling check from the write it guards or let the snapshot \
                  this returns be torn"
    )]
    fn query(
        &self,
        scope: &Scope,
        filter: &PluginRow,
        limit: usize,
        max_bytes: usize,
        after: Option<&str>,
    ) -> Result<QueryPage, StoreError> {
        let rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        // Ids first, values second. Sorting borrowed ids rather than cloned
        // rows is what lets the byte budget be honoured *before* a row is
        // cloned: gathering every match and truncating afterwards would
        // materialise the megabytes this argument exists to refuse.
        //
        // Sorted so a query is a deterministic function of the store, which is
        // what lets a test assert on the rows rather than on their set.
        let mut matched: Vec<&(String, String, String)> = rows
            .map
            .iter()
            .filter(|((table, tenant, _), _)| *table == scope.table && *tenant == scope.tenant)
            .filter(|(_, row)| {
                filter
                    .iter()
                    .all(|(column, want)| row.get(column) == Some(want))
            })
            .filter(|((_, _, row_id), _)| after.is_none_or(|after| row_id.as_str() > after))
            .map(|(key, _)| key)
            .collect();
        matched.sort_by(|left, right| left.2.cmp(&right.2));
        // A match beyond `limit` is a cut as much as a match beyond
        // `max_bytes`, and the guest cannot tell it happened: with `limit: 0`
        // the row cap is the *quota*, which the guest is never sent, so a full
        // page and a finished table look identical to it. One extra look is what
        // makes `truncated` mean "there is more" rather than "the bytes ran
        // out".
        let mut truncated = matched.len() > limit;
        let mut out = Vec::new();
        let mut total = 0_usize;
        for key in matched.into_iter().take(limit) {
            let Some(row) = rows.map.get(key) else {
                continue;
            };
            let weight = super::row_weight(row);
            if !out.is_empty() && total.saturating_add(weight) > max_bytes {
                // A row that matched and was left out. Reported rather than
                // merely omitted: the caller cannot see the difference between
                // this and a table with nothing more in it, and neither can the
                // guest it answers.
                truncated = true;
                break;
            }
            total = total.saturating_add(weight);
            out.push(with_id(row.clone(), &key.2));
        }
        Ok(QueryPage {
            rows: out,
            truncated,
        })
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the body is one critical section over the shared map; releasing early would \
                  either split a ceiling check from the write it guards or let the snapshot \
                  this returns be torn"
    )]
    fn update(&self, scope: &Scope, row_id: &str, row: PluginRow) -> Result<(), StoreError> {
        let key = (scope.table.clone(), scope.tenant.clone(), row_id.to_owned());
        let mut rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(existing) = rows.map.get(&key) else {
            return Err(StoreError::NotFound);
        };
        // The byte ceiling applies to a *replacement* as much as to an insert.
        // An update adds no row, so a check that only ran on `insert` left the
        // store growable without bound: replacing each of many small rows with
        // one near `MAX_ROW_BYTES` is the same memory, arrived at by the path
        // that was not looking. The outgoing row's weight comes off first, so
        // rewriting a row in place is never refused for the size it already
        // was.
        let outgoing = Self::entry_weight(&key, existing);
        let incoming = Self::entry_weight(&key, &row);
        if rows.bytes.saturating_sub(outgoing).saturating_add(incoming) > self.byte_capacity {
            return Err(StoreError::Backend(format!(
                "this host's in-memory plugin store is full at {} bytes",
                self.byte_capacity
            )));
        }
        rows.bytes = rows.bytes.saturating_sub(outgoing).saturating_add(incoming);
        rows.map.insert(key, row);
        Ok(())
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the removal and the total it changes are one critical section; releasing \
                  between them would let a reader see a row gone and its bytes still charged"
    )]
    fn delete(&self, scope: &Scope, row_id: &str) -> Result<(), StoreError> {
        let key = (scope.table.clone(), scope.tenant.clone(), row_id.to_owned());
        let mut rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(removed) = rows.map.remove(&key) else {
            return Err(StoreError::NotFound);
        };
        // The total follows the map, or a long-running store ratchets shut.
        let freed = Self::entry_weight(&key, &removed);
        rows.bytes = rows.bytes.saturating_sub(freed);
        Ok(())
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
