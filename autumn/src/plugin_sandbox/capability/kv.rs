//! Per-tenant key/value storage for a sandboxed plugin (issue #1632).
//!
//! The whole capability is one idea: **the guest names a key, the host names the
//! namespace**. A guest asking for `cart` gets
//! `plugin-kv:<plugin>:<tenant>:cart`, and there is no field in the wire
//! protocol where a different plugin or a different tenant would go. Tenant A's
//! data is not "denied" to tenant B's request — it is unnameable from it.
//!
//! # Why the segments are escaped
//!
//! `format!("{plugin}:{tenant}:{key}")` is the version of this that has a
//! cross-tenant read in it. Plugin names are validated (`[A-Za-z0-9._-]`), but
//! **tenant ids are not** — they come from a header, a subdomain or a session,
//! and an application decides their shape. A tenant literally called
//! `b:secret` would, unescaped, make its key `…:a:b:secret:k` — the same string
//! tenant `a` produces for the key `b:secret:k`, which tenant `a` is free to
//! ask for.
//!
//! So every segment is escaped before it is joined: `%` becomes `%25` and `:`
//! becomes `%3A`. After that the joined string parses back to exactly one tuple
//! of segments, which is what makes the namespace a partition rather than a
//! naming convention.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use super::{CallResult, CallValue, CapabilityCall, CapabilityRuntime, DenialReason, PluginValue};

/// The prefix every sandboxed plugin's cache key carries.
///
/// A literal an operator can grep the cache for, and a namespace the
/// application's own `#[cached]` keys cannot collide with — those are built by
/// `make_cache_key` from a function path, which cannot start with this.
pub const KV_PREFIX: &str = "plugin-kv";

/// Somewhere a plugin's key/value pairs live.
///
/// The keys handed to an implementation are **already namespaced**: an
/// implementation is storage, not policy, and nothing about the plugin or the
/// tenant is recoverable from what it is asked to do beyond what is in the key.
pub trait KvStore: Send + Sync + 'static {
    /// Read one already-namespaced key.
    fn get(&self, key: &str) -> Option<PluginValue>;
    /// Write one already-namespaced key.
    ///
    /// # Errors
    ///
    /// One line for the guest and the audit ledger when the write did not
    /// happen — a full store, an unreachable backend. A store that cannot say
    /// no is one whose ceiling is the host's memory.
    fn set(&self, key: &str, value: PluginValue) -> Result<(), String>;
    /// Delete one already-namespaced key.
    fn delete(&self, key: &str);
}

/// Escape one namespace segment so the joined key parses back to exactly one
/// tuple of segments.
///
/// `%` first, then `:`. The order matters: escaping `:` first would turn a
/// literal `%3A` in the input into `%3A` in the output — indistinguishable from
/// an escaped colon — and the injectivity this function exists for would be
/// gone.
#[must_use]
pub fn escape_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for ch in segment.chars() {
        match ch {
            '%' => out.push_str("%25"),
            ':' => out.push_str("%3A"),
            other => out.push(other),
        }
    }
    out
}

/// The physical cache key for one logical key.
///
/// Every segment but the last is the host's; the last is the guest's, and it is
/// escaped exactly like the others so a key of `a:b` cannot masquerade as a
/// namespace boundary.
///
/// The tenant arrives as an `Option` rather than a string because "no tenant"
/// has to be a segment no tenant could be named: see
/// [`tenant_segment`](super::tenant_segment).
#[must_use]
pub fn namespaced_key(plugin: &str, tenant: Option<&str>, key: &str) -> String {
    format!(
        "{KV_PREFIX}:{plugin}:{tenant}:{key}",
        plugin = escape_segment(plugin),
        tenant = escape_segment(&super::tenant_segment(tenant)),
        key = escape_segment(key)
    )
}

/// Answer one `kv-*` call. Capability, scope and quota are already checked.
pub(super) fn perform(runtime: &CapabilityRuntime, call: &CapabilityCall, key: &str) -> CallResult {
    let id = call.id();
    let Some(store) = runtime.services.kv.clone() else {
        return CallResult::denied(
            id,
            DenialReason::Unavailable,
            "this host has no key/value backend wired for sandboxed plugins",
        );
    };
    let physical = namespaced_key(&runtime.plugin, runtime.tenant(), key);
    match call {
        CapabilityCall::KvGet { .. } => {
            let ceiling = runtime.quotas().kv_value_bytes as usize;
            match store.get(&physical) {
                // Checked on the way *out* as well as in. A quota is the
                // operator's current answer, and lowering one is meant to
                // *reduce* authority — but the store keeps what was written
                // under the old ceiling, so without this an upgrade that
                // tightened `kv_value_bytes` would go on serving values over
                // it, and a value large enough could overrun the reply queue
                // and fail the request outright rather than being refused.
                Some(value) if value.weight() > ceiling => CallResult::denied(
                    id,
                    DenialReason::QuotaExceeded,
                    format!(
                        "the stored value is {} bytes, over this plugin's current {ceiling}-byte \
                         `kv_value_bytes` ceiling",
                        value.weight()
                    ),
                ),
                Some(value) => CallResult::Ok {
                    id,
                    value: CallValue::Value { value, found: true },
                },
                None => CallResult::Ok {
                    id,
                    value: CallValue::Value {
                        value: PluginValue::Null,
                        found: false,
                    },
                },
            }
        }
        CapabilityCall::KvSet { value, .. } => {
            let ceiling = runtime.quotas().kv_value_bytes as usize;
            if value.weight() > ceiling {
                return CallResult::denied(
                    id,
                    DenialReason::QuotaExceeded,
                    format!("a kv value may hold at most {ceiling} bytes"),
                );
            }
            match store.set(&physical, value.clone()) {
                Ok(()) => CallResult::Ok {
                    id,
                    value: CallValue::Done,
                },
                Err(detail) => CallResult::denied(id, DenialReason::BackendError, detail),
            }
        }
        CapabilityCall::KvDelete { .. } => {
            store.delete(&physical);
            CallResult::Ok {
                id,
                value: CallValue::Done,
            }
        }
        // `perform` is only reached from the KV arm of `CapabilityRuntime::perform`.
        _ => CallResult::denied(id, DenialReason::Malformed, "not a kv call"),
    }
}

// ── An in-process store ──────────────────────────────────────────────────

/// A `KvStore` held in a map, for tests and single-process deployments.
///
/// Not a cache — nothing expires — but it is **bounded**, because an unbounded
/// one is not something to hand a plugin. A plugin declares its own
/// `kv_writes` and `kv_value_bytes` quotas in a manifest an operator approves,
/// and both may legally be `MAX_QUOTA`; a store with no ceiling of its own
/// turns that into the host's memory. Past [`capacity`](Self::with_capacity) a
/// write is refused and the guest is told, rather than the process dying.
///
/// It exists so the containment properties this module claims can be proven
/// without standing up Redis, and so a small deployment has something to wire.
/// A deployment that outgrows it wants [`CacheKvStore`], which inherits Moka's
/// or Redis's eviction.
#[derive(Debug)]
pub struct MemoryKvStore {
    entries: Mutex<HashMap<String, PluginValue>>,
    capacity: usize,
    byte_capacity: usize,
}

/// Keys a [`MemoryKvStore::new`] store will hold.
///
/// Generous for the panel-rendering plugin this store is meant for, and small
/// enough that reaching it is a bug report rather than an outage.
pub const DEFAULT_KV_CAPACITY: usize = 10_000;

/// Bytes of keys and values a [`MemoryKvStore::new`] store will hold.
///
/// A key count is not a memory bound. `DEFAULT_KV_CAPACITY` entries at the
/// default 64 KiB `kv_value_bytes` is about 625 MiB, and at the largest legal
/// quota it is gigabytes — a granted plugin reaches that in under a minute at
/// the default call rate. What an operator can reason about is *how much
/// memory this store may take*, so that is what it is given.
pub const DEFAULT_KV_BYTE_CAPACITY: usize = 32 * 1024 * 1024;

impl Default for MemoryKvStore {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity: DEFAULT_KV_CAPACITY,
            byte_capacity: DEFAULT_KV_BYTE_CAPACITY,
        }
    }
}

impl MemoryKvStore {
    /// An empty store holding at most [`DEFAULT_KV_CAPACITY`] keys.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// An empty store holding at most `capacity` keys, with the default byte
    /// ceiling.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            capacity,
            byte_capacity: DEFAULT_KV_BYTE_CAPACITY,
        })
    }

    /// An empty store bounded by both a key count and a total size.
    #[must_use]
    pub fn with_capacities(capacity: usize, byte_capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            capacity,
            byte_capacity,
        })
    }

    /// How many keys this store will hold.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many bytes of keys and values this store will hold.
    #[must_use]
    pub const fn byte_capacity(&self) -> usize {
        self.byte_capacity
    }

    /// What one entry costs: its key, its value, and the map's own per-entry
    /// overhead, which a store measuring only payloads would charge at nothing.
    const fn entry_weight(key: &str, value: &PluginValue) -> usize {
        /// A `HashMap` bucket plus two `String` headers, near enough.
        const PER_ENTRY: usize = 96;
        key.len()
            .saturating_add(value.weight())
            .saturating_add(PER_ENTRY)
    }

    /// Every key currently stored, for assertions.
    #[must_use]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the body is one critical section over the shared map; releasing early would \
                  either split a ceiling check from the write it guards or let the snapshot \
                  this returns be torn"
    )]
    pub fn keys(&self) -> Vec<String> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let mut keys: Vec<String> = entries.keys().cloned().collect();
        keys.sort();
        keys
    }
}

impl KvStore for MemoryKvStore {
    fn get(&self, key: &str) -> Option<PluginValue> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    #[allow(
        clippy::significant_drop_tightening,
        reason = "the body is one critical section over the shared map; releasing early would \
                  either split a ceiling check from the write it guards or let the snapshot \
                  this returns be torn"
    )]
    fn set(&self, key: &str, value: PluginValue) -> Result<(), String> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        // Overwriting an existing key adds no *entry*, so the key ceiling
        // applies to new keys only — a plugin that keeps one counter updated is
        // never refused however long it runs.
        if entries.len() >= self.capacity && !entries.contains_key(key) {
            return Err(format!(
                "this host's plugin key/value store is full at {} keys",
                self.capacity
            ));
        }
        // The byte ceiling is the one that actually bounds memory, and it has to
        // account for a *replacement* too: overwriting a small value with a huge
        // one adds no key and can still take the store past its size.
        let incoming = Self::entry_weight(key, &value);
        let outgoing = entries
            .get(key)
            .map_or(0, |existing| Self::entry_weight(key, existing));
        let held: usize = entries
            .iter()
            .map(|(stored_key, stored)| Self::entry_weight(stored_key, stored))
            .fold(0, usize::saturating_add);
        if held.saturating_sub(outgoing).saturating_add(incoming) > self.byte_capacity {
            return Err(format!(
                "this host's plugin key/value store is full at {} bytes",
                self.byte_capacity
            ));
        }
        entries.insert(key.to_owned(), value);
        Ok(())
    }

    fn delete(&self, key: &str) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }
}

// ── The framework cache ──────────────────────────────────────────────────

/// A `KvStore` backed by the application's own [`Cache`](crate::cache::Cache).
///
/// Which is the point of binding the capability to a subsystem that already
/// exists: a plugin's KV inherits whatever the operator already chose — Moka in
/// process, Redis across replicas — and the eviction, TTL and metrics that come
/// with it, rather than growing a second storage system nobody configured.
#[derive(Clone)]
pub struct CacheKvStore(pub std::sync::Arc<dyn crate::cache::Cache>);

impl std::fmt::Debug for CacheKvStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CacheKvStore")
    }
}

impl KvStore for CacheKvStore {
    // `get_cached`/`insert_cached` rather than `get_value`/`insert_value`, and
    // the difference is the whole of whether this type works at all against the
    // cross-replica backend it exists to reach. The erased pair is the
    // *in-process* half of the `Cache` trait: a serializing backend implements
    // `insert_value` as a no-op and answers `get_value` with `RawCacheBytes`,
    // which no `downcast_ref::<PluginValue>` can see. Over Redis this stored
    // nothing, reported success, and missed every read afterwards — a plugin
    // told its write succeeded, on the one backend the module documents for
    // more than one replica. The serde pair takes both paths: the downcast when
    // the backend keeps values, JSON when it keeps bytes.
    fn get(&self, key: &str) -> Option<PluginValue> {
        crate::cache::get_cached::<PluginValue>(self.0.as_ref(), key)
    }

    fn set(&self, key: &str, value: PluginValue) -> Result<(), String> {
        // No TTL: a plugin's KV entry lives until the plugin deletes it or the
        // backend evicts it. A ceiling on *lifetime* is not one of the grants an
        // operator gave, and inventing one here would make a plugin's data
        // disappear on a schedule nothing in the manifest mentions.
        crate::cache::insert_cached(self.0.as_ref(), key, value, None);
        Ok(())
    }

    fn delete(&self, key: &str) {
        self.0.invalidate(key);
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A `Cache` that keeps only *bytes*, the way a cross-replica backend does.
    ///
    /// The shape that made the bug invisible: an in-process backend answers
    /// `get_value` with the value itself, so a downcast works and a test written
    /// against Moka passes while Redis silently stores nothing. This one drops
    /// the erased value and keeps only what was serialized — exactly what
    /// `cache/layer.rs` does — so an adapter relying on the downcast fails here.
    #[derive(Debug, Default)]
    struct SerializingCache {
        bytes: Mutex<std::collections::HashMap<String, Vec<u8>>>,
    }

    impl crate::cache::Cache for SerializingCache {
        fn get_value(&self, key: &str) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
            let bytes = self
                .bytes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(key)
                .cloned()?;
            Some(Arc::new(crate::cache::RawCacheBytes(bytes)))
        }

        /// Dropped, not stored. This is the line that made `kv-set` a no-op.
        fn insert_value(&self, _key: &str, _value: Arc<dyn std::any::Any + Send + Sync>) {}

        fn insert_raw_bytes(&self, key: &str, bytes: Vec<u8>, _ttl: Option<Duration>) {
            self.bytes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key.to_owned(), bytes);
        }

        fn invalidate(&self, key: &str) {
            self.bytes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(key);
        }

        fn clear(&self) {
            self.bytes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
        }
    }

    #[test]
    fn the_cache_backed_store_round_trips_through_a_serializing_backend() {
        let cache = Arc::new(SerializingCache::default());
        let store = CacheKvStore(Arc::clone(&cache) as Arc<dyn crate::cache::Cache>);
        let key = namespaced_key("shop", Some("alpha"), "cart");

        assert!(store.get(&key).is_none(), "nothing stored yet");
        let _ = store.set(&key, PluginValue::Text("A-1".to_owned()));
        assert_eq!(
            store.get(&key),
            Some(PluginValue::Text("A-1".to_owned())),
            "a value written through a serializing backend must read back"
        );
        store.delete(&key);
        assert!(store.get(&key).is_none(), "and delete must remove it");
    }
}
