//! `autumn maintenance` — enable or disable maintenance mode.
//!
//! Writes (or deletes) a JSON flag file at `tmp/autumn-maintenance.json`
//! (relative to the current working directory). A running Autumn app polls
//! this file and updates its in-process [`autumn_web::maintenance::MaintenanceState`]
//! within 500 ms, so every replica enters the 503 window almost instantly
//! without a process restart.
//!
//! ## Usage
//!
//! ```text
//! autumn maintenance on [--message <MSG>] [--allow-ips <CIDR>...] [--readonly]
//!                       [--bypass-header <NAME:VALUE>]
//! autumn maintenance off
//! ```

use std::path::{Path, PathBuf};

use autumn_web::maintenance::{MAINTENANCE_FLAG_FILE, MaintenanceConfig, MaintenanceState};

/// Options for `autumn maintenance on`.
pub struct MaintenanceOnOptions<'a> {
    pub message: Option<&'a str>,
    pub allow_ips: &'a [String],
    pub readonly: bool,
    pub bypass_header: Option<(&'a str, &'a str)>,
    /// Override the default flag file path (used in tests).
    pub flag_file: Option<&'a Path>,
}

/// Enable maintenance mode: write the flag file and print confirmation.
pub fn run_on(opts: &MaintenanceOnOptions<'_>) {
    let path = resolved_flag_path(opts.flag_file);

    let config = MaintenanceConfig {
        message: opts.message.map(str::to_owned),
        allow_ips: opts.allow_ips.to_vec(),
        readonly: opts.readonly,
        bypass_header: opts
            .bypass_header
            .map(|(name, val)| (name.to_owned(), val.to_owned())),
    };

    match MaintenanceState::save_to_file(&path, &config) {
        Ok(()) => {
            eprintln!("\u{1F342} Maintenance mode ENABLED");
            eprintln!("   Flag file: {}", path.display());
            if let Some(msg) = &config.message {
                eprintln!("   Message:   {msg}");
            }
            if config.readonly {
                eprintln!("   Mode:      read-only (GET/HEAD/OPTIONS pass through)");
            }
            if !config.allow_ips.is_empty() {
                eprintln!("   Allow IPs: {}", config.allow_ips.join(", "));
            }
            if let Some((name, _)) = &config.bypass_header {
                eprintln!("   Bypass:    header {name}");
            }
            eprintln!();
            eprintln!("   Running app(s) will enter maintenance within 500 ms.");
            eprintln!("   Run `autumn maintenance off` to re-open the door.");
        }
        Err(e) => {
            eprintln!("\u{274C} Failed to write maintenance flag: {e}");
            std::process::exit(1);
        }
    }
}

/// Disable maintenance mode: delete the flag file and print confirmation.
pub fn run_off(flag_file: Option<&Path>) {
    let path = resolved_flag_path(flag_file);

    match MaintenanceState::remove_flag_file(&path) {
        Ok(true) => {
            eprintln!("\u{1F342} Maintenance mode DISABLED");
            eprintln!("   Normal traffic will resume within 500 ms.");
        }
        Ok(false) => {
            eprintln!("\u{26A0}\u{FE0F}  Maintenance mode was not active (flag file not found).");
        }
        Err(e) => {
            eprintln!("\u{274C} Failed to remove maintenance flag: {e}");
            std::process::exit(1);
        }
    }
}

/// Read and return the current maintenance config, if any.
pub fn check_status(flag_file: Option<&Path>) -> Option<MaintenanceConfig> {
    let path = resolved_flag_path(flag_file);
    MaintenanceState::load_from_file(&path).unwrap_or(None)
}

/// Parse a `NAME:VALUE` bypass-header argument, returning `(name, value)`.
///
/// Returns an error string if the format is invalid.
///
/// # Errors
///
/// Returns `Err(String)` when the argument does not contain `:`.
pub fn parse_bypass_header(arg: &str) -> Result<(&str, &str), String> {
    arg.split_once(':').ok_or_else(|| {
        format!(
            "invalid --bypass-header '{arg}'; expected NAME:VALUE \
             (e.g. X-Autumn-Maintenance-Bypass:my-token)"
        )
    })
}

fn resolved_flag_path(override_: Option<&Path>) -> PathBuf {
    override_.map_or_else(|| PathBuf::from(MAINTENANCE_FLAG_FILE), Path::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bypass_header_valid() {
        let (name, value) = parse_bypass_header("X-Bypass:secret").unwrap();
        assert_eq!(name, "X-Bypass");
        assert_eq!(value, "secret");
    }

    #[test]
    fn parse_bypass_header_colon_in_value() {
        let (name, value) = parse_bypass_header("X-Token:tok:en").unwrap();
        assert_eq!(name, "X-Token");
        assert_eq!(value, "tok:en");
    }

    #[test]
    fn parse_bypass_header_missing_colon_is_error() {
        assert!(parse_bypass_header("NoColon").is_err());
    }

    #[test]
    fn run_on_writes_flag_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("maintenance.json");

        run_on(&MaintenanceOnOptions {
            message: Some("testing"),
            allow_ips: &[],
            readonly: false,
            bypass_header: None,
            flag_file: Some(&path),
        });

        assert!(path.exists(), "flag file should be created");
        let config = MaintenanceState::load_from_file(&path).unwrap().unwrap();
        assert_eq!(config.message.as_deref(), Some("testing"));
    }

    #[test]
    fn run_on_with_all_options() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("maintenance.json");

        run_on(&MaintenanceOnOptions {
            message: Some("deploying"),
            allow_ips: &["10.0.0.0/8".into()],
            readonly: true,
            bypass_header: Some(("X-Bypass", "tok")),
            flag_file: Some(&path),
        });

        let config = MaintenanceState::load_from_file(&path).unwrap().unwrap();
        assert_eq!(config.message.as_deref(), Some("deploying"));
        assert!(config.readonly);
        assert_eq!(config.allow_ips, vec!["10.0.0.0/8"]);
        assert_eq!(
            config
                .bypass_header
                .as_ref()
                .map(|(n, v)| (n.as_str(), v.as_str())),
            Some(("X-Bypass", "tok"))
        );
    }

    #[test]
    fn run_off_removes_flag_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("maintenance.json");
        std::fs::write(&path, "{}").unwrap();

        run_off(Some(&path));

        assert!(!path.exists(), "flag file should be removed");
    }

    #[test]
    fn run_off_no_op_when_not_active() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("maintenance.json");
        run_off(Some(&path));
    }

    #[test]
    fn check_status_returns_none_when_off() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("maintenance.json");
        assert!(check_status(Some(&path)).is_none());
    }

    #[test]
    fn check_status_returns_config_when_on() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("maintenance.json");
        let config = MaintenanceConfig {
            message: Some("on".into()),
            ..Default::default()
        };
        MaintenanceState::save_to_file(&path, &config).unwrap();
        let found = check_status(Some(&path)).unwrap();
        assert_eq!(found.message.as_deref(), Some("on"));
    }
}
