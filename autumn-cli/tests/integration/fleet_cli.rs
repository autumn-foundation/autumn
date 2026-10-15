//! `autumn fleet` (issue #3232) through the real binary, against a temp fleet
//! root.
//!
//! The `SQLite` build runs every subcommand. The default (Postgres) build only
//! checks that the command tells the operator which build to use.
//!
//!   `cargo test -p autumn-cli --no-default-features --features sqlite --test cli_tests fleet_cli`

use std::path::Path;
use std::process::Command;

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

fn run_autumn(dir: &Path, args: &[&str]) -> (String, String, Option<i32>) {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        .env("AUTUMN_ENV", "dev")
        .env("AUTUMN_MANIFEST_DIR", dir)
        .output()
        .expect("failed to run autumn");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

#[cfg(not(feature = "sqlite"))]
#[test]
fn fleet_in_the_postgres_build_names_the_sqlite_build() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, stderr, code) = run_autumn(tmp.path(), &["fleet", "list"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("--features sqlite"), "{stderr}");
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use autumn_web::config::DatabaseFleetConfig;
    use autumn_web::db::fleet::DatabaseFleet;
    use autumn_web::db::fleet_replication::FleetReplication;
    use autumn_web::fleet_layout::FleetMode;
    use autumn_web::replication::{FileDestination, ReplicaDestination, ReplicationSettings};
    use diesel::connection::SimpleConnection as _;
    use diesel::{Connection as _, QueryableByName, RunQueryDsl as _, SqliteConnection};

    use super::run_autumn;

    fn run_ok(dir: &Path, args: &[&str]) -> (String, String) {
        let (stdout, stderr, code) = run_autumn(dir, args);
        assert_eq!(
            code,
            Some(0),
            "autumn {args:?} failed\nstdout: {stdout}\nstderr: {stderr}"
        );
        (stdout, stderr)
    }

    fn run_err(dir: &Path, args: &[&str]) -> String {
        let (stdout, stderr, code) = run_autumn(dir, args);
        assert_eq!(
            code,
            Some(1),
            "autumn {args:?} must fail\nstdout: {stdout}\nstderr: {stderr}"
        );
        stderr
    }

    /// A project directory: `autumn.toml`, a `migrations/` directory with one
    /// app migration, and a tenant fleet under `fleet/` (relative, as an app
    /// usually writes it).
    struct App {
        _tmp: tempfile::TempDir,
        dir: PathBuf,
    }

    impl App {
        fn new(extra_toml: &str) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("app");
            let migration = dir.join("migrations/20260101000000_create_notes");
            std::fs::create_dir_all(&migration).unwrap();
            std::fs::write(
                migration.join("up.sql"),
                "CREATE TABLE notes (body TEXT NOT NULL);",
            )
            .unwrap();
            std::fs::write(migration.join("down.sql"), "DROP TABLE notes;").unwrap();
            std::fs::write(
                dir.join("autumn.toml"),
                format!(
                    "[database]\nurl = \"sqlite://{}\"\n\n[database.fleet]\nmode = \"tenant\"\n\
                     root = \"fleet\"\n\n{extra_toml}",
                    tmp.path().join("control.db").display()
                ),
            )
            .unwrap();
            Self { _tmp: tmp, dir }
        }

        fn root(&self) -> PathBuf {
            self.dir.join("fleet")
        }

        fn config(&self) -> DatabaseFleetConfig {
            DatabaseFleetConfig {
                mode: FleetMode::Tenant,
                root: self.root().display().to_string(),
                path: None,
                max_open: 8,
                pool_size: 2,
                create_on_demand: None,
                idle_close_secs: 0,
                restore_missing: false,
            }
        }

        /// A fleet as the app builds it: its migrations and the framework sets.
        fn fleet(&self) -> DatabaseFleet {
            let source =
                diesel_migrations::FileBasedMigrations::from_path(self.dir.join("migrations"))
                    .unwrap();
            DatabaseFleet::builder(self.config())
                .migration_source("app", source)
                .framework_migrations()
                .build()
                .unwrap()
        }

        /// A fleet with no migrations: databases that `fleet migrate` must
        /// bring up to date.
        fn bare_fleet(&self) -> DatabaseFleet {
            DatabaseFleet::builder(self.config()).build().unwrap()
        }

        fn path(&self, tenant: &str) -> PathBuf {
            let fleet = self.bare_fleet();
            fleet.path_of(&fleet.key_for(tenant).unwrap())
        }

        fn provision(&self, fleet: &DatabaseFleet, tenants: &[&str], rows: usize) {
            runtime().block_on(async {
                for tenant in tenants {
                    let key = fleet.key_for(tenant).unwrap();
                    fleet.provision(&key).await.unwrap();
                }
                fleet.close_all().await;
            });
            for tenant in tenants {
                for n in 0..rows {
                    exec(
                        &self.path(tenant),
                        &format!("INSERT INTO notes VALUES ('{n}')"),
                    );
                }
            }
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn connect(path: &Path) -> SqliteConnection {
        SqliteConnection::establish(path.to_str().unwrap()).unwrap()
    }

    fn exec(path: &Path, sql: &str) {
        connect(path).batch_execute(sql).unwrap();
    }

    fn count_notes(path: &Path) -> i64 {
        #[derive(QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            n: i64,
        }
        diesel::sql_query("SELECT COUNT(*) AS n FROM notes")
            .get_result::<Count>(&mut connect(path))
            .unwrap()
            .n
    }

    fn has_table(path: &Path, table: &str) -> bool {
        connect(path)
            .batch_execute(&format!("SELECT 1 FROM {table}"))
            .is_ok()
    }

    #[test]
    fn fleet_list_reports_every_database() {
        let app = App::new("");
        app.provision(&app.fleet(), &["acme", "globex"], 1);

        let (stdout, _) = run_ok(&app.dir, &["fleet", "list"]);
        assert!(stdout.contains("tenant:acme"), "{stdout}");
        assert!(stdout.contains("tenant:globex"), "{stdout}");

        let (stdout, _) = run_ok(&app.dir, &["fleet", "list", "--open", "--json"]);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect(&stdout);
        assert_eq!(rows.len(), 2, "{stdout}");
        let acme = &rows[0];
        assert_eq!(acme["key"], "tenant:acme");
        assert_eq!(acme["identity"], "tenant:acme");
        assert!(acme["bytes"].as_u64().unwrap() > 0, "{stdout}");
        assert_eq!(acme["migration_version"], "20260101000000", "{stdout}");
        assert_eq!(acme["pending"], 0, "{stdout}");
        assert!(
            Path::new(acme["path"].as_str().unwrap()).ends_with("acme.db"),
            "{stdout}"
        );

        // Without --open, nothing is read from the files.
        let (stdout, _) = run_ok(&app.dir, &["fleet", "list", "--json"]);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect(&stdout);
        assert!(rows[0].get("identity").is_none(), "{stdout}");

        // A file that is not a database: its row has the error, and the
        // command exits non-zero after it prints every row.
        let junk = app.path("initech");
        std::fs::create_dir_all(junk.parent().unwrap()).unwrap();
        std::fs::write(&junk, b"not a database, not at all, no").unwrap();
        let (stdout, stderr, code) = run_autumn(&app.dir, &["fleet", "list", "--open"]);
        assert_eq!(code, Some(1), "{stderr}");
        assert!(stdout.contains("tenant:acme"), "{stdout}");
        assert!(stdout.contains("tenant:initech"), "{stdout}");
        assert!(stderr.contains("1 of 3"), "{stderr}");
    }

    #[test]
    fn fleet_refuses_a_missing_root_and_creates_nothing() {
        let app = App::new("[replication]\nenabled = true\npath = \"replicas\"\n");
        let dest = app.dir.join("copy.db");
        for args in [
            vec!["fleet", "list"],
            vec!["fleet", "migrate"],
            vec!["fleet", "backup", "tenant:acme", dest.to_str().unwrap()],
            vec!["fleet", "delete", "tenant:acme", "--force"],
            vec!["fleet", "restore", "tenant:acme", "--force"],
        ] {
            let stderr = run_err(&app.dir, &args);
            assert!(stderr.contains("does not exist"), "{args:?}: {stderr}");
            assert!(!app.root().exists(), "{args:?} created the fleet root");
        }
    }

    #[test]
    fn fleet_resolves_the_profile_overlay() {
        let app = App::new("[profile.staging.database.fleet]\nroot = \"staging-fleet\"\n");
        std::fs::create_dir_all(app.dir.join("staging-fleet")).unwrap();
        let output = std::process::Command::new(super::autumn_bin())
            .args(["fleet", "list", "--profile", "staging"])
            .current_dir(&app.dir)
            .env_remove("AUTUMN_ENV")
            .env_remove("AUTUMN_PROFILE")
            .env("AUTUMN_MANIFEST_DIR", &app.dir)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(stderr.contains("staging-fleet"), "{stderr}");
    }

    #[test]
    fn fleet_needs_a_fleet_section() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            "[database]\nurl = \"sqlite://control.db\"\n",
        )
        .unwrap();
        let stderr = run_err(tmp.path(), &["fleet", "list"]);
        assert!(stderr.contains("[database.fleet]"), "{stderr}");
    }

    #[test]
    fn fleet_migrate_applies_pending_migrations_to_every_database() {
        let app = App::new("");
        let bare = app.bare_fleet();
        runtime().block_on(async {
            for tenant in ["acme", "globex", "initech"] {
                bare.provision(&bare.key_for(tenant).unwrap())
                    .await
                    .unwrap();
            }
            bare.close_all().await;
        });

        // One key first.
        let (_, stderr) = run_ok(&app.dir, &["fleet", "migrate", "--key", "tenant:acme"]);
        assert!(stderr.contains("tenant:acme"), "{stderr}");
        assert!(has_table(&app.path("acme"), "notes"));
        assert!(!has_table(&app.path("globex"), "notes"));

        // Then the rest, two at a time.
        run_ok(&app.dir, &["fleet", "migrate", "--concurrency", "2"]);
        for tenant in ["acme", "globex", "initech"] {
            let path = app.path(tenant);
            assert!(has_table(&path, "notes"), "{tenant}");
            assert!(has_table(&path, "_autumn_version_history"), "{tenant}");
            assert!(
                !has_table(&path, "api_tokens"),
                "{tenant}: control-plane tables stay on the control database"
            );
        }

        // Boot finds nothing left to apply.
        runtime().block_on(async {
            let boot = DatabaseFleet::builder(app.config())
                .migration_source(
                    "app",
                    diesel_migrations::FileBasedMigrations::from_path(app.dir.join("migrations"))
                        .unwrap(),
                )
                .framework_migrations()
                .apply_migrations_on_open(false)
                .build()
                .unwrap();
            boot.open(&boot.key_for("globex").unwrap()).await.unwrap();
            boot.close_all().await;
        });
    }

    #[test]
    fn fleet_migrate_reports_each_failure_and_exits_non_zero() {
        let app = App::new("");
        app.provision(&app.bare_fleet(), &["acme"], 0);
        // A file copied by hand onto another key's path.
        let copy = app.path("globex");
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::copy(app.path("acme"), &copy).unwrap();

        let stderr = run_err(&app.dir, &["fleet", "migrate"]);
        assert!(
            stderr.contains("tenant:globex identifies itself as tenant:acme"),
            "{stderr}"
        );
        assert!(stderr.contains("1 of 2"), "{stderr}");
        assert!(has_table(&app.path("acme"), "notes"), "the others migrate");
    }

    #[test]
    fn fleet_migrate_refuses_a_version_boot_could_record_differently() {
        let app = App::new("");
        app.provision(&app.bare_fleet(), &["acme"], 0);
        // The version of the framework's derivations migration.
        let shared = app.dir.join("migrations/20260907101530_create_tags");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("up.sql"), "CREATE TABLE tags (id INTEGER);").unwrap();
        std::fs::write(shared.join("down.sql"), "DROP TABLE tags;").unwrap();

        let stderr = run_err(&app.dir, &["fleet", "migrate"]);
        assert!(stderr.contains("20260907101530_create_tags"), "{stderr}");
        assert!(stderr.contains("AUTUMN_MIGRATE=1"), "{stderr}");
        assert!(
            !has_table(&app.path("acme"), "notes"),
            "nothing was applied"
        );
    }

    #[test]
    fn fleet_migrate_refuses_a_database_another_set_migrated() {
        let app = App::new("");
        app.provision(&app.fleet(), &["acme", "globex"], 0);
        // A plugin set recorded this version at boot.
        exec(
            &app.path("globex"),
            "INSERT INTO __diesel_schema_migrations (version) VALUES ('20990101000000')",
        );

        let stderr = run_err(&app.dir, &["fleet", "migrate"]);
        assert!(stderr.contains("tenant:globex: 20990101000000"), "{stderr}");
        assert!(stderr.contains("AUTUMN_MIGRATE=1"), "{stderr}");
        // Only the clean database, by key.
        run_ok(&app.dir, &["fleet", "migrate", "--key", "tenant:acme"]);
    }

    #[test]
    fn fleet_refuses_a_control_database_inside_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fleet");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            tmp.path().join("autumn.toml"),
            format!(
                "[database]\nurl = \"sqlite://{}\"\n\n[database.fleet]\nmode = \"tenant\"\n\
                 root = \"fleet\"\n",
                root.join("control.db").display()
            ),
        )
        .unwrap();
        let stderr = run_err(tmp.path(), &["fleet", "list"]);
        assert!(stderr.contains("control database"), "{stderr}");
    }

    #[test]
    fn fleet_backup_writes_a_copy() {
        let app = App::new("");
        app.provision(&app.fleet(), &["acme"], 3);
        let dest = app.dir.join("acme-export.db");

        // `tenant/<id>` is accepted as well as the canonical `tenant:<id>`.
        run_ok(
            &app.dir,
            &["fleet", "backup", "tenant/acme", dest.to_str().unwrap()],
        );
        assert_eq!(count_notes(&dest), 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "only the owner reads the copy");
        }

        let stderr = run_err(
            &app.dir,
            &["fleet", "backup", "tenant:acme", dest.to_str().unwrap()],
        );
        assert!(stderr.contains("already exists"), "{stderr}");

        let other = app.dir.join("missing.db");
        let stderr = run_err(
            &app.dir,
            &["fleet", "backup", "tenant:initech", other.to_str().unwrap()],
        );
        assert!(stderr.contains("does not exist"), "{stderr}");
        assert!(!app.path("initech").exists(), "backup never creates");
        assert!(!other.exists());

        let stderr = run_err(
            &app.dir,
            &["fleet", "backup", "Tenant:ACME", other.to_str().unwrap()],
        );
        assert!(stderr.contains("not a database name"), "{stderr}");
    }

    #[test]
    fn fleet_delete_needs_force_and_removes_the_database() {
        let app = App::new("");
        app.provision(&app.fleet(), &["acme", "globex"], 1);
        let path = app.path("acme");

        let stderr = run_err(&app.dir, &["fleet", "delete", "tenant:acme"]);
        assert!(stderr.contains("--force"), "{stderr}");
        assert!(path.is_file());

        run_ok(&app.dir, &["fleet", "delete", "tenant:acme", "--force"]);
        assert!(!path.exists());
        assert!(app.path("globex").is_file(), "only the named database");

        let stderr = run_err(&app.dir, &["fleet", "delete", "tenant:acme", "--force"]);
        assert!(stderr.contains("does not exist"), "{stderr}");
    }

    #[test]
    fn fleet_delete_refuses_a_database_open_elsewhere() {
        let app = App::new("");
        app.provision(&app.fleet(), &["acme"], 1);
        let path = app.path("acme");
        // A live app: another process with the file open.
        let mut live = connect(&path);
        live.batch_execute("SELECT 1 FROM notes").unwrap();

        let stderr = run_err(&app.dir, &["fleet", "delete", "tenant:acme", "--force"]);
        assert!(stderr.contains("open in another process"), "{stderr}");
        assert!(path.is_file(), "nothing was removed");

        // The app closes the database: the delete goes through.
        drop(live);
        run_ok(&app.dir, &["fleet", "delete", "tenant:acme", "--force"]);
        assert!(!path.exists());
    }

    /// The `fleet_replication` setup: a fleet that ships to a local replica
    /// directory, under the root `autumn.toml`'s `[replication]` resolves to.
    fn replicated_fleet(app: &App, replicas: &Path) -> (DatabaseFleet, Arc<FleetReplication>) {
        let fleet = app.fleet();
        let destination: Arc<dyn ReplicaDestination> =
            Arc::new(FileDestination::new(replicas).unwrap());
        let settings = ReplicationSettings {
            database_path: PathBuf::new(),
            root: autumn_web::replication::segment::root_prefix(Some("app"), "dev"),
            sync_interval: Duration::from_secs(1),
            snapshot_interval: Duration::from_secs(3600),
            max_wal_bytes: 4 * 1024 * 1024,
            retention: Duration::from_secs(24 * 3600),
            verify_interval: None,
        };
        let replication = Arc::new(FleetReplication::new(
            destination,
            settings,
            Duration::from_secs(30),
            false,
            Arc::new(autumn_web::time::SystemClock),
        ));
        assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
        (fleet, replication)
    }

    #[test]
    fn fleet_restore_rebuilds_a_database_from_its_replica() {
        let tmp = tempfile::tempdir().unwrap();
        let replicas = tmp.path().join("replicas");
        let app = App::new(&format!(
            "[replication]\nenabled = true\npath = \"{}\"\nprefix = \"app\"\n",
            replicas.display()
        ));
        let (fleet, replication) = replicated_fleet(&app, &replicas);
        let rt = runtime();
        let before_second = rt.block_on(async {
            let key = fleet.key_for("acme").unwrap();
            let db = fleet.provision(&key).await.unwrap();
            // Written while the fleet holds the database open, so the
            // replicator ships the frames.
            exec(db.path(), "INSERT INTO notes VALUES ('first')");
            let ticker = Arc::clone(&replication);
            tokio::task::spawn_blocking(move || ticker.tick_all())
                .await
                .unwrap();
            // A restore plans by millisecond timestamps.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let between = chrono::Utc::now();
            tokio::time::sleep(Duration::from_millis(100)).await;
            exec(db.path(), "INSERT INTO notes VALUES ('second')");
            drop(db);
            // Closing ships the last frames.
            fleet.close_all().await;
            between
        });
        let path = app.path("acme");
        assert_eq!(count_notes(&path), 2);
        exec(&path, "DELETE FROM notes");

        // Destructive: needs --force.
        let stderr = run_err(&app.dir, &["fleet", "restore", "tenant:acme"]);
        assert!(stderr.contains("--force"), "{stderr}");
        assert_eq!(count_notes(&path), 0, "nothing was restored");

        let (_, stderr) = run_ok(&app.dir, &["fleet", "restore", "tenant:acme", "--force"]);
        assert!(stderr.contains("integrity verified"), "{stderr}");
        assert_eq!(count_notes(&path), 2);

        // Point in time.
        let at = before_second.to_rfc3339();
        run_ok(
            &app.dir,
            &[
                "fleet",
                "restore",
                "tenant:acme",
                "--timestamp",
                &at,
                "--force",
            ],
        );
        assert_eq!(count_notes(&path), 1);

        // A lost volume: the file is gone, the replica is not. The operator
        // makes the new root.
        std::fs::remove_dir_all(app.root()).unwrap();
        std::fs::create_dir(app.root()).unwrap();
        run_ok(&app.dir, &["fleet", "restore", "tenant:acme", "--force"]);
        assert_eq!(count_notes(&path), 2);

        let stderr = run_err(
            &app.dir,
            &[
                "fleet",
                "restore",
                "tenant:acme",
                "--timestamp",
                "yesterday",
                "--force",
            ],
        );
        assert!(stderr.contains("RFC 3339"), "{stderr}");

        let stderr = run_err(&app.dir, &["fleet", "restore", "tenant:globex", "--force"]);
        assert!(
            stderr.contains("tenant:globex has no replica under app/dev/fleet/tenant/globex"),
            "{stderr}"
        );
    }

    #[test]
    fn fleet_restore_refuses_a_database_open_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let replicas = tmp.path().join("replicas");
        let app = App::new(&format!(
            "[replication]\nenabled = true\npath = \"{}\"\nprefix = \"app\"\n",
            replicas.display()
        ));
        let (fleet, _) = replicated_fleet(&app, &replicas);
        runtime().block_on(async {
            fleet
                .provision(&fleet.key_for("acme").unwrap())
                .await
                .unwrap();
            fleet.close_all().await;
        });
        let path = app.path("acme");
        let mut live = connect(&path);
        live.batch_execute("SELECT 1 FROM notes").unwrap();

        let stderr = run_err(&app.dir, &["fleet", "restore", "tenant:acme", "--force"]);
        assert!(stderr.contains("open in another process"), "{stderr}");

        drop(live);
        run_ok(&app.dir, &["fleet", "restore", "tenant:acme", "--force"]);
    }

    #[test]
    fn fleet_restore_needs_replication() {
        let app = App::new("");
        app.provision(&app.fleet(), &["acme"], 1);
        let stderr = run_err(&app.dir, &["fleet", "restore", "tenant:acme", "--force"]);
        assert!(stderr.contains("[replication]"), "{stderr}");
        assert_eq!(count_notes(&app.path("acme")), 1);
    }
}
