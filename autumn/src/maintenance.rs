//! Maintenance mode state and file-flag coordinator.
//!
//! [`MaintenanceState`] is the in-process source of truth, shared between the
//! [`crate::middleware::maintenance::MaintenanceLayer`] and a background task
//! that polls the flag file written by `autumn maintenance on/off`.
//!
//! # File-flag protocol
//!
//! - **On**: `autumn maintenance on` writes [`MAINTENANCE_FLAG_FILE`] as JSON.
//! - **Off**: `autumn maintenance off` deletes the file.
//! - **Middleware**: a background task polls the file every 500 ms and updates
//!   the in-process [`MaintenanceState`] so every request is decided
//!   in-memory without disk I/O.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// Default path for the maintenance flag file, relative to the project root.
pub const MAINTENANCE_FLAG_FILE: &str = "tmp/autumn-maintenance.json";

/// File name of the maintenance flag, without any directory part.
///
/// Split out from [`MAINTENANCE_FLAG_FILE`] (whose tail it is — pinned by
/// `maintenance_flag_basename_is_the_tail_of_the_legacy_path`) so a deploy tool
/// can compose the flag's path under a *different* directory — the per-app
/// `shared/` dir — without re-declaring the file name. Autumn's house rule is that
/// anything the framework and a CLI both depend on lives here and is imported.
pub const MAINTENANCE_FLAG_BASENAME: &str = "autumn-maintenance.json";

/// Environment variable that overrides where the runtime reads the maintenance
/// flag file (issue #1621).
///
/// # Why this exists
///
/// [`MAINTENANCE_FLAG_FILE`] is **relative to the process's working directory**,
/// and a deploy-managed slot unit sets `WorkingDirectory={release_dir}` — a fresh
/// directory on every release. So, before this override:
///
/// - a flag written into the blue slot's release dir is invisible to the green
///   slot, and
/// - the next cutover **orphans** the flag entirely, silently un-maintaining the
///   host with no error and no log line.
///
/// A maintenance switch that lies is worse than no switch, so `autumn deploy`
/// stamps this variable into every slot unit, pointing it at the per-app
/// `shared/` directory that survives cutovers, rollbacks and pruning and is shared
/// by both slots.
///
/// Unset (the default, and every non-deploy-managed app) → behaviour is exactly as
/// before: the cwd-relative [`MAINTENANCE_FLAG_FILE`].
pub const MAINTENANCE_FLAG_FILE_ENV: &str = "AUTUMN_MAINTENANCE_FLAG_FILE";

/// Resolve the maintenance flag file path from an already-read override value —
/// the pure core of [`resolve_flag_file_path`] (issue #1621).
///
/// A blank/whitespace-only override is treated as unset: an empty env var is the
/// shape a half-written unit file or a `Environment=AUTUMN_MAINTENANCE_FLAG_FILE=`
/// line produces, and resolving it to `""` would make every poll fail to find the
/// flag with no diagnostic. Falling back to the legacy path keeps the pre-#1621
/// behaviour in exactly the cases where the override says nothing.
#[must_use]
pub fn flag_file_path_from(override_value: Option<&str>) -> PathBuf {
    override_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(MAINTENANCE_FLAG_FILE), PathBuf::from)
}

/// The path the runtime reads (and re-polls) the maintenance flag file from.
///
/// Honours [`MAINTENANCE_FLAG_FILE_ENV`], defaulting to the cwd-relative
/// [`MAINTENANCE_FLAG_FILE`] — so behaviour is unchanged when the variable is
/// unset. Consumed at BOTH runtime read sites (the synchronous boot load and the
/// 500 ms poller) so the two can never disagree about where the flag lives.
#[must_use]
pub fn resolve_flag_file_path() -> PathBuf {
    flag_file_path_from(std::env::var(MAINTENANCE_FLAG_FILE_ENV).ok().as_deref())
}

/// Configuration for an active maintenance window.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceConfig {
    /// Human-readable message shown to users during maintenance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// CIDR allow-list; clients matching any entry bypass the 503.
    #[serde(default)]
    pub allow_ips: Vec<String>,
    /// When `true`, only write methods (POST/PUT/PATCH/DELETE) are gated;
    /// GET/HEAD/OPTIONS continue to work ("read-only" mode).
    #[serde(default)]
    pub readonly: bool,
    /// Optional bypass header `(header_name, expected_value)`.
    /// Requests carrying this header with the matching value bypass the 503.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bypass_header: Option<(String, String)>,
}

/// In-process maintenance state, cheaply cloneable across threads.
///
/// Wraps `Arc<RwLock<Option<MaintenanceConfig>>>`:
/// - `None`  → maintenance is off
/// - `Some(config)` → maintenance is on with that config
#[derive(Clone, Debug, Default)]
pub struct MaintenanceState(Arc<RwLock<Option<MaintenanceConfig>>>);

impl MaintenanceState {
    /// Create a new [`MaintenanceState`] with maintenance initially off.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` when maintenance mode is currently active.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (a thread panicked while holding the lock).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.0
            .read()
            .expect("maintenance state lock poisoned")
            .is_some()
    }

    /// Returns the active [`MaintenanceConfig`], or `None` when off.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (a thread panicked while holding the lock).
    #[must_use]
    pub fn get(&self) -> Option<MaintenanceConfig> {
        self.0
            .read()
            .expect("maintenance state lock poisoned")
            .clone()
    }

    /// Enable maintenance mode with the given config.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (a thread panicked while holding the lock).
    pub fn enable(&self, config: MaintenanceConfig) {
        *self.0.write().expect("maintenance state lock poisoned") = Some(config);
    }

    /// Disable maintenance mode.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned (a thread panicked while holding the lock).
    pub fn disable(&self) {
        *self.0.write().expect("maintenance state lock poisoned") = None;
    }

    /// Load state from a JSON flag file written by `autumn maintenance on`.
    ///
    /// Returns `Ok(None)` when the file does not exist (maintenance is off).
    ///
    /// # Errors
    ///
    /// Returns `Err` for I/O errors other than `NotFound`, or when the file
    /// content is not valid JSON for [`MaintenanceConfig`].
    pub fn load_from_file(path: &Path) -> std::io::Result<Option<MaintenanceConfig>> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let config: MaintenanceConfig = serde_json::from_str(&s)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                Ok(Some(config))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Write a [`MaintenanceConfig`] to the flag file.
    ///
    /// Creates parent directories (e.g. `tmp/`) if they do not exist.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the directory cannot be created, the config cannot be
    /// serialised to JSON, or the file cannot be written.
    pub fn save_to_file(path: &Path, config: &MaintenanceConfig) -> std::io::Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(config).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    /// Delete the flag file, turning maintenance off.
    ///
    /// Returns `Ok(true)` when the file was deleted, `Ok(false)` when it was
    /// already absent.
    ///
    /// # Errors
    ///
    /// Returns `Err` for filesystem errors other than `NotFound`.
    pub fn remove_flag_file(path: &Path) -> std::io::Result<bool> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Check whether `client_ip` is covered by any entry in `allow_ips`.
///
/// Each entry may be an exact IP address or an IPv4/IPv6 CIDR block.
/// Invalid entries are silently skipped.
#[must_use]
pub fn ip_in_allow_list(client_ip: &IpAddr, allow_ips: &[String]) -> bool {
    for entry in allow_ips {
        if let Some((prefix, bits)) = entry.split_once('/') {
            if let (Ok(network_ip), Ok(prefix_len)) = (prefix.parse::<IpAddr>(), bits.parse::<u8>())
                && ip_in_cidr(client_ip, &network_ip, prefix_len)
            {
                return true;
            }
        } else if let Ok(allowed) = entry.parse::<IpAddr>()
            && client_ip == &allowed
        {
            return true;
        }
    }
    false
}

const fn ip_in_cidr(ip: &IpAddr, network: &IpAddr, prefix_len: u8) -> bool {
    match (ip, network) {
        (IpAddr::V4(ip), IpAddr::V4(net)) => {
            if prefix_len > 32 {
                return false;
            }
            let ip_bits = u32::from_be_bytes(ip.octets());
            let net_bits = u32::from_be_bytes(net.octets());
            let mask = if prefix_len == 0 {
                0u32
            } else {
                !0u32 << (32 - prefix_len)
            };
            (ip_bits & mask) == (net_bits & mask)
        }
        (IpAddr::V6(ip), IpAddr::V6(net)) => {
            if prefix_len > 128 {
                return false;
            }
            let ip_bits = u128::from_be_bytes(ip.octets());
            let net_bits = u128::from_be_bytes(net.octets());
            let mask = if prefix_len == 0 {
                0u128
            } else {
                !0u128 << (128 - prefix_len)
            };
            (ip_bits & mask) == (net_bits & mask)
        }
        _ => false, // IPv4 vs IPv6 mismatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── MaintenanceState ──────────────────────────────────────────────────────

    #[test]
    fn maintenance_state_default_is_off() {
        let state = MaintenanceState::new();
        assert!(!state.is_active());
        assert!(state.get().is_none());
    }

    #[test]
    fn maintenance_state_enable_sets_active() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig::default());
        assert!(state.is_active());
    }

    #[test]
    fn maintenance_state_disable_clears_active() {
        let state = MaintenanceState::new();
        state.enable(MaintenanceConfig::default());
        state.disable();
        assert!(!state.is_active());
        assert!(state.get().is_none());
    }

    #[test]
    fn maintenance_state_get_returns_config() {
        let state = MaintenanceState::new();
        let config = MaintenanceConfig {
            message: Some("deploying".into()),
            readonly: true,
            ..Default::default()
        };
        state.enable(config.clone());
        assert_eq!(state.get().unwrap(), config);
    }

    #[test]
    fn maintenance_state_is_clone_safe() {
        let state = MaintenanceState::new();
        let clone = state.clone();
        state.enable(MaintenanceConfig::default());
        // Clone shares the underlying Arc
        assert!(clone.is_active());
    }

    // ── File operations ───────────────────────────────────────────────────────

    #[test]
    fn load_from_file_returns_none_when_missing() {
        let path = std::path::Path::new("/tmp/autumn_nonexistent_maintenance_test_xyz.json");
        let result = MaintenanceState::load_from_file(path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn save_and_load_round_trips() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("maintenance.json");
        let config = MaintenanceConfig {
            message: Some("Deploying new version".into()),
            allow_ips: vec!["192.168.1.0/24".into()],
            readonly: false,
            bypass_header: Some(("X-Bypass".into(), "secret-token".into())),
        };
        MaintenanceState::save_to_file(&path, &config).unwrap();
        let loaded = MaintenanceState::load_from_file(&path).unwrap().unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn save_creates_parent_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("tmp").join("maintenance.json");
        let config = MaintenanceConfig::default();
        MaintenanceState::save_to_file(&path, &config).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn remove_flag_file_returns_true_when_deleted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("maintenance.json");
        std::fs::write(&path, "{}").unwrap();
        assert!(MaintenanceState::remove_flag_file(&path).unwrap());
    }

    #[test]
    fn remove_flag_file_returns_false_when_already_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("maintenance.json");
        assert!(!MaintenanceState::remove_flag_file(&path).unwrap());
    }

    #[test]
    fn maintenance_config_default_has_no_message() {
        let c = MaintenanceConfig::default();
        assert!(c.message.is_none());
        assert!(c.allow_ips.is_empty());
        assert!(!c.readonly);
        assert!(c.bypass_header.is_none());
    }

    // ── Flag-file path resolution (issue #1621) ───────────────────────────────

    #[test]
    fn flag_file_path_honors_the_env_override() {
        // #1621 R1: a deploy-managed slot unit stamps
        // `Environment=AUTUMN_MAINTENANCE_FLAG_FILE={app_dir}/shared/…` so the flag
        // lives OUTSIDE the per-release WorkingDirectory a cutover replaces. Without
        // this the flag is orphaned on every deploy and the host silently
        // un-maintains itself.
        assert_eq!(
            flag_file_path_from(Some("/srv/autumn/shop/shared/autumn-maintenance.json")),
            PathBuf::from("/srv/autumn/shop/shared/autumn-maintenance.json"),
        );
    }

    #[test]
    fn flag_file_path_without_override_is_the_legacy_cwd_relative_path() {
        // Unset → byte-for-byte pre-#1621 behaviour, so every app that does not go
        // through `autumn deploy` is unaffected.
        assert_eq!(
            flag_file_path_from(None),
            PathBuf::from(MAINTENANCE_FLAG_FILE),
        );
    }

    #[test]
    fn flag_file_path_treats_a_blank_override_as_unset() {
        // An empty/whitespace value is what a half-written unit line produces;
        // resolving it to "" would make every poll silently miss the flag.
        assert_eq!(
            flag_file_path_from(Some("")),
            PathBuf::from(MAINTENANCE_FLAG_FILE),
        );
        assert_eq!(
            flag_file_path_from(Some("   ")),
            PathBuf::from(MAINTENANCE_FLAG_FILE),
        );
    }

    #[test]
    fn resolve_flag_file_path_reads_the_documented_env_var() {
        // The env-reading wrapper is a one-liner over `flag_file_path_from`; pin the
        // variable NAME (it is a deploy-unit contract) and the unset default. Guarded
        // rather than mutating process env, which is `unsafe` in edition 2024.
        assert_eq!(MAINTENANCE_FLAG_FILE_ENV, "AUTUMN_MAINTENANCE_FLAG_FILE");
        if std::env::var(MAINTENANCE_FLAG_FILE_ENV).is_err() {
            assert_eq!(
                resolve_flag_file_path(),
                PathBuf::from(MAINTENANCE_FLAG_FILE)
            );
        }
    }

    #[test]
    fn maintenance_flag_basename_is_the_tail_of_the_legacy_path() {
        // The deploy CLI composes `{app_dir}/shared/{BASENAME}` and
        // `{release_dir}/{MAINTENANCE_FLAG_FILE}`; both must name the same file, or a
        // host deployed before #1621 and one deployed after would read different
        // files under the same command.
        assert!(MAINTENANCE_FLAG_FILE.ends_with(MAINTENANCE_FLAG_BASENAME));
        assert_eq!(
            Path::new(MAINTENANCE_FLAG_FILE).file_name().unwrap(),
            MAINTENANCE_FLAG_BASENAME,
        );
    }

    // ── IP allow-list ─────────────────────────────────────────────────────────

    #[test]
    fn ip_in_allow_list_exact_ipv4_match() {
        let ip: IpAddr = "192.168.1.5".parse().unwrap();
        assert!(ip_in_allow_list(&ip, &["192.168.1.5".into()]));
    }

    #[test]
    fn ip_in_allow_list_exact_ipv4_no_match() {
        let ip: IpAddr = "192.168.1.5".parse().unwrap();
        assert!(!ip_in_allow_list(&ip, &["192.168.1.6".into()]));
    }

    #[test]
    fn ip_in_allow_list_cidr_v4_in_range() {
        let ip: IpAddr = "192.168.1.50".parse().unwrap();
        assert!(ip_in_allow_list(&ip, &["192.168.1.0/24".into()]));
    }

    #[test]
    fn ip_in_allow_list_cidr_v4_out_of_range() {
        let ip: IpAddr = "192.168.2.50".parse().unwrap();
        assert!(!ip_in_allow_list(&ip, &["192.168.1.0/24".into()]));
    }

    #[test]
    fn ip_in_allow_list_cidr_v4_boundary() {
        let ip: IpAddr = "10.0.0.255".parse().unwrap();
        assert!(ip_in_allow_list(&ip, &["10.0.0.0/24".into()]));
        let outside: IpAddr = "10.0.1.0".parse().unwrap();
        assert!(!ip_in_allow_list(&outside, &["10.0.0.0/24".into()]));
    }

    #[test]
    fn ip_in_allow_list_loopback() {
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(ip_in_allow_list(&ip, &["127.0.0.0/8".into()]));
        assert!(ip_in_allow_list(&ip, &["127.0.0.1".into()]));
    }

    #[test]
    fn ip_in_allow_list_empty_returns_false() {
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(!ip_in_allow_list(&ip, &[]));
    }

    #[test]
    fn ip_in_allow_list_ipv6_exact() {
        let ip: IpAddr = "::1".parse().unwrap();
        assert!(ip_in_allow_list(&ip, &["::1".into()]));
    }

    #[test]
    fn ip_in_allow_list_mixed_family_no_match() {
        let ipv4: IpAddr = "127.0.0.1".parse().unwrap();
        // IPv6 loopback in allow list should not match IPv4 loopback
        assert!(!ip_in_allow_list(&ipv4, &["::1".into()]));
    }

    #[test]
    fn ip_in_allow_list_multiple_entries() {
        let ip: IpAddr = "10.10.10.10".parse().unwrap();
        let allow = vec!["192.168.1.1".into(), "10.10.0.0/16".into()];
        assert!(ip_in_allow_list(&ip, &allow));
    }

    #[test]
    fn ip_in_allow_list_invalid_entry_skipped() {
        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        // Invalid CIDR entries are silently skipped; only valid ones checked
        assert!(!ip_in_allow_list(
            &ip,
            &["not-an-ip".into(), "999.999.999.999".into()]
        ));
    }
}
