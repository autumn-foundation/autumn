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
    /// Copy a database file into the fleet as a key, for example a backup.
    Import {
        /// Profile overlay for the config.
        profile: Option<String>,
        /// The database name.
        key: String,
        /// The file to copy in.
        source: PathBuf,
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
    /// Rebuild one database from its replica, or from a local file.
    Restore {
        /// Profile overlay for the config.
        profile: Option<String>,
        /// The database name.
        key: String,
        /// Restore to this RFC 3339 instant instead of the latest state.
        timestamp: Option<String>,
        /// Restore from this local file instead of the replica.
        from: Option<PathBuf>,
        /// With `from`: accept a backup of another key.
        allow_rekey: bool,
        /// With `from`: restore a deleted key, and drop its tombstone.
        clear_tombstone: bool,
        /// Confirm the replacement.
        force: bool,
    },
    /// Run one SQL query over the replicas of every tenant (issue #3236).
    Lake {
        /// Profile overlay for the config.
        profile: Option<String>,
        /// The SQL file. `-` reads stdin.
        sql: PathBuf,
        /// Write the CSV here instead of stdout.
        out: Option<PathBuf>,
        /// Also keep the lake as a `SQLite` file here.
        keep: Option<PathBuf>,
        /// Restore each tenant at or before this RFC 3339 instant.
        timestamp: Option<String>,
        /// The number of tenants to restore at the same time.
        concurrency: usize,
        /// Load only these tables. Empty: every table.
        tables: Vec<String>,
        /// The parent of the scratch directory.
        scratch: Option<PathBuf>,
        /// Skip a tenant that fails, and report it.
        skip_failed: bool,
        /// Run with no tombstone rows: markers only.
        markers_only: bool,
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

    #[cfg(test)]
    mod tests {
        #[test]
        fn the_postgres_build_names_the_sqlite_build() {
            let err = super::run(&super::FleetCommand::List {
                profile: None,
                open: false,
                json: false,
            })
            .unwrap_err();
            assert!(err.contains("--features sqlite"), "{err}");
        }
    }
}

#[cfg(feature = "sqlite")]
mod imp {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use std::io::Write as _;
    use std::ops::ControlFlow;

    use autumn_web::config::{AutumnConfig, DatabaseFleetConfig};
    use autumn_web::db::fleet::{DatabaseFleet, FleetError, LocalRestore};
    use autumn_web::db::fleet_lake::{FleetLake, LakeSnapshot, LakeValue, SkipReason};
    use autumn_web::db::fleet_replication::{FleetReplication, replica_root};
    use autumn_web::db::fleet_tombstones::FleetTombstones;
    use autumn_web::fleet_layout::FleetDbKey;
    use autumn_web::migrate::MigrationScope;
    use autumn_web::replication::ReplicationSettings;

    use super::FleetCommand;
    use crate::db::replica;

    /// The scopes whose directories a fleet database gets, as at boot:
    /// `migrations/` and `migrations-shards/`. Never `migrations-control/`.
    const FLEET_SCOPES: [MigrationScope; 2] = [MigrationScope::Everywhere, MigrationScope::Shards];

    /// The migration sets of a fleet database, read from disk: each
    /// directory of [`FLEET_SCOPES`] that exists, in that order. One of them
    /// must exist.
    fn fleet_sources() -> Result<Vec<diesel_migrations::FileBasedMigrations>, String> {
        let mut sources = Vec::new();
        for scope in FLEET_SCOPES {
            let dir = Path::new(scope.directory());
            if !dir.is_dir() {
                continue;
            }
            sources.push(
                diesel_migrations::FileBasedMigrations::from_path(dir)
                    .map_err(|e| format!("could not read {}/: {e}", dir.display()))?,
            );
        }
        if sources.is_empty() {
            return Err(format!(
                "Migrations directory not found: {}/\n  Run this command from the app \
                 directory.",
                MigrationScope::Everywhere.directory()
            ));
        }
        Ok(sources)
    }

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
            FleetCommand::Import {
                profile,
                key,
                source,
            } => import(&Target::load(profile.as_deref())?, key, source),
            FleetCommand::Delete {
                profile,
                key,
                force,
            } => delete(&Target::load(profile.as_deref())?, key, *force),
            FleetCommand::Restore {
                profile,
                key,
                from: Some(from),
                allow_rekey,
                clear_tombstone,
                force,
                ..
            } => restore_from_file(
                &Target::load(profile.as_deref())?,
                key,
                from,
                LocalRestore::new()
                    .allow_rekey(*allow_rekey)
                    .clear_tombstone(*clear_tombstone)
                    .apply_migrations(true),
                *force,
            ),
            FleetCommand::Restore {
                profile,
                key,
                timestamp,
                force,
                ..
            } => restore(
                &Target::load(profile.as_deref())?,
                key,
                timestamp.as_deref(),
                *force,
            ),
            FleetCommand::Lake {
                profile,
                sql,
                out,
                keep,
                timestamp,
                concurrency,
                tables,
                scratch,
                skip_failed,
                markers_only,
            } => lake(
                &Target::load(profile.as_deref())?,
                &LakeArgs {
                    sql,
                    out: out.as_deref(),
                    keep: keep.as_deref(),
                    timestamp: timestamp.as_deref(),
                    concurrency: *concurrency,
                    tables,
                    scratch: scratch.as_deref(),
                    skip_failed: *skip_failed,
                    markers_only: *markers_only,
                },
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

        /// The fleet, with `migrations` and the framework sets when
        /// `migrations` is not empty. An open never migrates.
        fn build(
            &self,
            migrations: Vec<diesel_migrations::FileBasedMigrations>,
        ) -> Result<DatabaseFleet, String> {
            self.require_root()?;
            let mut builder = DatabaseFleet::builder(self.fleet.clone())
                .connect_timeout_secs(self.config.database.connect_timeout_secs)
                .apply_migrations_on_open(false);
            if !migrations.is_empty() {
                for source in migrations {
                    builder = builder.migration_source("app", source);
                }
                // Repeat the `refuse_unknown_versions` check on the file each run
                // migrates: another process can replace it in between.
                builder = builder
                    .framework_migrations()
                    .refuse_unknown_migrations(true);
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
        /// The versions in [`fleet_sources`], as named on disk.
        versions: Vec<String>,
        /// Every version the CLI's sets record, sorted.
        known: Vec<String>,
        /// No two of the CLI's sets share a version, so a version on disk is
        /// the version a database records.
        exact: bool,
    }

    /// The app view, or `None` when [`fleet_sources`] cannot be read.
    fn app_view(target: &Target) -> Option<AppView> {
        let sources = fleet_sources().ok()?;
        let mut versions = Vec::new();
        for source in &sources {
            versions.extend(
                diesel::migration::MigrationSource::<diesel::sqlite::Sqlite>::migrations(source)
                    .ok()?
                    .iter()
                    .map(|m| m.name().version().to_string()),
            );
        }
        let fleet = target.build(sources).ok()?;
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
        let fleet = target.build(Vec::new())?;
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
        let fleet = target.build(fleet_sources()?)?;
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
            "These databases record migrations that are not in {}/, {}/ or the framework \
             sets:\n  {}\n  A plugin set probably applied them. Run the app once with \
             AUTUMN_MIGRATE=1 instead.",
            MigrationScope::Everywhere.directory(),
            MigrationScope::Shards.directory(),
            found.join("\n  ")
        ))
    }

    fn backup(target: &Target, key: &str, dest: &Path) -> Result<(), String> {
        let fleet = target.build(Vec::new())?;
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
        // From here on the checked path is used, with no symlink in it: a
        // symlink on the way could point somewhere else after the check.
        #[cfg(unix)]
        let (parent, dest) = {
            let dir = refuse_unsafe_path(parent)?;
            let name = dest
                .file_name()
                .ok_or_else(|| format!("{} does not name a file.", dest.display()))?;
            let dest = dir.join(name);
            (dir, dest)
        };
        let staging =
            replica::create_private_dir(&parent, "fleet-backup").map_err(|e| e.to_string())?;
        let staged = staging.join("backup.db");
        let rt = runtime()?;
        let copied = rt.block_on(async {
            let result = fleet.backup(&key, &staged).await;
            fleet.close_all().await;
            result
        });
        let published = copied.map_err(|e| e.to_string()).and_then(|()| {
            // A default ACL on `dest`'s directory overrides the umask. `0600`
            // also sets the ACL mask, which stops each inherited entry.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| format!("could not protect the copy: {e}"))?;
            }
            std::fs::hard_link(&staged, &dest).map_err(|e| {
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
            bytes_on_disk(&dest)
        );
        Ok(())
    }

    /// Refuse a path another user could change on the way to `dir`. Every
    /// directory from `/` down to `dir` must belong to root or to this user,
    /// and must not be writable by others unless it has the sticky bit (as
    /// `/tmp` has). Otherwise another user could rename a directory on the way
    /// and put their own file where the copy is written. The rule is
    /// OpenSSH's for its own files. Returns the checked path, symlinks
    /// resolved.
    #[cfg(unix)]
    fn refuse_unsafe_path(dir: &Path) -> Result<PathBuf, String> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let me = nix::unistd::geteuid().as_raw();
        let canonical = std::fs::canonicalize(dir)
            .map_err(|e| format!("could not read {}: {e}", dir.display()))?;
        for ancestor in canonical.ancestors() {
            let meta = std::fs::metadata(ancestor)
                .map_err(|e| format!("could not read {}: {e}", ancestor.display()))?;
            let mode = meta.permissions().mode();
            let owned = meta.uid() == 0 || meta.uid() == me;
            let shared = mode & 0o022 != 0 && mode & 0o1000 == 0;
            if !owned || shared {
                return Err(format!(
                    "{} is writable by other users, or belongs to another user.\n  Keep \
                     backups under directories that only you (or root) can change.",
                    ancestor.display()
                ));
            }
        }
        Ok(canonical)
    }

    fn import(target: &Target, key: &str, source: &Path) -> Result<(), String> {
        // The copy must carry the epoch of the host that owns the slot. This
        // process holds no lease, so the app's next open could replace the
        // copy from the replica.
        if target.fleet.ownership.is_some() {
            return Err(
                "Import stopped: [database.fleet.ownership] is on, and this command holds no \
                 slot lease.\n  Call `DatabaseFleet::import` in the app that owns the slot."
                    .to_owned(),
            );
        }
        if !source.is_file() {
            return Err(format!("{} is not a file.", source.display()));
        }
        target.require_root()?;
        let fleet = target.build(fleet_sources()?)?;
        let remapped = fleet.remapped_migrations();
        if !remapped.is_empty() {
            return Err(format!(
                "These migrations share a version with another migration: {}.\n  \
                 `autumn fleet import` cannot record them as boot does. Use \
                 `DatabaseFleet::import` in the app instead.",
                remapped.join(", ")
            ));
        }
        let key = parse_key(&fleet, key)?;
        let path = fleet.path_of(&key);
        // With replication on, the import purges a deleted key's tombstone
        // and old replica first, so the retry task cannot purge the new one.
        if target.replicated() {
            let purge = |error: String| {
                format!(
                    "Import stopped: with [replication] on, an import first purges the \
                     replica of a deleted database, and it cannot reach it.\n  {error}"
                )
            };
            let loaded = validated_replication(target).map_err(purge)?;
            let replication = replication_for(target, &loaded).map_err(purge)?;
            fleet
                .install_replication(replication)
                .map_err(|_| "the fleet already replicates".to_owned())?;
        }
        // Publish only: an open would start a replicator in this process.
        let rt = runtime()?;
        let imported = rt.block_on(async {
            let result = fleet.import_file(&key, source).await;
            fleet.close_all().await;
            result
        });
        imported.map_err(|e| match e {
            FleetError::AlreadyExists { .. } => format!(
                "{key} already has a database at {}.\n  An import never replaces a file. \
                 Run `autumn fleet delete {key} --force` first.",
                path.display()
            ),
            FleetError::Migration { .. } => format!(
                "{e}\n  If the copy records a plugin migration, call \
                 `DatabaseFleet::import` in the app instead."
            ),
            other => format!("{other}\n  Nothing was imported."),
        })?;
        eprintln!(
            "\u{2713} Imported {} as {key} ({}).",
            source.display(),
            path.display()
        );
        Ok(())
    }

    fn delete(target: &Target, key: &str, force: bool) -> Result<(), String> {
        let fleet = target.build(Vec::new())?;
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
        // With replication on, the replica goes too (issue #3235).
        let replication = if target.replicated() {
            let purge = |error: String| {
                format!(
                    "Delete stopped: with [replication] on, a delete also removes the replica, \
                     and it cannot reach it.\n  {error}"
                )
            };
            let loaded = validated_replication(target).map_err(purge)?;
            let replication = replication_for(target, &loaded).map_err(purge)?;
            fleet
                .install_replication(Arc::clone(&replication))
                .map_err(|_| "the fleet already replicates".to_owned())?;
            Some(replication)
        } else {
            None
        };
        let rt = runtime()?;
        let deleted = rt.block_on(fleet.delete(&key)).map_err(|e| e.to_string())?;
        if !deleted {
            return Err(format!("{key} does not exist at {}.", path.display()));
        }
        eprintln!("\u{2713} Deleted {key} ({}).", path.display());
        let Some(tombstones) = replication.as_ref().and_then(|r| r.tombstones()) else {
            return Ok(());
        };
        let kept = |row: &autumn_web::db::fleet_tombstones::Tombstone| {
            row.confirmed && row.attempts == 0 && row.purge_after > tombstones.now()
        };
        match rt.block_on(tombstones.get(&key.name())) {
            Ok(None) => eprintln!("  \u{2713} Its replica is purged."),
            Ok(Some(row)) if kept(&row) => eprintln!(
                "  \u{2139} Its replica is kept until {} \
                 (database.fleet.replica_retention_after_delete). Until that time, `autumn \
                 fleet restore {key} --force` brings it back. After that time, the app \
                 purges it.",
                row.purge_after.to_rfc3339()
            ),
            Ok(Some(row)) => eprintln!(
                "  \u{26A0} Its replica is not purged yet: {}.\n    A tombstone records it: \
                 restore_missing does not bring it back, and the app tries the purge again.",
                row.last_error
                    .as_deref()
                    .unwrap_or("the purge did not finish")
            ),
            Err(error) => eprintln!("  \u{26A0} Could not read the replica tombstone: {error}."),
        }
        Ok(())
    }

    /// `[replication]` as `delete` and `restore` read it. It must name
    /// exactly one destination, whether replication is on or not.
    fn validated_replication(target: &Target) -> Result<replica::Loaded, String> {
        let loaded = replica::Loaded::new(target.profile.clone(), target.config.clone())
            .map_err(|e| e.to_string())?;
        // The check boot makes, and one more: a restore must not guess
        // between two destinations, whether replication is on or not.
        let mut errors = loaded.replication.validation_errors();
        if loaded.replication.s3.is_some()
            && loaded
                .replication
                .path
                .as_deref()
                .is_some_and(|p| !p.trim().is_empty())
            && errors.is_empty()
        {
            errors.push(
                "[replication] configures both `s3` and `path`; pick exactly one destination"
                    .to_owned(),
            );
        }
        if !errors.is_empty() {
            return Err(format!(
                "Invalid [replication] configuration: {}",
                errors.join("; ")
            ));
        }
        Ok(loaded)
    }

    /// The fleet replication of `delete` and `restore`: the `[replication]`
    /// destination, and the tombstones on the control database (issue
    /// #3235). This process does not upload WAL frames.
    fn replication_for(
        target: &Target,
        loaded: &replica::Loaded,
    ) -> Result<Arc<FleetReplication>, String> {
        let destination = loaded.destination().map_err(|e| e.to_string())?;
        let settings = ReplicationSettings {
            database_path: PathBuf::new(),
            root: loaded.root(),
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
        let clock: Arc<dyn autumn_web::time::ClockSource> = Arc::new(autumn_web::time::SystemClock);
        let control = autumn_web::db::create_pool(&target.config.database)
            .map_err(|e| format!("could not open the control database: {e}"))?
            .ok_or_else(|| "no control database is configured ([database] url)".to_owned())?;
        Ok(Arc::new(
            FleetReplication::new(
                destination,
                settings,
                loaded.replication.rpo(),
                false,
                Arc::clone(&clock),
            )
            .with_tombstones(Arc::new(FleetTombstones::new(control, clock)))
            .with_retention_after_delete(target.fleet.replica_retention_after_delete),
        ))
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
        let loaded = validated_replication(target)?;
        let fleet = target.build(Vec::new())?;
        let key = parse_key(&fleet, key)?;
        let prefix = replica_root(&loaded.root(), &key);
        // The restore also drops the tombstone of a deleted database.
        let replication = replication_for(target, &loaded)?;
        eprintln!(
            "  \u{2139} source: {}",
            loaded.destination().map_err(|e| e.to_string())?.describe()
        );
        eprintln!("  \u{2139} prefix: {prefix}");
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

    /// `fleet restore --from`: replace the database of `key` with a local
    /// file, as `DatabaseFleet::restore_from_file` does. With `[replication]`
    /// on, it also ships a new base snapshot of the restored file.
    fn restore_from_file(
        target: &Target,
        key: &str,
        source: &Path,
        opts: LocalRestore,
        force: bool,
    ) -> Result<(), String> {
        if !force {
            return Err(format!(
                "Restore stopped: this replaces the database file of {key} with {} ({} \
                 profile).\n  Run again with --force.",
                source.display(),
                target.profile
            ));
        }
        // The copy must carry the epoch of the host that owns the slot.
        if target.fleet.ownership.is_some() {
            return Err(
                "Restore stopped: [database.fleet.ownership] is on, and this command holds no \
                 slot lease.\n  Call `DatabaseFleet::restore_from_file` in the app that owns \
                 the slot."
                    .to_owned(),
            );
        }
        let source = checked_source(source)?;
        target.require_root()?;
        let fleet = target.build(fleet_sources()?)?;
        let remapped = fleet.remapped_migrations();
        if !remapped.is_empty() {
            return Err(format!(
                "These migrations share a version with another migration: {}.\n  \
                 `autumn fleet restore --from` cannot record them as boot does. Use \
                 `DatabaseFleet::restore_from_file` in the app instead.",
                remapped.join(", ")
            ));
        }
        let key = parse_key(&fleet, key)?;
        // With replication on, the restore ships a new base snapshot, and
        // reads the tombstone of a deleted key.
        if target.replicated() {
            let needs = |error: String| {
                format!(
                    "Restore stopped: with [replication] on, a restore ships the restored \
                     database and reads its tombstone, and it cannot reach them.\n  {error}"
                )
            };
            let loaded = validated_replication(target).map_err(needs)?;
            let replication = replication_for(target, &loaded).map_err(needs)?;
            fleet
                .install_replication(replication)
                .map_err(|_| "the fleet already replicates".to_owned())?;
        }
        let restored = runtime()?.block_on(async {
            let result = fleet.restore_from_file(&key, &source, opts).await;
            // Ships the last frames of the restored database.
            fleet.close_all().await;
            result
        });
        let outcome = restored.map_err(|e| match &e {
            FleetError::Restore { detail, .. } if detail.contains("is a copy of") => format!(
                "{e}\n  Nothing was restored. To restore it as {key}, run again with \
                 --allow-rekey."
            ),
            FleetError::Restore { detail, .. } if detail.contains("clear_tombstone") => format!(
                "{key} was deleted, and its tombstone keeps the replica for undo.\n  Nothing \
                 was restored. Run again with --clear-tombstone: this purges the old replica."
            ),
            FleetError::Migration { .. } => format!(
                "{e}\n  Nothing was restored. If the file records a plugin migration, call \
                 `DatabaseFleet::restore_from_file` in the app instead."
            ),
            _ => format!("{e}\n  Nothing was restored."),
        })?;
        eprintln!(
            "\u{2713} Restored {key} from {} ({}).",
            source.display(),
            if outcome.replaced {
                "replaced the old file"
            } else {
                "created the file"
            }
        );
        if let Some(found) = &outcome.rekeyed_from {
            eprintln!("  \u{2139} The file was a copy of {found}; it now records {key}.");
        }
        if outcome.migrations_applied > 0 {
            eprintln!(
                "  \u{2139} {} migration(s) applied.",
                outcome.migrations_applied
            );
        }
        if outcome.tombstone_cleared {
            eprintln!("  \u{2139} The tombstone and the old replica are gone.");
        }
        if let Some(shipped) = &outcome.shipped {
            eprintln!(
                "  \u{2713} New base snapshot shipped: {} object(s), {} byte(s).",
                shipped.shipped, shipped.bytes
            );
        }
        if let Some(error) = &outcome.ship_error {
            eprintln!(
                "  \u{26A0} The new base snapshot did not ship: {error}.\n    The restored file \
                 is in place. A replica restore would bring back older data. If the app \
                 serves {key}, it ships a new base when it opens it. Otherwise run this \
                 restore again."
            );
        }
        Ok(())
    }

    /// The `--from` file, checked. It must be a regular file, not a symlink.
    /// On Unix it must belong to root or to this user, and every directory on
    /// the way must pass [`refuse_unsafe_path`]: another user must not swap
    /// the file under the restore. Returns the checked path.
    fn checked_source(source: &Path) -> Result<PathBuf, String> {
        let not_a_file = || {
            format!(
                "Restore stopped: {} is not a file.\n  Nothing was restored.",
                source.display()
            )
        };
        let meta = source.symlink_metadata().map_err(|_| not_a_file())?;
        if !meta.file_type().is_file() {
            return Err(not_a_file());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let me = nix::unistd::geteuid().as_raw();
            if meta.uid() != 0 && meta.uid() != me {
                return Err(format!(
                    "Restore stopped: {} belongs to another user.\n  Restore only a file \
                     that you (or root) own.",
                    source.display()
                ));
            }
            let parent = source
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let dir = refuse_unsafe_path(parent)?;
            let name = source.file_name().ok_or_else(not_a_file)?;
            Ok(dir.join(name))
        }
        #[cfg(not(unix))]
        {
            Ok(source.to_path_buf())
        }
    }

    /// The arguments of `fleet lake`.
    struct LakeArgs<'a> {
        sql: &'a Path,
        out: Option<&'a Path>,
        keep: Option<&'a Path>,
        timestamp: Option<&'a str>,
        concurrency: usize,
        tables: &'a [String],
        scratch: Option<&'a Path>,
        skip_failed: bool,
        markers_only: bool,
    }

    /// `fleet lake`: restore the live tenants from `[replication]` into a
    /// scratch lake, run one query, and write CSV. It never builds the fleet,
    /// so the fleet root need not exist.
    fn lake(target: &Target, args: &LakeArgs<'_>) -> Result<(), String> {
        let sql = if args.sql == Path::new("-") {
            std::io::read_to_string(std::io::stdin())
                .map_err(|e| format!("could not read the SQL from stdin: {e}"))?
        } else {
            std::fs::read_to_string(args.sql)
                .map_err(|e| format!("could not read {}: {e}", args.sql.display()))?
        };
        let at = replica::parse_timestamp(args.timestamp).map_err(|e| e.to_string())?;
        let loaded = validated_replication(target)?;
        let destination = loaded.destination().map_err(|e| e.to_string())?;
        eprintln!("  \u{2139} source: {}", destination.describe());
        eprintln!("  \u{2139} prefix: {}/fleet/tenant", loaded.root());

        // Cheap checks first: a restore of every tenant comes after them.
        if let Some(keep) = args.keep {
            refuse_keep(keep, &target.fleet.root)?;
        }
        let out = args
            .out
            .map(|out| create_private(out).map(|file| (out, file)));
        let out = out.transpose()?;
        let result = run_lake(target, args, &sql, at, destination, loaded.root(), out);
        if result.is_err()
            && let Some(out) = args.out
        {
            let _ = std::fs::remove_file(out);
        }
        result
    }

    /// The rest of [`lake`], after the outputs are checked.
    fn run_lake(
        target: &Target,
        args: &LakeArgs<'_>,
        sql: &str,
        at: Option<chrono::DateTime<chrono::Utc>>,
        destination: Arc<dyn autumn_web::replication::ReplicaDestination>,
        base: String,
        out: Option<(&Path, std::fs::File)>,
    ) -> Result<(), String> {
        let mut lake = FleetLake::new(destination, base)
            .outside_root(&target.fleet.root)
            .with_workers(args.concurrency);
        if args.markers_only {
            eprintln!("  \u{26a0} --markers-only: deleted tenants come from the markers only");
            lake = lake.markers_only();
        } else {
            let tombstones = control_tombstones(target)?.ok_or_else(|| {
                "This host has no control database, so the lake cannot read the tombstone \
                 rows of deleted tenants.\n  Run where the control database is, or pass \
                 --markers-only to use the markers in object storage only."
                    .to_owned()
            })?;
            lake = lake.with_tombstones(tombstones);
        }
        if args.skip_failed {
            lake = lake.skip_failed_tenants();
        }
        if let Some(at) = at {
            lake = lake.as_of(at);
        }
        if !args.tables.is_empty() {
            lake = lake.with_tables(args.tables.iter().cloned());
        }
        if let Some(scratch) = args.scratch {
            lake = lake.with_scratch_dir(scratch);
        }

        let snapshot = runtime()?
            .block_on(lake.materialize())
            .map_err(|e| e.to_string())?;
        report_load(&snapshot);
        if let Some(keep) = args.keep {
            snapshot.persist(keep).map_err(|e| e.to_string())?;
            eprintln!("  \u{2139} lake kept at {}", keep.display());
        }
        let rows = match out {
            Some((path, file)) => {
                let mut writer = std::io::BufWriter::new(file);
                let rows = write_csv(&snapshot, sql, &mut writer)?;
                writer
                    .flush()
                    .map_err(|e| format!("could not write {}: {e}", path.display()))?;
                rows
            }
            None => {
                let stdout = std::io::stdout();
                let mut writer = std::io::BufWriter::new(stdout.lock());
                let rows = write_csv(&snapshot, sql, &mut writer)?;
                writer
                    .flush()
                    .map_err(|e| format!("could not write stdout: {e}"))?;
                rows
            }
        };
        eprintln!("\n\u{2713} {rows} row(s).");
        Ok(())
    }

    /// Refuse a `--keep` path that exists or is under the fleet root.
    fn refuse_keep(keep: &Path, root: &str) -> Result<(), String> {
        if keep.exists() {
            return Err(format!("--keep {} already exists.", keep.display()));
        }
        let parent = match keep.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let parent = parent
            .canonicalize()
            .map_err(|e| format!("--keep {}: {e}", keep.display()))?;
        if let Ok(root) = Path::new(root).canonicalize()
            && parent.starts_with(&root)
        {
            return Err(format!(
                "--keep {} is under the fleet root {}.",
                keep.display(),
                root.display()
            ));
        }
        Ok(())
    }

    /// The tombstone rows of the control database, when it is on this host.
    /// A missing file is not created.
    fn control_tombstones(target: &Target) -> Result<Option<Arc<FleetTombstones>>, String> {
        let Some(url) = target.config.database.effective_primary_url() else {
            return Ok(None);
        };
        if !autumn_web::replication::database_file(url).is_some_and(|file| file.is_file()) {
            return Ok(None);
        }
        let control = autumn_web::db::create_pool(&target.config.database)
            .map_err(|e| format!("could not open the control database: {e}"))?
            .ok_or_else(|| "no control database is configured ([database] url)".to_owned())?;
        let clock: Arc<dyn autumn_web::time::ClockSource> = Arc::new(autumn_web::time::SystemClock);
        Ok(Some(Arc::new(FleetTombstones::new(control, clock))))
    }

    /// Create `path` for writing, owner-only on unix. Never replace a file.
    fn create_private(path: &Path) -> Result<std::fs::File, String> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        options
            .open(path)
            .map_err(|e| format!("could not create {}: {e}", path.display()))
    }

    /// Write the result of `sql` as CSV. Returns the number of rows.
    fn write_csv(
        snapshot: &LakeSnapshot,
        sql: &str,
        writer: &mut impl std::io::Write,
    ) -> Result<usize, String> {
        let mut failed: Option<std::io::Error> = None;
        let mut header = false;
        let rows = snapshot
            .for_each_row(sql, |columns, row| {
                let mut line = String::new();
                if !header {
                    header = true;
                    line = csv_line(columns.iter().map(|c| csv_field(c)));
                }
                line.push_str(&csv_line(row.iter().map(csv_value)));
                match writer.write_all(line.as_bytes()) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(e) => {
                        failed = Some(e);
                        ControlFlow::Break(())
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        if let Some(e) = failed {
            return Err(format!("could not write the CSV: {e}"));
        }
        if !header {
            let columns = snapshot.columns(sql).map_err(|e| e.to_string())?;
            writer
                .write_all(csv_line(columns.iter().map(|c| csv_field(c))).as_bytes())
                .map_err(|e| format!("could not write the CSV: {e}"))?;
        }
        Ok(rows)
    }

    fn csv_line(fields: impl Iterator<Item = String>) -> String {
        let mut line = fields.collect::<Vec<_>>().join(",");
        line.push('\n');
        line
    }

    /// One CSV field (RFC 4180): quoted when it holds a comma, a quote or a
    /// line break.
    fn csv_field(text: &str) -> String {
        if text.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", text.replace('"', "\"\""))
        } else {
            text.to_owned()
        }
    }

    /// NULL is empty, a blob is lowercase hex.
    fn csv_value(value: &LakeValue) -> String {
        match value {
            LakeValue::Null => String::new(),
            LakeValue::Integer(n) => n.to_string(),
            LakeValue::Real(r) => r.to_string(),
            LakeValue::Text(text) => csv_field(text),
            LakeValue::Blob(bytes) => {
                use std::fmt::Write as _;
                bytes.iter().fold(String::new(), |mut hex, byte| {
                    let _ = write!(hex, "{byte:02x}");
                    hex
                })
            }
        }
    }

    /// The load summary on stderr: what was loaded and what was skipped.
    fn report_load(snapshot: &LakeSnapshot) {
        let count = |want: fn(&SkipReason) -> bool| {
            snapshot
                .skipped()
                .iter()
                .filter(|skip| want(&skip.reason))
                .count()
        };
        for skip in snapshot.skipped() {
            if let SkipReason::Failed(detail) = &skip.reason {
                eprintln!("  \u{2717} {}: {detail}", skip.name);
            }
        }
        let tenants = snapshot.tenants();
        if let (Some(oldest), Some(newest)) = (
            tenants.iter().map(|t| t.effective).min(),
            tenants.iter().map(|t| t.effective).max(),
        ) {
            eprintln!(
                "  \u{2139} each tenant is current as of its own instant: oldest {}, newest {}",
                oldest.to_rfc3339(),
                newest.to_rfc3339()
            );
        }
        eprintln!(
            "  \u{2139} {} tenant(s) loaded, {} deleted, {} unreplicated, {} absent at the \
             timestamp, {} failed, {} invalid name(s), {} slot database(s).",
            tenants.len(),
            count(|r| *r == SkipReason::Deleted),
            count(|r| *r == SkipReason::Unreplicated),
            count(|r| *r == SkipReason::AbsentAtTarget),
            count(|r| matches!(r, SkipReason::Failed(_))),
            count(|r| *r == SkipReason::InvalidName),
            count(|r| *r == SkipReason::SlotDatabase),
        );
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use autumn_web::fleet_layout::FleetMode;

        #[test]
        fn lake_csv_quotes_and_encodes_values() {
            assert_eq!(csv_field("plain"), "plain");
            assert_eq!(csv_field("a,b"), "\"a,b\"");
            assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
            assert_eq!(csv_field("two\nlines"), "\"two\nlines\"");
            assert_eq!(csv_value(&LakeValue::Null), "");
            assert_eq!(csv_value(&LakeValue::Integer(-3)), "-3");
            assert_eq!(
                csv_value(&LakeValue::Real(0.1 + 0.2)),
                "0.30000000000000004"
            );
            assert_eq!(csv_value(&LakeValue::Text("x,y".into())), "\"x,y\"");
            assert_eq!(csv_value(&LakeValue::Blob(vec![0, 0xab])), "00ab");
            assert_eq!(
                csv_line(["a".to_owned(), String::new()].into_iter()),
                "a,\n"
            );
        }

        fn fleet(mode: FleetMode) -> (tempfile::TempDir, DatabaseFleet) {
            let tmp = tempfile::tempdir().unwrap();
            let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
                mode,
                root: tmp.path().display().to_string(),
                path: None,
                max_open: 1,
                max_resident_bytes: 0,
                pool_size: 1,
                create_on_demand: None,
                idle_close_secs: 0,
                restore_missing: false,
                ownership: None,
                replica_retention_after_delete: std::time::Duration::ZERO,
                restore_check_sample: 0,
                replication_workers: 4,
                min_residency: std::time::Duration::from_secs(2),
                max_open_policy: autumn_web::config::FleetMaxOpenPolicy::Soft,
                mmap_size: 0,
                cache_size_kib: 0,
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
