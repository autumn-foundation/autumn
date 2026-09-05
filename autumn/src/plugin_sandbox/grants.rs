//! What a granted capability is *scoped to*, and how much of it a plugin may
//! use (issue #1632).
//!
//! A capability name answers "may it?"; nothing in it answers "to what?". A
//! plugin granted `http-outbound` may call *something*, and until the manifest
//! says which hostnames, the operator reading that word has approved the open
//! internet. So every capability whose authority points at a named thing
//! carries its list of names here, in one `[grants]` table:
//!
//! ```toml
//! capabilities = ["http-request", "kv", "http-outbound", "db", "jobs", "render"]
//!
//! [grants]
//! hosts = ["api.example.com"]      # http-outbound may call exactly these
//! tables = ["orders"]              # db owns exactly these, tenant-scoped
//! job_types = ["reindex"]          # jobs may enqueue exactly these
//! slots = ["order-summary"]        # render may fill exactly these
//!
//! [quotas]
//! kv_reads = 64                    # per request; declared here, approved on install
//! ```
//!
//! # The two-way rule
//!
//! A grant list and its capability must agree, in *both* directions:
//!
//! * a non-empty list without its capability is refused, because the operator
//!   read "no outbound network" in one place and `api.example.com` three lines
//!   below it, and the runtime must not be the one to decide which of those the
//!   operator meant;
//! * a capability with an empty list is refused, because a `db` grant that names
//!   no table is authority the consent screen displays and the runtime can never
//!   honour — an operator who approved it learned nothing true.
//!
//! # Why the names are identifiers, not strings
//!
//! `tables`, `job_types` and `slots` are `[a-z][a-z0-9_]*`. That is not
//! tidiness: the host *derives* a physical name from each one (a SQL table name,
//! a job type, a slot key), and a derivation is only safe when its input cannot
//! contain the syntax of the thing being derived. Refusing `orders; drop table
//! users` at parse time is what lets the DB capability build statements by
//! concatenation and still be provably unable to name a host-application table.
//! See [`crate::plugin_sandbox::capability::db`].
//!
//! Hostnames get the same treatment for the same reason, plus one more: an
//! outbound allow-list compared with anything looser than equality is not an
//! allow-list. `api.example.com.attacker.test` ends with the granted name, and
//! `https://api.example.com@attacker.test/` starts with it.

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

use std::fmt;

use serde::{Deserialize, Serialize};

use super::manifest::{ManifestError, SandboxCapability};

/// The most entries any one grant list may carry.
///
/// Generous against anything an author writes by hand — a plugin calls a
/// handful of upstreams, not hundreds — and it is the count that has to be
/// bounded: validation scans the entries already seen to reject duplicates, so
/// the work grows with the square of the list, and an accepted list is held for
/// the plugin's lifetime and consulted on every call.
pub const MAX_GRANT_ENTRIES: usize = 64;

/// Longest accepted grant identifier (table, job type, render slot), in bytes.
///
/// 63 is `PostgreSQL`'s identifier ceiling, and the derived physical table name
/// is longer than the granted one — so this is the input bound that keeps the
/// *derived* name inside the bound that actually matters. See
/// [`CapabilityGrants::validate`].
pub const MAX_GRANT_IDENT_LEN: usize = 63;

/// Longest accepted hostname, in bytes: the DNS ceiling.
pub const MAX_HOST_LEN: usize = 253;

/// Upper bound on any one quota.
///
/// A quota is a ceiling on host work bought once in a manifest and paid on
/// every request, so an unbounded one is not a limit. Sized so that the largest
/// legal manifest still describes a plugin whose per-request host work an
/// operator can reason about.
pub const MAX_QUOTA: u32 = 1_000_000;

// ── Grants ───────────────────────────────────────────────────────────────

/// The named things each granted capability is scoped to.
///
/// Not `#[non_exhaustive]`: an embedder building a manifest in memory fills this
/// in, usually as `CapabilityGrants { hosts: …, ..Default::default() }`, and
/// that spelling only works from outside the crate while the type stays open.
///
/// Every list is empty by default, which is what makes the vocabulary
/// fail-closed: a manifest that forgets a list does not get a permissive one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CapabilityGrants {
    /// Hostnames `http-outbound` may call, compared by exact equality.
    pub hosts: Vec<String>,
    /// Logical table names `db` owns. The physical tables are derived; these
    /// are the only names the guest can spell.
    pub tables: Vec<String>,
    /// Job types `jobs` may enqueue.
    pub job_types: Vec<String>,
    /// Render slots `render` may fill.
    pub slots: Vec<String>,
}

/// One grant list, paired with the capability that gives it meaning.
///
/// Written as a table rather than as four near-identical validation blocks so
/// that adding the next capability is one row, and so the two-way rule below is
/// stated once instead of four times.
type GrantRow<'a> = (&'static str, SandboxCapability, &'a Vec<String>, EntryKind);

/// What a grant entry has to look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    /// A DNS hostname, lower-case, bare — no scheme, port, path or userinfo.
    Host,
    /// A `[a-z][a-z0-9_]*` identifier a **SQL** name is derived from, so the
    /// charset is the intersection of what an unquoted identifier allows and
    /// what a reader can tell apart.
    Ident,
    /// A `[a-z][a-z0-9_-]*` name used as a key rather than as an identifier.
    ///
    /// Job types and render slots are looked up, never concatenated into a
    /// statement, so the hyphen an author would naturally write
    /// (`order-summary`, `send-receipt`) costs nothing. Everything a name is
    /// still refused for — spaces, quotes, control characters, upper case — is
    /// refused for the same reasons: it appears in a log line, in the audit
    /// surface and on the consent screen.
    Name,
}

impl CapabilityGrants {
    /// Whether every list is empty — the shape a first-slice manifest has.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.hosts.is_empty()
            && self.tables.is_empty()
            && self.job_types.is_empty()
            && self.slots.is_empty()
    }

    /// The rows, in the order the consent screen prints them.
    const fn rows(&self) -> [GrantRow<'_>; 4] {
        [
            (
                "hosts",
                SandboxCapability::HttpOutbound,
                &self.hosts,
                EntryKind::Host,
            ),
            (
                "tables",
                SandboxCapability::Db,
                &self.tables,
                EntryKind::Ident,
            ),
            (
                "job_types",
                SandboxCapability::Jobs,
                &self.job_types,
                EntryKind::Name,
            ),
            (
                "slots",
                SandboxCapability::Render,
                &self.slots,
                EntryKind::Name,
            ),
        ]
    }

    /// The list `capability` is scoped by, if it has one.
    #[must_use]
    pub fn list_for(&self, capability: SandboxCapability) -> Option<&[String]> {
        self.rows()
            .into_iter()
            .find(|(_, cap, ..)| *cap == capability)
            .map(|(_, _, list, _)| list.as_slice())
    }

    /// Whether `capability`'s list names `entry`, by exact match.
    ///
    /// Exact, deliberately. Every looser comparison an allow-list has ever been
    /// written with — prefix, suffix, `contains`, glob — accepts a name the
    /// operator did not grant: `api.example.com.attacker.test` ends with the
    /// granted host, `orders_secret` starts with the granted table.
    #[must_use]
    pub fn allows(&self, capability: SandboxCapability, entry: &str) -> bool {
        self.list_for(capability)
            .is_some_and(|list| list.iter().any(|granted| granted == entry))
    }

    /// Check every list against the capabilities actually granted.
    ///
    /// # Errors
    ///
    /// See the two-way rule in the module header, plus
    /// [`ManifestError::InvalidGrantEntry`] for an entry whose shape would make
    /// a derived name ambiguous and [`ManifestError::DuplicateGrantEntry`] for a
    /// repeat, which conveys nothing the first entry did not.
    pub fn validate(&self, granted: &[SandboxCapability]) -> Result<(), ManifestError> {
        for (field, capability, list, kind) in self.rows() {
            let held = granted.contains(&capability);
            if !list.is_empty() && !held {
                return Err(ManifestError::GrantWithoutCapability { capability, field });
            }
            if list.is_empty() && held {
                return Err(ManifestError::CapabilityWithoutGrant { capability, field });
            }
            if list.len() > MAX_GRANT_ENTRIES {
                return Err(ManifestError::TooManyGrantEntries {
                    field,
                    found: list.len(),
                    max: MAX_GRANT_ENTRIES,
                });
            }
            // Scanned in place against the entries already seen rather than
            // into a set sized from `list.len()`: the length is the manifest
            // author's to choose, and the ceiling above is checked first so
            // this scan is bounded by `MAX_GRANT_ENTRIES²` regardless.
            for (at, entry) in list.iter().enumerate() {
                let legal = match kind {
                    EntryKind::Host => is_grantable_host(entry),
                    EntryKind::Ident => is_grantable_ident(entry),
                    EntryKind::Name => is_grantable_name(entry),
                };
                if !legal {
                    return Err(ManifestError::InvalidGrantEntry {
                        field,
                        entry: super::manifest::rejected(entry),
                        reason: match kind {
                            EntryKind::Host => {
                                "expected a bare lower-case DNS hostname: no scheme, port, path, \
                                 userinfo or wildcard, and at most 253 bytes"
                            }
                            EntryKind::Ident => {
                                "expected a lower-case identifier matching `[a-z][a-z0-9_]*` of \
                                 at most 63 bytes, because the host derives a physical name from \
                                 it"
                            }
                            EntryKind::Name => {
                                "expected a lower-case name matching `[a-z][a-z0-9_-]*` of at \
                                 most 63 bytes"
                            }
                        },
                    });
                }
                if list.get(..at).is_some_and(|seen| seen.contains(entry)) {
                    return Err(ManifestError::DuplicateGrantEntry {
                        field,
                        entry: super::manifest::rejected(entry),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Whether `host` is a bare, lower-case DNS hostname.
///
/// Rejects everything that would make an *exact* comparison mean less than it
/// looks: an embedded scheme, a port, a path, userinfo, a wildcard, an
/// upper-case letter (so the comparison never needs a case fold, and so two
/// spellings of one grant cannot read as two grants on the consent screen), a
/// trailing dot, and an empty or over-long label.
#[must_use]
pub fn is_grantable_host(host: &str) -> bool {
    if host.is_empty() || host.len() > MAX_HOST_LEN {
        return false;
    }
    // A single label is legal DNS but is never a public upstream, and accepting
    // one invites `localhost` into an allow-list whose whole purpose is to name
    // somewhere else.
    if !host.contains('.') {
        return false;
    }
    // An IPv4 literal is refused for the same reason `host_of` refuses an IPv6
    // one: a literal address is not a name, so a *name* allow-list can neither
    // grant nor deny one honestly. Enforcing it for one family and not the
    // other was the gap — `169.254.169.254` is all digits and dots, so it
    // passed the label rules below and put the cloud metadata endpoint on a
    // consent screen as though it were a hostname.
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return false;
    }
    // …and every other spelling that a URL parser reads as an address. Rust's
    // `Ipv4Addr` accepts only dotted-quad, but WHATWG and POSIX resolvers also
    // accept short and non-decimal forms — `127.1`, `127.0.1`, `0177.0.0.1`,
    // `0x7f.1` — each of which reached loopback while passing the label rules
    // below as though it were a name. The rule those parsers actually use is
    // that a host is an address when its *last* label is a number, so that is
    // the rule here: it catches every short form, and it still admits
    // `1.example.com`, whose last label is not.
    if host.rsplit('.').next().is_some_and(looks_like_number) {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    })
}

/// Whether `label` is something a URL parser would read as a number.
///
/// Decimal or hexadecimal; octal needs no separate arm because it is written
/// with digits and reads as decimal here, which refuses it either way. The
/// charset check in [`is_grantable_host`] has already ruled out upper case, so
/// only the lower-case `0x` prefix has to be recognised.
fn looks_like_number(label: &str) -> bool {
    if label.is_empty() {
        return false;
    }
    if let Some(hex) = label.strip_prefix("0x") {
        return !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    label.bytes().all(|byte| byte.is_ascii_digit())
}

/// Whether `ident` is a name a physical SQL identifier may be derived from.
#[must_use]
pub fn is_grantable_ident(ident: &str) -> bool {
    !ident.is_empty()
        && ident.len() <= MAX_GRANT_IDENT_LEN
        && ident.starts_with(|ch: char| ch.is_ascii_lowercase())
        && ident
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Whether `name` is a grantable key: an identifier, or one with hyphens.
///
/// Used where the entry is looked up rather than concatenated into a statement.
#[must_use]
pub fn is_grantable_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_GRANT_IDENT_LEN
        && name.starts_with(|ch: char| ch.is_ascii_lowercase())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

// ── Quotas ───────────────────────────────────────────────────────────────

/// Per-request ceilings on each capability.
///
/// **Declared by the plugin's author and approved by the operator**, not set by
/// the operator: they live in the manifest, which the artifact digest covers, so
/// there is no mount-time override. That is why every one of them is on the
/// consent screen and why [`ConsentDelta`] treats a raised quota as new
/// authority — an upgrade that doubles `db_writes` is asking for something, and
/// approving it is the operator's only lever.
///
/// Fuel bounds the *guest's* work. It does not bound the host's: a KV write
/// costs a guest one call frame and costs the host a cache round-trip, and a
/// job enqueue costs it a durable write. Every capability therefore carries its
/// own count, defaulted conservatively — generous for a plugin that renders a
/// panel, small enough that a hostile one is stopped inside one request.
///
/// Exceeding a quota denies that call and records it. It does not fail the
/// request: a plugin that hits a ceiling should degrade, and a denial the guest
/// can see is a denial its author can fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CapabilityQuotas {
    /// KV reads per request.
    pub kv_reads: u32,
    /// KV writes (set and delete) per request.
    pub kv_writes: u32,
    /// Bytes one KV value may hold.
    pub kv_value_bytes: u32,
    /// Outbound HTTP calls per request.
    pub outbound_calls: u32,
    /// Bytes one outbound response may return to the guest.
    pub outbound_response_bytes: u32,
    /// Bytes one outbound *request* body may carry.
    ///
    /// The counterpart to `outbound_response_bytes`, and it was missing: a
    /// plugin's own body is guest-chosen, and without this its only bound was
    /// the stdout frame the call arrives in — megabytes by default, and far
    /// more once an operator raises `max_response_bytes`. An operator granting
    /// `http-outbound` is agreeing to calls to named hosts, not to whatever
    /// volume of egress the plugin decides to push through them.
    pub outbound_request_bytes: u32,
    /// Row-returning DB operations per request.
    pub db_reads: u32,
    /// Row-writing DB operations per request.
    pub db_writes: u32,
    /// Rows one DB query may return.
    pub db_rows: u32,
    /// Jobs enqueued per request.
    pub job_enqueues: u32,
    /// Bytes one rendered fragment may hold.
    pub render_bytes: u32,
    /// Capability calls of every kind, per request, across all capabilities.
    ///
    /// The per-capability counts bound each surface; this bounds their sum, so
    /// a plugin cannot spend every ceiling at once and call that staying within
    /// its quota.
    pub calls: u32,
    /// How long one outbound call may take, in milliseconds.
    ///
    /// Fuel bounds the guest's instructions and cannot bound a socket that
    /// never answers. An outbound call runs on a blocking worker holding the
    /// plugin's concurrency permit, so without a deadline a granted host that
    /// black-holes the connection shuts the plugin's prefix and eats the shared
    /// blocking pool.
    pub outbound_timeout_ms: u32,
    /// Calls per second per capability, across every request this plugin
    /// serves.
    ///
    /// The counts above bound one request; this bounds the aggregate. A panel
    /// fetched a thousand times a second spends its per-request budget a
    /// thousand times, and every one of those calls is legitimate as far as a
    /// per-request ledger can see.
    pub calls_per_second: u32,
}

impl Default for CapabilityQuotas {
    fn default() -> Self {
        Self {
            kv_reads: 64,
            kv_writes: 32,
            kv_value_bytes: 64 * 1024,
            outbound_calls: 4,
            outbound_response_bytes: 256 * 1024,
            outbound_request_bytes: 256 * 1024,
            db_reads: 64,
            db_writes: 32,
            db_rows: 500,
            job_enqueues: 8,
            render_bytes: 64 * 1024,
            calls: 128,
            outbound_timeout_ms: 5_000,
            calls_per_second: 200,
        }
    }
}

impl CapabilityQuotas {
    /// Every quota, as `(field, value)`, in declaration order.
    ///
    /// One list, so validation, the consent screen and the upgrade diff cannot
    /// disagree about which quotas exist — the failure mode of writing them out
    /// three times is a quota that is enforced but never displayed.
    #[must_use]
    pub const fn fields(&self) -> [(&'static str, u32); 14] {
        [
            ("kv_reads", self.kv_reads),
            ("kv_writes", self.kv_writes),
            ("kv_value_bytes", self.kv_value_bytes),
            ("outbound_calls", self.outbound_calls),
            ("outbound_response_bytes", self.outbound_response_bytes),
            ("outbound_request_bytes", self.outbound_request_bytes),
            ("db_reads", self.db_reads),
            ("db_writes", self.db_writes),
            ("db_rows", self.db_rows),
            ("job_enqueues", self.job_enqueues),
            ("render_bytes", self.render_bytes),
            ("calls", self.calls),
            ("outbound_timeout_ms", self.outbound_timeout_ms),
            ("calls_per_second", self.calls_per_second),
        ]
    }

    /// # Errors
    ///
    /// Returns [`ManifestError::QuotaOutOfRange`] for a zero or oversized
    /// quota. Zero is refused for the same reason a zero resource limit is: it
    /// is not "no limit" but "cannot run", and a manifest that says it by
    /// accident should say so at load rather than at the first call.
    pub fn validate(&self) -> Result<(), ManifestError> {
        for (field, value) in self.fields() {
            if value == 0 || value > MAX_QUOTA {
                return Err(ManifestError::QuotaOutOfRange {
                    field,
                    value,
                    max: MAX_QUOTA,
                });
            }
        }
        Ok(())
    }
}

// ── Consent deltas ───────────────────────────────────────────────────────

/// Everything a new manifest asks for that the approved one did not.
///
/// An upgrade is the moment a plugin's authority can grow without anybody
/// looking, so the install flow asks this type — not a version comparison —
/// whether the operator has to be prompted again.
///
/// Only *growth* counts. A plugin that drops a capability, stops calling a
/// host, or lowers a quota is asking for less than was already approved, and
/// re-prompting for that trains operators to click through the prompt that
/// matters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ConsentDelta {
    /// Capabilities the new manifest asks for and the old one did not.
    pub added_capabilities: Vec<SandboxCapability>,
    /// Outbound hosts newly named.
    pub added_hosts: Vec<String>,
    /// Tables newly named.
    pub added_tables: Vec<String>,
    /// Job types newly named.
    pub added_job_types: Vec<String>,
    /// Render slots newly named.
    pub added_slots: Vec<String>,
    /// Quotas raised, as `(field, approved, requested)`.
    pub raised_quotas: Vec<(&'static str, u32, u32)>,
    /// Routes newly mounted, as `"METHOD /path"`.
    ///
    /// A route is *enforced* authority, not documentation: the host builds its
    /// router from exactly this list, and the consent screen promises the plugin
    /// serves "these and only these". An upgrade that adds one exposes an
    /// endpoint nobody approved, so it belongs here beside a new capability.
    pub added_routes: Vec<String>,
    /// Resource ceilings raised, as `(field, approved, requested)`.
    ///
    /// Also authority, and the kind an upgrade can grow enormously without
    /// touching a capability name: `fuel` and `memory_bytes` and
    /// `max_concurrency` are what one plugin may cost the host, and their
    /// product is what `request_footprint_bytes` bounds.
    pub raised_limits: Vec<(&'static str, u128, u128)>,
}

impl ConsentDelta {
    /// Whether the operator must be asked again before this manifest runs.
    #[must_use]
    pub const fn requires_consent(&self) -> bool {
        !self.added_capabilities.is_empty()
            || !self.added_hosts.is_empty()
            || !self.added_tables.is_empty()
            || !self.added_job_types.is_empty()
            || !self.added_slots.is_empty()
            || !self.raised_quotas.is_empty()
            || !self.added_routes.is_empty()
            || !self.raised_limits.is_empty()
    }

    /// The lines to print above a re-consent prompt.
    ///
    /// Empty when nothing grew, so a caller can print it unconditionally.
    #[must_use]
    pub fn summary(&self) -> String {
        use std::fmt::Write as _;

        if !self.requires_consent() {
            return String::new();
        }
        let mut out =
            String::from("This upgrade asks for authority the installed version did not:\n");
        // `write!` to a `String` is infallible; results are dropped rather than
        // unwrapped so this stays panic-free by construction.
        for capability in &self.added_capabilities {
            let _ = writeln!(
                out,
                "  + capability {name} — {describe}",
                name = capability.as_str(),
                describe = capability.describe()
            );
        }
        for (label, entries) in [
            ("outbound host", &self.added_hosts),
            ("database table", &self.added_tables),
            ("job type", &self.added_job_types),
            ("render slot", &self.added_slots),
        ] {
            for entry in entries {
                let _ = writeln!(out, "  + {label} {entry}");
            }
        }
        for route in &self.added_routes {
            let _ = writeln!(out, "  + route {route}");
        }
        for (field, approved, requested) in &self.raised_quotas {
            let _ = writeln!(out, "  + quota {field} {approved} -> {requested}");
        }
        for (field, approved, requested) in &self.raised_limits {
            let _ = writeln!(out, "  + limit {field} {approved} -> {requested}");
        }
        out
    }
}

/// Entries in `next` that `previous` does not carry, order preserved.
#[must_use]
pub(crate) fn added<T: PartialEq + Clone>(previous: &[T], next: &[T]) -> Vec<T> {
    next.iter()
        .filter(|entry| !previous.contains(entry))
        .cloned()
        .collect()
}

impl fmt::Display for ConsentDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grantable_host_is_a_bare_lower_case_dns_name() {
        for host in [
            "api.example.com",
            "a.b",
            "x1-y2.example.co.uk",
            &format!("{}.example.com", "a".repeat(63)),
        ] {
            assert!(is_grantable_host(host), "{host}");
        }
        for host in [
            "",
            "localhost",
            // A literal address is not a name, so a *name* allow-list can
            // neither grant nor deny one honestly. Enforcing that for IPv6 and
            // not IPv4 was the gap: `169.254.169.254` is all digits and dots,
            // so it passed every label rule and put the cloud metadata endpoint
            // on a consent screen as though it were a hostname.
            "169.254.169.254",
            "127.0.0.1",
            "10.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "API.example.com",
            "api.example.com.",
            ".example.com",
            "api..example.com",
            "-api.example.com",
            "api-.example.com",
            "api.example.com:443",
            "https://api.example.com",
            "api.example.com/v1",
            "user@api.example.com",
            "*.example.com",
            "api.exam ple.com",
            "api.exämple.com",
            &format!("{}.example.com", "a".repeat(64)),
            &format!("{}.com", "a.".repeat(200)),
        ] {
            assert!(!is_grantable_host(host), "{host} was accepted");
        }
    }

    #[test]
    fn a_grantable_identifier_can_never_close_an_sql_identifier() {
        for ident in ["orders", "o", "line_items", "t1"] {
            assert!(is_grantable_ident(ident), "{ident}");
        }
        for ident in [
            "",
            "Orders",
            "1orders",
            "_orders",
            "orders-2",
            "orders ",
            "orders;",
            "\"orders\"",
            "orders'",
            "orders`",
            "public.users",
            "orders--",
            &"o".repeat(MAX_GRANT_IDENT_LEN + 1),
        ] {
            assert!(!is_grantable_ident(ident), "{ident} was accepted");
        }
    }

    #[test]
    fn a_grantable_name_allows_the_hyphen_an_author_would_write() {
        for name in ["order-summary", "send-receipt", "reindex"] {
            assert!(is_grantable_name(name), "{name}");
            // Hyphens are the only thing a name allows that an identifier does
            // not, so the two agree everywhere else.
            assert_eq!(is_grantable_ident(name), !name.contains('-'), "{name}");
        }
        for name in ["-lead", "Order-Summary", "order summary", ""] {
            assert!(!is_grantable_name(name), "{name} was accepted");
        }
    }

    #[test]
    fn a_grant_is_matched_by_equality_and_never_by_anything_looser() {
        let grants = CapabilityGrants {
            hosts: vec!["api.example.com".to_owned()],
            tables: vec!["orders".to_owned()],
            ..CapabilityGrants::default()
        };
        assert!(grants.allows(SandboxCapability::HttpOutbound, "api.example.com"));
        for near_miss in [
            "api.example.com.attacker.test",
            "evil-api.example.com",
            "api.example.co",
            "API.example.com",
            "api.example.com ",
            "",
        ] {
            assert!(
                !grants.allows(SandboxCapability::HttpOutbound, near_miss),
                "{near_miss} matched"
            );
        }
        assert!(!grants.allows(SandboxCapability::Db, "orders_secret"));
        // A capability with no list of its own matches nothing, rather than
        // everything.
        assert!(!grants.allows(SandboxCapability::Kv, "anything"));
        assert!(grants.list_for(SandboxCapability::Kv).is_none());
    }

    #[test]
    fn a_delta_summary_is_empty_when_nothing_grew() {
        let delta = ConsentDelta::default();
        assert!(!delta.requires_consent());
        assert!(delta.summary().is_empty());
        assert!(delta.to_string().is_empty());
    }

    #[test]
    fn every_quota_field_is_checked_by_validate() {
        // The one list, read by validation, the consent screen and the upgrade
        // diff alike — so a quota that is enforced but never displayed cannot
        // exist.
        let defaults = CapabilityQuotas::default();
        assert!(defaults.validate().is_ok());
        for (index, (field, _)) in defaults.fields().into_iter().enumerate() {
            let mut broken = defaults;
            // Zero out exactly one field by rebuilding through the same list.
            match index {
                0 => broken.kv_reads = 0,
                1 => broken.kv_writes = 0,
                2 => broken.kv_value_bytes = 0,
                3 => broken.outbound_calls = 0,
                4 => broken.outbound_response_bytes = 0,
                5 => broken.outbound_request_bytes = 0,
                6 => broken.db_reads = 0,
                7 => broken.db_writes = 0,
                8 => broken.db_rows = 0,
                9 => broken.job_enqueues = 0,
                10 => broken.render_bytes = 0,
                11 => broken.calls = 0,
                12 => broken.outbound_timeout_ms = 0,
                13 => broken.calls_per_second = 0,
                other => panic!("quota field {other} ({field}) has no case here"),
            }
            assert_eq!(
                broken.validate(),
                Err(ManifestError::QuotaOutOfRange {
                    field,
                    value: 0,
                    max: MAX_QUOTA
                }),
                "{field}"
            );
        }
    }

    #[test]
    fn a_grant_list_longer_than_the_ceiling_is_refused_before_it_is_scanned() {
        let grants = CapabilityGrants {
            hosts: (0..=MAX_GRANT_ENTRIES)
                .map(|index| format!("h{index}.example.com"))
                .collect(),
            ..CapabilityGrants::default()
        };
        assert_eq!(
            grants.validate(&[SandboxCapability::HttpOutbound]),
            Err(ManifestError::TooManyGrantEntries {
                field: "hosts",
                found: MAX_GRANT_ENTRIES + 1,
                max: MAX_GRANT_ENTRIES,
            })
        );
    }

    #[test]
    fn no_spelling_of_an_address_is_grantable_as_a_host() {
        // `Ipv4Addr::parse` accepts only dotted-quad, but a URL parser accepts
        // short and non-decimal forms and resolves every one of these to
        // loopback or to the cloud metadata endpoint. Each passed the label
        // rules as though it were a name.
        for spelling in [
            "127.0.0.1",
            "127.1",
            "127.0.1",
            "0177.0.0.1",
            "0x7f.1",
            "169.254.169.254",
            "10.0.0.1",
            "1.2.3.4",
        ] {
            assert!(
                !is_grantable_host(spelling),
                "{spelling} is an address, not a name"
            );
        }
        // And a name whose last label is not a number is still a name.
        for name in [
            "api.example.com",
            "1.example.com",
            "10.internal.example",
            "a-b.example.co.uk",
        ] {
            assert!(is_grantable_host(name), "{name} is a name");
        }
    }
}
