//! On-disk layout of a SQLite database fleet: one database file per tenant,
//! or one per routing slot (ADR 0019).
//!
//! This module is the pure, backend-independent half of the fleet. It owns:
//!
//! - [`FleetMode`]: what a database file is keyed by;
//! - [`TenantDbId`]: a tenant id that is safe to use as a file name;
//! - [`FleetDbKey`]: the identity of one database in the fleet;
//! - [`FleetPathTemplate`]: how a key maps to a path under the fleet root,
//!   and back again when the fleet enumerates its files.
//!
//! The runtime half (pools, lazy open, migrations, eviction, replication) is
//! `crate::db::fleet`, compiled only under the `sqlite` feature. Keeping the
//! layout here lets config validation and its tests run in every build.
//!
//! # Why the tenant id rules are strict
//!
//! A tenant id reaches this module from a request header, a subdomain, a
//! session or a JWT claim. The tenancy layer only rejects empty ids. Used as a
//! file name, `../other`, `a/b`, `CON` or `Acme` vs `acme` on a
//! case-insensitive file system would each reach a database the tenant does
//! not own. [`TenantDbId::parse`] therefore admits only lowercase ASCII
//! letters, digits, `-` and `_`, starting with a letter or digit, at most
//! [`TENANT_DB_ID_MAX_LEN`] bytes, and never a Windows device name.

use std::fmt::{self, Write as _};
use std::path::{Path, PathBuf};

/// Number of logical routing slots. Mirrors [`crate::config::SLOT_COUNT`].
const SLOTS: u16 = 16384;

/// Slots per `{bucket}` directory. `16384 / 64 = 256` buckets.
pub const SLOTS_PER_BUCKET: u16 = 64;

/// Longest tenant id that can name a database file.
pub const TENANT_DB_ID_MAX_LEN: usize = 128;

/// What one database file in the fleet holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetMode {
    /// One database per tenant. The hardest isolation: a tenant's data is
    /// one file, so export, deletion and restore are file operations.
    /// File count grows with the tenant count.
    Tenant,
    /// One database per routing slot (16384 at most). Tenants that hash to
    /// the same slot share a file, so the file count is bounded and a slot is
    /// the unit that moves between hosts.
    Slot,
}

impl FleetMode {
    /// The path template used when `database.fleet.path` is not set.
    #[must_use]
    pub const fn default_path(self) -> &'static str {
        match self {
            Self::Tenant => "{bucket}/{tenant}.db",
            Self::Slot => "{bucket}/slot-{slot}.db",
        }
    }

    /// Whether a database that does not exist yet is created on first use.
    ///
    /// Slots are a closed set of 16384, so creating one on demand cannot be
    /// abused. Tenant ids come from requests: creating a file for any id a
    /// client sends would let one client fill the disk, so tenant databases
    /// must be provisioned explicitly unless the app opts in.
    #[must_use]
    pub const fn default_create_on_demand(self) -> bool {
        matches!(self, Self::Slot)
    }
}

impl std::str::FromStr for FleetMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "tenant" => Ok(Self::Tenant),
            "slot" => Ok(Self::Slot),
            other => Err(format!("expected `tenant` or `slot`, got {other:?}")),
        }
    }
}

impl fmt::Display for FleetMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tenant => "tenant",
            Self::Slot => "slot",
        })
    }
}

/// Why a key or a template was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FleetLayoutError {
    /// The tenant id cannot name a database file.
    #[error("tenant id {id:?} cannot name a fleet database: {reason}")]
    InvalidTenantId {
        /// The refused id, truncated to 64 bytes for the message.
        id: String,
        /// Which rule it broke.
        reason: &'static str,
    },
    /// The slot is outside `0..16384`.
    #[error("slot {0} is out of range (0..16384)")]
    SlotOutOfRange(u32),
    /// The path template is malformed or does not fit the mode.
    #[error("database.fleet.path {template:?}: {reason}")]
    InvalidTemplate {
        /// The configured template.
        template: String,
        /// What is wrong with it.
        reason: String,
    },
    /// A database name (`tenant:<id>` / `slot:<n>`) could not be parsed.
    #[error("{0:?} is not a fleet database name (expected `tenant:<id>` or `slot:<0-16383>`)")]
    InvalidName(String),
}

/// A tenant id that is safe to use as a file name. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantDbId(String);

/// Device names Windows reserves in every directory, with or without an
/// extension.
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

impl TenantDbId {
    /// Validate a tenant id for use as a database file name.
    ///
    /// # Errors
    ///
    /// [`FleetLayoutError::InvalidTenantId`] when the id is empty, too long,
    /// has a character outside `[a-z0-9_-]`, starts with `-` or `_`, or is a
    /// Windows device name.
    pub fn parse(id: &str) -> Result<Self, FleetLayoutError> {
        let refuse = |reason| {
            let mut shown = id.to_owned();
            if shown.len() > 64 {
                let mut cut = 64;
                while !shown.is_char_boundary(cut) {
                    cut -= 1;
                }
                shown.truncate(cut);
            }
            Err(FleetLayoutError::InvalidTenantId { id: shown, reason })
        };
        if id.is_empty() {
            return refuse("it is empty");
        }
        if id.len() > TENANT_DB_ID_MAX_LEN {
            return refuse("it is longer than 128 bytes");
        }
        if !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return refuse(
                "only lowercase ASCII letters, digits, `-` and `_` are allowed \
                 (uppercase would let `Acme` and `acme` share a file on a \
                 case-insensitive file system)",
            );
        }
        if id.starts_with(['-', '_']) {
            return refuse("it must start with a letter or a digit");
        }
        if WINDOWS_DEVICE_NAMES.contains(&id) {
            return refuse("it is a reserved device name on Windows");
        }
        Ok(Self(id.to_owned()))
    }

    /// The validated id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantDbId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identity of one database in the fleet.
///
/// A tenant key carries the tenant's routing slot as well, so the layout can
/// place tenant files in `{bucket}` / `{slot}` directories without depending
/// on the hash function (which lives in [`crate::sharding`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FleetDbKey {
    /// A per-tenant database.
    Tenant {
        /// The tenant.
        id: TenantDbId,
        /// The tenant's routing slot.
        slot: u16,
    },
    /// A per-slot database.
    Slot(u16),
}

impl FleetDbKey {
    /// A per-slot key.
    ///
    /// # Errors
    ///
    /// [`FleetLayoutError::SlotOutOfRange`] for a slot `>= 16384`.
    pub fn slot(slot: u16) -> Result<Self, FleetLayoutError> {
        if slot >= SLOTS {
            return Err(FleetLayoutError::SlotOutOfRange(u32::from(slot)));
        }
        Ok(Self::Slot(slot))
    }

    /// The routing slot of this database.
    #[must_use]
    pub const fn routing_slot(&self) -> u16 {
        match self {
            Self::Tenant { slot, .. } | Self::Slot(slot) => *slot,
        }
    }

    /// The `{bucket}` this database lives in: `slot / 64`, so a bucket is a
    /// contiguous slot range and moves between hosts with its slots.
    #[must_use]
    pub const fn bucket(&self) -> u16 {
        self.routing_slot() / SLOTS_PER_BUCKET
    }

    /// Stable display name: `tenant:<id>` or `slot:<nnnnn>`.
    ///
    /// Used as the shard name of the database's [`Shard`](crate::sharding::Shard)
    /// and as the key prefix of its replica. It is unbounded in a tenant fleet,
    /// so it goes on spans and logs, never on metric labels.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Self::Tenant { id, .. } => format!("tenant:{id}"),
            Self::Slot(slot) => format!("slot:{slot:05}"),
        }
    }

    /// The tenant id, for a per-tenant key.
    #[must_use]
    pub const fn tenant(&self) -> Option<&TenantDbId> {
        match self {
            Self::Tenant { id, .. } => Some(id),
            Self::Slot(_) => None,
        }
    }

    /// The mode this key belongs to.
    #[must_use]
    pub const fn mode(&self) -> FleetMode {
        match self {
            Self::Tenant { .. } => FleetMode::Tenant,
            Self::Slot(_) => FleetMode::Slot,
        }
    }
}

impl fmt::Display for FleetDbKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// A parsed `tenant:<id>` / `slot:<n>` name, before the tenant's slot is
/// known. See [`parse_db_name`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedDbName {
    /// `tenant:<id>`.
    Tenant(TenantDbId),
    /// `slot:<n>`.
    Slot(u16),
}

/// Parse a name produced by [`FleetDbKey::name`].
///
/// # Errors
///
/// [`FleetLayoutError::InvalidName`] for anything else, or the tenant-id /
/// slot error of the payload.
pub fn parse_db_name(name: &str) -> Result<ParsedDbName, FleetLayoutError> {
    if let Some(id) = name.strip_prefix("tenant:") {
        return TenantDbId::parse(id).map(ParsedDbName::Tenant);
    }
    if let Some(slot) = name.strip_prefix("slot:") {
        let slot: u32 = slot
            .parse()
            .map_err(|_| FleetLayoutError::InvalidName(name.to_owned()))?;
        if slot >= u32::from(SLOTS) {
            return Err(FleetLayoutError::SlotOutOfRange(slot));
        }
        #[allow(clippy::cast_possible_truncation)]
        return Ok(ParsedDbName::Slot(slot as u16));
    }
    Err(FleetLayoutError::InvalidName(name.to_owned()))
}

// ── Path templates ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Lit(String),
    Tenant,
    Slot,
    Bucket,
}

/// Values captured from a path while enumerating the fleet directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Captures {
    tenant: Option<String>,
    slot: Option<u16>,
    bucket: Option<u16>,
}

/// Maps a [`FleetDbKey`] to a path relative to the fleet root, and back.
///
/// A template is `/`-separated segments. Each segment mixes literal text with
/// the placeholders:
///
/// | Placeholder | Renders | Example |
/// | --- | --- | --- |
/// | `{tenant}` | the tenant id | `acme` |
/// | `{slot}` | the routing slot, 5 digits | `00042` |
/// | `{bucket}` | `slot / 64`, 3 digits (256 buckets) | `000` |
///
/// Rules, all checked by [`FleetPathTemplate::parse`]:
///
/// - relative, no `.`/`..`/empty segments, no `\`;
/// - only the three placeholders, never two placeholders side by side;
/// - a tenant fleet must use `{tenant}`; a slot fleet must use `{slot}` and
///   may not use `{tenant}`;
/// - the file name ends with literal text that ends in an extension
///   (`.db`, `-data.sqlite3`), so the `-wal`/`-shm`/`-journal` sidecars
///   never enumerate as databases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetPathTemplate {
    raw: String,
    mode: FleetMode,
    segments: Vec<Vec<Part>>,
}

impl FleetPathTemplate {
    /// Parse and validate a template for `mode`.
    ///
    /// # Errors
    ///
    /// [`FleetLayoutError::InvalidTemplate`] naming the broken rule.
    pub fn parse(raw: &str, mode: FleetMode) -> Result<Self, FleetLayoutError> {
        let refuse = |reason: String| FleetLayoutError::InvalidTemplate {
            template: raw.to_owned(),
            reason,
        };
        if raw.is_empty() {
            return Err(refuse("it is empty".to_owned()));
        }
        if raw.contains('\\') {
            return Err(refuse("use `/` as the separator, not `\\`".to_owned()));
        }
        if raw.starts_with('/') {
            return Err(refuse(
                "it must be relative to database.fleet.root".to_owned(),
            ));
        }
        let mut segments = Vec::new();
        for segment in raw.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(refuse(format!(
                    "segment {segment:?} is not allowed (no empty, `.` or `..` segments)"
                )));
            }
            segments.push(parse_segment(segment).map_err(refuse)?);
        }
        let all: Vec<&Part> = segments.iter().flatten().collect();
        let count = |want: &Part| all.iter().filter(|p| **p == want).count();
        for (part, name) in [
            (Part::Tenant, "{tenant}"),
            (Part::Slot, "{slot}"),
            (Part::Bucket, "{bucket}"),
        ] {
            if count(&part) > 1 {
                return Err(refuse(format!("{name} appears more than once")));
            }
        }
        match mode {
            FleetMode::Tenant if count(&Part::Tenant) == 0 => {
                return Err(refuse(
                    "a tenant fleet must use {tenant}, or tenants would share a file".to_owned(),
                ));
            }
            FleetMode::Slot if count(&Part::Slot) == 0 => {
                return Err(refuse(
                    "a slot fleet must use {slot}, or slots would share a file".to_owned(),
                ));
            }
            FleetMode::Slot if count(&Part::Tenant) > 0 => {
                return Err(refuse("a slot fleet has no {tenant}".to_owned()));
            }
            _ => {}
        }
        let file_ext_ok = segments
            .last()
            .and_then(|parts| parts.last())
            .is_some_and(|part| matches!(part, Part::Lit(lit) if ends_with_extension(lit)));
        if !file_ext_ok {
            return Err(refuse(
                "the file name must end with a literal extension such as `.db`, so SQLite's \
                 -wal/-shm/-journal sidecars (which end in `-…`) are never mistaken for databases"
                    .to_owned(),
            ));
        }
        Ok(Self {
            raw: raw.to_owned(),
            mode,
            segments,
        })
    }

    /// The template as configured.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The fleet mode this template was validated for.
    #[must_use]
    pub const fn mode(&self) -> FleetMode {
        self.mode
    }

    /// The path of `key`, relative to the fleet root.
    #[must_use]
    pub fn render(&self, key: &FleetDbKey) -> PathBuf {
        let mut path = PathBuf::new();
        for segment in &self.segments {
            let mut out = String::new();
            for part in segment {
                match part {
                    Part::Lit(lit) => out.push_str(lit),
                    Part::Tenant => {
                        // `parse` guarantees a slot template has no {tenant}.
                        if let Some(id) = key.tenant() {
                            out.push_str(id.as_str());
                        }
                    }
                    Part::Slot => {
                        let _ = write!(out, "{:05}", key.routing_slot());
                    }
                    Part::Bucket => {
                        let _ = write!(out, "{:03}", key.bucket());
                    }
                }
            }
            path.push(out);
        }
        path
    }

    /// Recover a key from a path relative to the fleet root.
    ///
    /// `slot_of` maps a tenant id to its routing slot (the fleet passes
    /// [`crate::sharding::slot_for_key`]). Returns `None` for any path this
    /// template would not render — including a tenant file sitting in the
    /// wrong `{bucket}`/`{slot}` directory, which a stray copy can produce.
    #[must_use]
    pub fn key_for_path(
        &self,
        relative: &Path,
        slot_of: &dyn Fn(&str) -> u16,
    ) -> Option<FleetDbKey> {
        let names: Vec<&str> = relative
            .components()
            .map(|c| c.as_os_str().to_str())
            .collect::<Option<_>>()?;
        if names.len() != self.segments.len() {
            return None;
        }
        let mut caps = Captures::default();
        for (name, parts) in names.iter().zip(&self.segments) {
            if !match_segment(parts, name, &mut caps) {
                return None;
            }
        }
        self.key_from_captures(&caps, slot_of)
    }

    fn key_from_captures(
        &self,
        caps: &Captures,
        slot_of: &dyn Fn(&str) -> u16,
    ) -> Option<FleetDbKey> {
        let key = match self.mode {
            FleetMode::Tenant => {
                let id = TenantDbId::parse(caps.tenant.as_deref()?).ok()?;
                let slot = slot_of(id.as_str());
                FleetDbKey::Tenant { id, slot }
            }
            FleetMode::Slot => FleetDbKey::slot(caps.slot?).ok()?,
        };
        // A captured {slot}/{bucket} must agree with the key's own: a tenant
        // file in another tenant's bucket is a stray, not a database.
        if caps.slot.is_some_and(|s| s != key.routing_slot())
            || caps.bucket.is_some_and(|b| b != key.bucket())
        {
            return None;
        }
        Some(key)
    }

    /// Match one directory entry name against segment `depth`, for a
    /// directory walk. `true` when the name can lead to a database.
    #[must_use]
    pub fn segment_matches(&self, depth: usize, name: &str) -> bool {
        self.segments
            .get(depth)
            .is_some_and(|parts| match_segment(parts, name, &mut Captures::default()))
    }

    /// The literal directory name of segment `depth`, when it has no
    /// placeholder (so a walk can join it instead of listing the directory).
    #[must_use]
    pub fn literal_segment(&self, depth: usize) -> Option<&str> {
        match self.segments.get(depth)?.as_slice() {
            [Part::Lit(lit)] => Some(lit),
            _ => None,
        }
    }

    /// Number of path segments (directory levels plus the file name).
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.segments.len()
    }
}

impl fmt::Display for FleetPathTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Whether a literal ends in `.<alphanumerics>`, which no sidecar name does.
fn ends_with_extension(lit: &str) -> bool {
    lit.rsplit_once('.')
        .is_some_and(|(_, ext)| !ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric()))
}

fn parse_segment(segment: &str) -> Result<Vec<Part>, String> {
    let mut parts = Vec::new();
    let mut rest = segment;
    while !rest.is_empty() {
        if let Some(after_open) = rest.strip_prefix('{') {
            let close = after_open
                .find('}')
                .ok_or_else(|| format!("unclosed `{{` in segment {segment:?}"))?;
            let name = &after_open[..close];
            let part = match name {
                "tenant" => Part::Tenant,
                "slot" => Part::Slot,
                "bucket" => Part::Bucket,
                other => {
                    return Err(format!(
                        "unknown placeholder {{{other}}} (known: {{tenant}}, {{slot}}, {{bucket}})"
                    ));
                }
            };
            if parts.last().is_some_and(|p| !matches!(p, Part::Lit(_))) {
                return Err(format!(
                    "two placeholders side by side in segment {segment:?} cannot be told apart; \
                     put literal text between them"
                ));
            }
            parts.push(part);
            rest = &after_open[close + 1..];
        } else {
            let end = rest.find('{').unwrap_or(rest.len());
            let lit = &rest[..end];
            if lit.contains('}') {
                return Err(format!("stray `}}` in segment {segment:?}"));
            }
            parts.push(Part::Lit(lit.to_owned()));
            rest = &rest[end..];
        }
    }
    Ok(parts)
}

/// Backtracking match of one name against one segment's parts.
fn match_segment(parts: &[Part], name: &str, caps: &mut Captures) -> bool {
    let Some((first, rest)) = parts.split_first() else {
        return name.is_empty();
    };
    match first {
        Part::Lit(lit) => name
            .strip_prefix(lit.as_str())
            .is_some_and(|tail| match_segment(rest, tail, caps)),
        Part::Slot | Part::Bucket => {
            let width = if *first == Part::Slot { 5 } else { 3 };
            let Some(digits) = name.get(..width) else {
                return false;
            };
            if !digits.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            let Ok(value) = digits.parse::<u16>() else {
                return false;
            };
            let limit = if *first == Part::Slot {
                SLOTS
            } else {
                SLOTS / SLOTS_PER_BUCKET
            };
            if value >= limit {
                return false;
            }
            let mut next = caps.clone();
            if *first == Part::Slot {
                next.slot = Some(value);
            } else {
                next.bucket = Some(value);
            }
            if match_segment(rest, &name[width..], &mut next) {
                *caps = next;
                true
            } else {
                false
            }
        }
        Part::Tenant => {
            // Try every split point, longest first; the tenant charset is
            // checked by `TenantDbId::parse` on the final capture.
            let valid_prefix = name
                .bytes()
                .take_while(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_'
                })
                .count();
            for len in (1..=valid_prefix.min(TENANT_DB_ID_MAX_LEN)).rev() {
                let mut next = caps.clone();
                next.tenant = Some(name[..len].to_owned());
                if match_segment(rest, &name[len..], &mut next) {
                    *caps = next;
                    return true;
                }
            }
            false
        }
    }
}

/// The `SQLite` sidecar files that belong to a database file.
#[must_use]
pub fn sidecar_paths(db: &Path) -> [PathBuf; 3] {
    let with = |suffix: &str| {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    [with("-wal"), with("-shm"), with("-journal")]
}

#[cfg(test)]
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "fleet path templates use {placeholders}"
)]
mod tests {
    use super::*;

    fn tenant(id: &str, slot: u16) -> FleetDbKey {
        FleetDbKey::Tenant {
            id: TenantDbId::parse(id).unwrap(),
            slot,
        }
    }

    #[test]
    fn tenant_ids_that_could_escape_or_collide_are_refused() {
        for bad in [
            "", "../etc", "a/b", "a\\b", ".hidden", "Acme", "acme.db", "-lead", "_lead", "con",
            "lpt1", "nul", "spa ce", "tab\t", "ünï", "a%2f",
        ] {
            assert!(TenantDbId::parse(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(TenantDbId::parse(&"a".repeat(129)).is_err());
    }

    #[test]
    fn ordinary_tenant_ids_are_accepted() {
        for good in [
            "acme",
            "a",
            "0",
            "tenant-42",
            "my_org",
            "3f2504e0-4f89-11d3-9a0c-0305e82c3301",
            "console",
        ] {
            assert_eq!(TenantDbId::parse(good).unwrap().as_str(), good);
        }
        assert!(TenantDbId::parse(&"a".repeat(128)).is_ok());
    }

    #[test]
    fn the_refusal_message_truncates_long_ids_on_a_char_boundary() {
        let long = format!("{}é{}", "a".repeat(63), "b".repeat(100));
        let FleetLayoutError::InvalidTenantId { id, .. } = TenantDbId::parse(&long).unwrap_err()
        else {
            panic!("expected InvalidTenantId");
        };
        assert!(id.len() <= 64);
        assert!(long.starts_with(&id));
    }

    #[test]
    fn names_round_trip() {
        assert_eq!(tenant("acme", 7).name(), "tenant:acme");
        assert_eq!(FleetDbKey::slot(42).unwrap().name(), "slot:00042");
        assert_eq!(
            parse_db_name("tenant:acme").unwrap(),
            ParsedDbName::Tenant(TenantDbId::parse("acme").unwrap())
        );
        assert_eq!(parse_db_name("slot:00042").unwrap(), ParsedDbName::Slot(42));
        assert_eq!(
            parse_db_name("slot:16383").unwrap(),
            ParsedDbName::Slot(16383)
        );
        assert!(parse_db_name("slot:16384").is_err());
        assert!(parse_db_name("slot:x").is_err());
        assert!(parse_db_name("shard0").is_err());
        assert!(parse_db_name("tenant:../x").is_err());
        assert!(FleetDbKey::slot(16384).is_err());
    }

    #[test]
    fn buckets_are_contiguous_slot_ranges() {
        assert_eq!(FleetDbKey::slot(0).unwrap().bucket(), 0);
        assert_eq!(FleetDbKey::slot(63).unwrap().bucket(), 0);
        assert_eq!(FleetDbKey::slot(64).unwrap().bucket(), 1);
        assert_eq!(FleetDbKey::slot(16383).unwrap().bucket(), 255);
    }

    #[test]
    fn default_templates_parse_for_their_mode() {
        for mode in [FleetMode::Tenant, FleetMode::Slot] {
            FleetPathTemplate::parse(mode.default_path(), mode).unwrap();
        }
        assert!(FleetMode::Slot.default_create_on_demand());
        assert!(!FleetMode::Tenant.default_create_on_demand());
    }

    #[test]
    fn templates_render_keys() {
        let t = FleetPathTemplate::parse("{bucket}/{tenant}.db", FleetMode::Tenant).unwrap();
        assert_eq!(t.render(&tenant("acme", 130)), PathBuf::from("002/acme.db"));
        let s = FleetPathTemplate::parse("slots/{bucket}/slot-{slot}.sqlite3", FleetMode::Slot)
            .unwrap();
        assert_eq!(
            s.render(&FleetDbKey::slot(16383).unwrap()),
            PathBuf::from("slots/255/slot-16383.sqlite3")
        );
    }

    #[test]
    fn malformed_templates_are_refused() {
        let cases = [
            ("", FleetMode::Tenant),
            ("/abs/{tenant}.db", FleetMode::Tenant),
            ("../{tenant}.db", FleetMode::Tenant),
            ("a/./{tenant}.db", FleetMode::Tenant),
            ("a//{tenant}.db", FleetMode::Tenant),
            ("a\\{tenant}.db", FleetMode::Tenant),
            ("{tenant}", FleetMode::Tenant),
            ("{tenant}.", FleetMode::Tenant),
            ("{tenant}db", FleetMode::Tenant),
            ("{tenant}.db-wal", FleetMode::Tenant),
            ("{tenant}{slot}.db", FleetMode::Tenant),
            ("{tenant}/{tenant}.db", FleetMode::Tenant),
            ("{nope}/{tenant}.db", FleetMode::Tenant),
            ("{tenant.db", FleetMode::Tenant),
            ("tenant}.db", FleetMode::Tenant),
            ("{bucket}/all.db", FleetMode::Tenant),
            ("{bucket}/all.db", FleetMode::Slot),
            ("{slot}/{tenant}.db", FleetMode::Slot),
        ];
        for (raw, mode) in cases {
            assert!(
                FleetPathTemplate::parse(raw, mode).is_err(),
                "{raw:?} ({mode}) must be refused"
            );
        }
    }

    fn slot_of_len(id: &str) -> u16 {
        u16::try_from(id.len() * 100).unwrap()
    }

    #[test]
    fn paths_map_back_to_keys() {
        let t = FleetPathTemplate::parse("{bucket}/{tenant}.db", FleetMode::Tenant).unwrap();
        // "acme": slot 400, bucket 6.
        let key = t
            .key_for_path(Path::new("006/acme.db"), &slot_of_len)
            .unwrap();
        assert_eq!(key, tenant("acme", 400));
        assert_eq!(t.render(&key), PathBuf::from("006/acme.db"));
        // Wrong bucket: a stray copy, not a database.
        assert!(
            t.key_for_path(Path::new("007/acme.db"), &slot_of_len)
                .is_none()
        );
        // Sidecars and foreign files never enumerate.
        for path in [
            "006/acme.db-wal",
            "006/acme.db-shm",
            "006/acme.db-journal",
            "006/notes.txt",
        ] {
            assert!(
                t.key_for_path(Path::new(path), &slot_of_len).is_none(),
                "{path}"
            );
        }
        // Depth must match.
        assert!(t.key_for_path(Path::new("acme.db"), &slot_of_len).is_none());
    }

    #[test]
    fn tenant_capture_backtracks_over_literal_suffixes() {
        let t = FleetPathTemplate::parse("{tenant}-data.db", FleetMode::Tenant).unwrap();
        let key = t.key_for_path(Path::new("a-data-data.db"), &|_| 1).unwrap();
        assert_eq!(key.tenant().unwrap().as_str(), "a-data");
    }

    #[test]
    fn slot_paths_map_back_to_keys() {
        let s = FleetPathTemplate::parse("{bucket}/slot-{slot}.db", FleetMode::Slot).unwrap();
        let key = s
            .key_for_path(Path::new("000/slot-00042.db"), &|_| 0)
            .unwrap();
        assert_eq!(key, FleetDbKey::slot(42).unwrap());
        assert!(
            s.key_for_path(Path::new("001/slot-00042.db"), &|_| 0)
                .is_none()
        );
        assert!(
            s.key_for_path(Path::new("255/slot-16384.db"), &|_| 0)
                .is_none()
        );
        assert!(
            s.key_for_path(Path::new("000/slot-0042.db"), &|_| 0)
                .is_none()
        );
        assert!(
            s.key_for_path(Path::new("256/slot-16383.db"), &|_| 0)
                .is_none()
        );
    }

    #[test]
    fn walk_helpers_describe_segments() {
        let t = FleetPathTemplate::parse("fleet/{bucket}/{tenant}.db", FleetMode::Tenant).unwrap();
        assert_eq!(t.depth(), 3);
        assert_eq!(t.literal_segment(0), Some("fleet"));
        assert_eq!(t.literal_segment(1), None);
        assert!(t.segment_matches(1, "012"));
        assert!(!t.segment_matches(1, "12"));
        assert!(t.segment_matches(2, "acme.db"));
        assert!(!t.segment_matches(2, "acme.db-wal"));
        assert!(!t.segment_matches(3, "anything"));
    }

    #[test]
    fn sidecars_are_named_after_the_database() {
        let [wal, shm, journal] = sidecar_paths(Path::new("/x/acme.db"));
        assert_eq!(wal, PathBuf::from("/x/acme.db-wal"));
        assert_eq!(shm, PathBuf::from("/x/acme.db-shm"));
        assert_eq!(journal, PathBuf::from("/x/acme.db-journal"));
    }
}
