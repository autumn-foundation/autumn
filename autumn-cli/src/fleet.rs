//! `autumn fleet`: operate a `SQLite` database fleet (ADR 0019) from the
//! command line (issue #3232).
//!
//! Each subcommand calls the `DatabaseFleet` API. The config resolves as for
//! `autumn db replica`: `autumn.toml`, the `[profile.<p>]` overlay,
//! `.env.<profile>` and `AUTUMN_*` overrides.
//!
//! The fleet API is in the `sqlite` build of `autumn-web` only. The default
//! (Postgres) build keeps the command and tells the operator which build to
//! use.

use std::path::PathBuf;

/// The subcommands of `autumn fleet`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetCommand {
    /// Show every database on disk.
    List {
        /// Profile overlay for the config.
        profile: Option<String>,
        /// Also read the identity row and migrations of each file.
        open: bool,
        /// Write JSON to stdout instead of a table.
        json: bool,
    },
    /// Apply pending migrations.
    Migrate {
        /// Profile overlay for the config.
        profile: Option<String>,
        /// The number of databases to migrate at the same time.
        concurrency: usize,
        /// Migrate only this database.
        key: Option<String>,
    },
    /// Copy one database to a new file with `VACUUM INTO`.
    Backup {
        /// Profile overlay for the config.
        profile: Option<String>,
        /// The database name.
        key: String,
        /// The path of the new file.
        dest: PathBuf,
    },
    /// Remove one database.
    Delete {
        /// Profile overlay for the config.
        profile: Option<String>,
        /// The database name.
        key: String,
        /// Confirm the deletion.
        force: bool,
    },
    /// Rebuild one database from its replica.
    Restore {
        /// Profile overlay for the config.
        profile: Option<String>,
        /// The database name.
        key: String,
        /// Restore to this RFC 3339 instant instead of the latest state.
        timestamp: Option<String>,
        /// Confirm the replacement.
        force: bool,
    },
}

/// Entry point dispatched from `main`. Exits non-zero on failure.
pub fn run(command: &FleetCommand) {
    eprintln!("\u{1F342} autumn fleet\n");
    if let Err(e) = imp::run(command) {
        eprintln!("\u{2717} {e}");
        std::process::exit(1);
    }
}

#[cfg(not(feature = "sqlite"))]
mod imp {
    use super::FleetCommand;

    pub fn run(_: &FleetCommand) -> Result<(), String> {
        Err(
            "`autumn fleet` needs the SQLite build of the CLI.\n  Install it with \
             `cargo install autumn-cli --no-default-features --features sqlite`, or \
             build it with `cargo build -p autumn-cli --no-default-features --features sqlite`."
                .to_owned(),
        )
    }
}

#[cfg(feature = "sqlite")]
mod imp {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use autumn_web::config::{AutumnConfig, DatabaseFleetConfig};
    use autumn_web::db::fleet::{DatabaseFleet, FleetError};
    use autumn_web::db::fleet_replication::{FleetReplication, replica_root};
    use autumn_web::fleet_layout::FleetDbKey;
    use autumn_web::replication::ReplicationSettings;

    use super::FleetCommand;
    use crate::db::replica;

    /// The app's migration directory, relative to the app directory.
    const MIGRATIONS_DIR: &str = "migrations";

    pub fn run(command: &FleetCommand) -> Result<(), String> {
        match command {
            FleetCommand::List {
                profile,
                open,
                json,
            } => list(&Target::load(profile.as_deref())?, *open, *json),
            FleetCommand::Migrate {
                profile,
                concurrency,
                key,
            } => migrate(
                &Target::load(profile.as_deref())?,
                *concurrency,
                key.as_deref(),
            ),
            FleetCommand::Backup { profile, key, dest } => {
                backup(&Target::load(profile.as_deref())?, key, dest)
            }
            FleetCommand::Delete {
                profile,
                key,
                force,
            } => delete(&Target::load(profile.as_deref())?, key, *force),
            FleetCommand::Restore {
                profile,
                key,
                timestamp,
                force,
            } => restore(
                &Target::load(profile.as_deref())?,
                key,
                timestamp.as_deref(),
                *force,
            ),
        }
    }

    /// The resolved config and its `[database.fleet]` section.
    struct Target {
        profile: String,
        config: AutumnConfig,
        fleet: DatabaseFleetConfig,
    }

    impl Target {
        fn load(profile: Option<&str>) -> Result<Self, String> {
            let (profile, config) = replica::load_app_config(profile).map_err(|e| match e {
                // The shared loader labels its errors for `[replication]`.
                replica::ReplicaError::Config { detail } => {
                    format!("Invalid configuration: {detail}")
                }
                other => other.to_string(),
            })?;
            let fleet = config.database.fleet.clone().ok_or_else(|| {
                "No [database.fleet] section is configured.\n  Add one to autumn.toml, or \
                 set AUTUMN_DATABASE__FLEET__MODE and AUTUMN_DATABASE__FLEET__ROOT."
                    .to_owned()
            })?;
            // The checks boot makes: a file-backed SQLite control database,
            // no `[[database.shards]]` beside the fleet, and the rest.
            config
                .database
                .validate()
                .map_err(|e| format!("Invalid [database] configuration: {e}"))?;
            Ok(Self {
                profile,
                config,
                fleet,
            })
        }

        /// Refuse a root that does not exist. The fleet builder creates the
        /// root, so a wrong working directory gives an empty fleet.
        fn require_root(&self) -> Result<(), String> {
            let root = Path::new(&self.fleet.root);
            if root.is_dir() {
                return Ok(());
            }
            Err(format!(
                "database.fleet.root {} does not exist.\n  Run this command from the app \
                 directory. For a new volume, create the directory first.",
                root.display()
            ))
        }

        /// The fleet, with `migrations` when given. An open never migrates.
        fn build(
            &self,
            migrations: Option<diesel_migrations::FileBasedMigrations>,
        ) -> Result<DatabaseFleet, String> {
            self.require_root()?;
            let mut builder = DatabaseFleet::builder(self.fleet.clone())
                .connect_timeout_secs(self.config.database.connect_timeout_secs)
                .apply_migrations_on_open(false);
            if let Some(migrations) = migrations {
                builder = builder
                    .migration_source("app", migrations)
                    .framework_migrations();
            }
            let fleet = builder.build().map_err(|e| e.to_string())?;
            // The check boot makes: a key must not reach the control database.
            let control: Vec<&str> = self
                .config
                .database
                .effective_primary_url()
                .into_iter()
                .collect();
            fleet.check_control_outside_root(&control)?;
            Ok(fleet)
        }

        fn replicated(&self) -> bool {
            self.config.replication.as_ref().is_some_and(|r| r.enabled)
        }
    }

    fn runtime() -> Result<tokio::runtime::Runtime, String> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("could not start the async runtime: {e}"))
    }

    /// Parse a database name as `autumn fleet list` prints it. `tenant/<id>`
    /// and `slot/<n>` are also accepted.
    fn parse_key(fleet: &DatabaseFleet, raw: &str) -> Result<FleetDbKey, String> {
        let raw = raw.trim();
        let name = ["tenant", "slot"]
            .iter()
            .find_map(|kind| {
                raw.strip_prefix(kind)
                    .and_then(|rest| rest.strip_prefix('/'))
                    .map(|rest| format!("{kind}:{rest}"))
            })
            .unwrap_or_else(|| raw.to_owned());
        fleet.key_for_name(&name).map_err(|e| {
            format!(
                "{raw:?} is not a database name of this fleet: {e}\n  Write tenant:<id> or \
                 slot:<nnnnn>, as `autumn fleet list` prints it."
            )
        })
    }

    /// Bytes on disk: the database file and its `-wal`.
    fn bytes_on_disk(path: &Path) -> u64 {
        let len = |p: &Path| std::fs::metadata(p).map_or(0, |m| m.len());
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        len(path) + len(Path::new(&wal))
    }

    /// What the `--open` columns compare a database with.
    struct AppView {
        /// The versions in `migrations/`, as named on disk.
        versions: Vec<String>,
        /// Every version the CLI's sets record, sorted.
        known: Vec<String>,
        /// No two of the CLI's sets share a version, so a version on disk is
        /// the version a database records.
        exact: bool,
    }

    /// The app view, or `None` when `migrations/` cannot be read.
    fn app_view(target: &Target) -> Option<AppView> {
        let source = diesel_migrations::FileBasedMigrations::from_path(MIGRATIONS_DIR).ok()?;
        let versions =
            diesel::migration::MigrationSource::<diesel::sqlite::Sqlite>::migrations(&source)
                .ok()?
                .iter()
                .map(|m| m.name().version().to_string())
                .collect();
        let fleet = target.build(Some(source)).ok()?;
        Some(AppView {
            versions,
            known: fleet.known_migration_versions(),
            exact: fleet.remapped_migrations().is_empty(),
        })
    }

    /// The `--open` columns: the latest applied app migration and the number
    /// of app migrations not applied. Without an exact app view (no
    /// `migrations/`, a shared version, or a database that records a version
    /// of another set), the latest applied migration of any set and no
    /// pending count: boot can record an app migration under a substitute.
    fn migration_columns(
        applied: &[String],
        app: Option<&AppView>,
    ) -> (Option<String>, Option<usize>) {
        let app = app
            .filter(|app| app.exact && applied.iter().all(|v| app.known.binary_search(v).is_ok()));
        let Some(app) = app else {
            return (applied.iter().max().cloned(), None);
        };
        let latest = app
            .versions
            .iter()
            .filter(|v| applied.contains(v))
            .max()
            .cloned();
        let pending = app.versions.iter().filter(|v| !applied.contains(v)).count();
        (latest, Some(pending))
    }

    fn list(target: &Target, open: bool, json: bool) -> Result<(), String> {
        let fleet = target.build(None)?;
        let rt = runtime()?;
        let keys = rt.block_on(fleet.list()).map_err(|e| e.to_string())?;
        eprintln!("  \u{2139} root: {}", fleet.root().display());
        let app = if open { app_view(target) } else { None };

        let mut rows = Vec::with_capacity(keys.len());
        let mut failed = 0_usize;
        for key in &keys {
            let path = fleet.path_of(key);
            let mut row = serde_json::json!({
                "key": key.name(),
                "path": path.display().to_string(),
                "bytes": bytes_on_disk(&path),
            });
            if open {
                match rt.block_on(fleet.inspect(key)) {
                    Ok(info) => {
                        let (latest, pending) =
                            migration_columns(&info.applied_migrations, app.as_ref());
                        row["identity"] = info.identity.into();
                        row["migration_version"] = latest.into();
                        row["pending"] = pending.into();
                    }
                    Err(e) => {
                        failed += 1;
                        row["identity"] = serde_json::Value::Null;
                        row["migration_version"] = serde_json::Value::Null;
                        row["pending"] = serde_json::Value::Null;
                        row["error"] = e.to_string().into();
                    }
                }
            }
            rows.push(row);
        }

        if json {
            println!("{}", serde_json::Value::Array(rows));
        } else {
            print_table(&rows, open);
        }
        if failed > 0 {
            return Err(format!(
                "{failed} of {} database(s) could not be read.",
                keys.len()
            ));
        }
        eprintln!("\n\u{2713} {} database(s).", keys.len());
        Ok(())
    }

    fn print_table(rows: &[serde_json::Value], open: bool) {
        let mut columns = vec!["key", "bytes"];
        if open {
            columns.extend(["identity", "migration_version", "pending"]);
        }
        columns.push("path");
        let cell = |row: &serde_json::Value, column: &str| match &row[column] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => "-".to_owned(),
            other => other.to_string(),
        };
        let widths: Vec<usize> = columns
            .iter()
            .map(|c| {
                rows.iter()
                    .map(|row| cell(row, c).len())
                    .chain([c.len()])
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let line = |values: Vec<String>| {
            let padded: Vec<String> = values
                .iter()
                .zip(&widths)
                .map(|(v, w)| format!("{v:<w$}"))
                .collect();
            println!("{}", padded.join("  ").trim_end());
        };
        line(columns.iter().map(|c| c.to_uppercase()).collect());
        for row in rows {
            line(columns.iter().map(|c| cell(row, c)).collect());
            if let Some(error) = row.get("error").and_then(serde_json::Value::as_str) {
                println!("  \u{2717} {error}");
            }
        }
    }

    fn migrate(target: &Target, concurrency: usize, key: Option<&str>) -> Result<(), String> {
        target.require_root()?;
        let dir = Path::new(MIGRATIONS_DIR);
        if !dir.is_dir() {
            return Err(format!(
                "Migrations directory not found: {MIGRATIONS_DIR}/\n  Run this command from \
                 the app directory."
            ));
        }
        let source = diesel_migrations::FileBasedMigrations::from_path(dir)
            .map_err(|e| format!("could not read {MIGRATIONS_DIR}/: {e}"))?;
        let fleet = target.build(Some(source))?;
        // A shared version gets a substitute that depends on every set the app
        // registers. This command does not have them all, so it could record
        // other versions than boot does.
        let remapped = fleet.remapped_migrations();
        if !remapped.is_empty() {
            return Err(format!(
                "These migrations share a version with another migration: {}.\n  \
                 `autumn fleet migrate` cannot record them as boot does. Run the app once \
                 with AUTUMN_MIGRATE=1 instead.",
                remapped.join(", ")
            ));
        }
        let rt = runtime()?;
        let keys = match key {
            Some(raw) => vec![parse_key(&fleet, raw)?],
            None => rt.block_on(fleet.list()).map_err(|e| e.to_string())?,
        };
        let keys = refuse_unknown_versions(&rt, &fleet, keys)?;
        if target.replicated() {
            eprintln!(
                "  \u{2139} [replication] is on. A migration of a database that a running \
                 app has open can make its replicator start a new generation."
            );
        }

        if let Some(raw) = key {
            let [key] = keys.as_slice() else {
                return Err(format!("{raw} does not exist."));
            };
            let applied = rt.block_on(fleet.migrate(key)).map_err(|e| e.to_string())?;
            eprintln!("\u{2713} {key}: {applied} migration(s) applied.");
            return Ok(());
        }

        // Exactly the checked keys: a database made since the check is not
        // visited (the app made it with every set).
        let report = rt.block_on(fleet.migrate_keys(keys, concurrency));
        for (key, error) in &report.failed {
            eprintln!("  \u{2717} {key}: {error}");
        }
        if !report.failed.is_empty() {
            return Err(format!(
                "{} of {} database(s) failed. {} migration(s) applied to the other \
                 databases.",
                report.failed.len(),
                report.databases,
                report.applied
            ));
        }
        eprintln!(
            "\u{2713} {} database(s), {} migration(s) applied.",
            report.databases, report.applied
        );
        Ok(())
    }

    /// Refuse to migrate when a database records a version that none of this
    /// command's sets has. Another set (a plugin's) migrates these databases,
    /// and the version map depends on it, so only boot can migrate them.
    /// Returns the keys to migrate: those inspected, less those deleted since
    /// the list.
    fn refuse_unknown_versions(
        rt: &tokio::runtime::Runtime,
        fleet: &DatabaseFleet,
        keys: Vec<FleetDbKey>,
    ) -> Result<Vec<FleetDbKey>, String> {
        let known = fleet.known_migration_versions();
        let mut checked = Vec::with_capacity(keys.len());
        let mut found = Vec::new();
        for key in keys {
            let info = match rt.block_on(fleet.inspect(&key)) {
                Ok(info) => info,
                // Deleted since the list: not migrated, so never recreated.
                Err(FleetError::NotFound { .. }) => continue,
                // Unchecked means unsafe: the check must not be skipped.
                Err(e) => return Err(format!("could not check {key} before the migration: {e}")),
            };
            let unknown: Vec<String> = info
                .applied_migrations
                .into_iter()
                .filter(|v| known.binary_search(v).is_err())
                .collect();
            if !unknown.is_empty() {
                found.push(format!("{key}: {}", unknown.join(", ")));
            }
            checked.push(key);
        }
        if found.is_empty() {
            return Ok(checked);
        }
        Err(format!(
            "These databases record migrations that are not in {MIGRATIONS_DIR}/ or the \
             framework sets:\n  {}\n  A plugin set probably applied them. Run the app once \
             with AUTUMN_MIGRATE=1 instead.",
            found.join("\n  ")
        ))
    }

    fn backup(target: &Target, key: &str, dest: &Path) -> Result<(), String> {
        let fleet = target.build(None)?;
        let key = parse_key(&fleet, key)?;
        let taken = || {
            format!(
                "{} already exists.\n  Choose a new path: a backup never replaces a file.",
                dest.display()
            )
        };
        // `symlink_metadata`: a dangling symlink is taken too.
        if dest.symlink_metadata().is_ok() {
            return Err(taken());
        }
        // The copy holds tenant data. It is written in a new directory, then
        // linked to `dest`. A link never replaces or follows a file another
        // user put at `dest`. On Unix only this user can open the directory
        // and the copy. On Windows both get the ACL of `dest`'s directory.
        #[cfg(unix)]
        nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
        let parent = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        #[cfg(unix)]
        refuse_shared_parent(parent)?;
        let staging =
            replica::create_private_dir(parent, "fleet-backup").map_err(|e| e.to_string())?;
        let staged = staging.join("backup.db");
        let rt = runtime()?;
        let copied = rt.block_on(async {
            let result = fleet.backup(&key, &staged).await;
            fleet.close_all().await;
            result
        });
        let published = copied.map_err(|e| e.to_string()).and_then(|()| {
            std::fs::hard_link(&staged, dest).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    taken()
                } else {
                    format!("could not write {}: {e}", dest.display())
                }
            })
        });
        let _ = std::fs::remove_dir_all(&staging);
        published?;
        eprintln!(
            "\u{2713} Copied {key} to {} ({} byte(s)).",
            dest.display(),
            bytes_on_disk(dest)
        );
        Ok(())
    }

    /// Refuse a directory that other users can write to without the sticky
    /// bit: such a user could rename the staging directory and put their own
    /// file in its place before the copy is written.
    #[cfg(unix)]
    fn refuse_shared_parent(parent: &Path) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(parent)
            .map_err(|e| format!("could not read {}: {e}", parent.display()))?
            .permissions()
            .mode();
        let sticky = mode & 0o1000 != 0;
        if mode & 0o022 != 0 && !sticky {
            return Err(format!(
                "{} is writable by other users.\n  Write the backup to a directory that only \
                 you can write to.",
                parent.display()
            ));
        }
        Ok(())
    }

    fn delete(target: &Target, key: &str, force: bool) -> Result<(), String> {
        let fleet = target.build(None)?;
        let key = parse_key(&fleet, key)?;
        let path = fleet.path_of(&key);
        if !force {
            return Err(format!(
                "Delete stopped: this removes {} and its data ({} profile).\n  Run again \
                 with --force. To keep a copy, run `autumn fleet backup` first.",
                path.display(),
                target.profile
            ));
        }
        let deleted = runtime()?
            .block_on(fleet.delete(&key))
            .map_err(|e| e.to_string())?;
        if !deleted {
            return Err(format!("{key} does not exist at {}.", path.display()));
        }
        eprintln!("\u{2713} Deleted {key} ({}).", path.display());
        if target.config.replication.is_some() {
            eprintln!("  \u{2139} Its replica stays at the replica destination.");
            if target.fleet.restore_missing {
                eprintln!(
                    "  \u{26A0} restore_missing is on: the app restores {key} from its \
                     replica on the next request for it."
                );
            }
        }
        Ok(())
    }

    fn restore(
        target: &Target,
        key: &str,
        timestamp: Option<&str>,
        force: bool,
    ) -> Result<(), String> {
        // Every check that can refuse runs before the fleet is built.
        if !force {
            return Err(format!(
                "Restore stopped: this replaces the database file of {key} with its replica \
                 ({} profile).\n  Run again with --force.",
                target.profile
            ));
        }
        let at = replica::parse_timestamp(timestamp).map_err(|e| e.to_string())?;
        let loaded = replica::Loaded::new(target.profile.clone(), target.config.clone())
            .map_err(|e| e.to_string())?;
        let fleet = target.build(None)?;
        let key = parse_key(&fleet, key)?;
        let destination = loaded.destination().map_err(|e| e.to_string())?;
        let root = loaded.root();
        let prefix = replica_root(&root, &key);
        eprintln!("  \u{2139} source: {}", destination.describe());
        eprintln!("  \u{2139} prefix: {prefix}");

        // A restore uses only `root`. This process does not upload WAL frames.
        let settings = ReplicationSettings {
            database_path: PathBuf::new(),
            root,
            sync_interval: loaded.replication.sync_interval(),
            snapshot_interval: Duration::from_secs(
                loaded.replication.snapshot_interval_secs.max(1),
            ),
            max_wal_bytes: loaded.replication.max_wal_bytes,
            retention: Duration::from_secs(
                loaded
                    .replication
                    .retention_hours
                    .max(1)
                    .saturating_mul(3600),
            ),
            verify_interval: None,
        };
        let replication = Arc::new(FleetReplication::new(
            destination,
            settings,
            loaded.replication.rpo(),
            false,
            Arc::new(autumn_web::time::SystemClock),
        ));
        fleet
            .install_replication(replication)
            .map_err(|_| "the fleet already replicates".to_owned())?;

        let outcome = runtime()?
            .block_on(fleet.restore(&key, at))
            .map_err(|e| match e {
                FleetError::NotFound { .. } => {
                    format!("{key} has no replica under {prefix}.")
                }
                // Windows: another process made the file during the restore.
                FleetError::AlreadyExists { .. } => format!(
                    "Another process created {key} during the restore. Nothing was \
                     replaced. Stop the app, then run again."
                ),
                other => other.to_string(),
            })?;
        eprintln!("  \u{2713} integrity verified.");
        eprintln!(
            "\n\u{2713} Restored {key}: {} byte(s) to {} ({} WAL frame(s) replayed), current \
             as of {}.",
            outcome.bytes,
            outcome.output.display(),
            outcome.frames_replayed,
            outcome.plan.effective.to_rfc3339()
        );
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use autumn_web::fleet_layout::FleetMode;

        fn fleet(mode: FleetMode) -> (tempfile::TempDir, DatabaseFleet) {
            let tmp = tempfile::tempdir().unwrap();
            let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
                mode,
                root: tmp.path().display().to_string(),
                path: None,
                max_open: 1,
                pool_size: 1,
                create_on_demand: None,
                idle_close_secs: 0,
                restore_missing: false,
            })
            .build()
            .unwrap();
            (tmp, fleet)
        }

        #[test]
        fn keys_parse_with_a_colon_or_a_slash() {
            let (_tmp, tenants) = fleet(FleetMode::Tenant);
            let key = parse_key(&tenants, "tenant:acme").unwrap();
            assert_eq!(key.name(), "tenant:acme");
            assert_eq!(parse_key(&tenants, " tenant/acme ").unwrap(), key);

            let (_tmp, slots) = fleet(FleetMode::Slot);
            assert_eq!(parse_key(&slots, "slot/42").unwrap().name(), "slot:00042");
            assert_eq!(
                parse_key(&slots, "slot:00042").unwrap().name(),
                "slot:00042"
            );
        }

        #[test]
        fn a_key_of_the_other_mode_or_a_bad_id_is_refused() {
            let (_tmp, tenants) = fleet(FleetMode::Tenant);
            for raw in [
                "slot:1",
                "Tenant:ACME",
                "tenant:../x",
                "acme",
                "tenant/",
                "tenantx/a",
            ] {
                let err = parse_key(&tenants, raw).unwrap_err();
                assert!(err.contains("not a database name"), "{raw}: {err}");
            }
            let (_tmp, slots) = fleet(FleetMode::Slot);
            assert!(parse_key(&slots, "slot:16384").is_err());
        }

        #[test]
        fn bytes_on_disk_counts_the_wal() {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join("acme.db");
            assert_eq!(bytes_on_disk(&path), 0);
            std::fs::write(&path, [0_u8; 10]).unwrap();
            std::fs::write(tmp.path().join("acme.db-wal"), [0_u8; 5]).unwrap();
            assert_eq!(bytes_on_disk(&path), 15);
        }

        #[test]
        fn migration_columns_follow_the_app_set() {
            let strings = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
            let app = AppView {
                versions: strings(&["1", "2"]),
                known: strings(&["1", "2", "20260907101530"]),
                exact: true,
            };
            // A framework version sorts after the app's: it must not hide
            // that the app migration `2` is pending.
            let applied = strings(&["1", "20260907101530"]);
            assert_eq!(
                migration_columns(&applied, Some(&app)),
                (Some("1".to_owned()), Some(1))
            );
            assert_eq!(
                migration_columns(&applied, None),
                (Some("20260907101530".to_owned()), None)
            );
            assert_eq!(migration_columns(&[], Some(&app)), (None, Some(2)));
            // A version of another set: boot may record an app migration
            // under a substitute, so no pending count.
            let foreign = strings(&["1", "20990101000000"]);
            assert_eq!(
                migration_columns(&foreign, Some(&app)),
                (Some("20990101000000".to_owned()), None)
            );
            let shared = AppView {
                exact: false,
                ..app
            };
            assert_eq!(migration_columns(&applied, Some(&shared)).1, None);
        }
    }
}
