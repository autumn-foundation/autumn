//! Managed local Postgres.
//!
//! A [`DatabasePoolProvider`] that provisions and supervises a local `postgres`
//! child in the app's data dir, surfaced through the existing pluggable
//! pool-provider seam (no changes to the query path).
//!
//! Enabled by the `managed-pg` feature. With `managed-pg-bundled` the Postgres
//! binaries are embedded in the app executable; otherwise they are downloaded
//! on first run. Either way Postgres runs as a supervised child process with an
//! on-disk data dir (it is never linked in-process).
//!
//! # Example
//!
//! ```ignore
//! let pg = ManagedPostgresPoolProvider::new();
//! autumn_web::app()
//!     .with_pool_provider(pg.clone())
//!     .on_shutdown(move || { let pg = pg.clone(); async move { pg.stop().await; } })
//!     .run()
//!     .await;
//! ```

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::deadpool::Pool;
use postgresql_embedded::{PostgreSQL, Settings, VersionReq};

use crate::config::DatabaseConfig;
use crate::db::{DatabasePoolProvider, PoolError, create_pool};

/// Database created inside the managed cluster for the application.
const MANAGED_DB_NAME: &str = "autumn";

/// Environment variable that points the provider at a persistent data dir.
/// `autumn serve` sets this to a platform data dir; falls back to a per-user
/// default otherwise.
pub const MANAGED_PG_DATA_DIR_ENV: &str = "AUTUMN_MANAGED_PG_DATA_DIR";

/// Makes the provider *attach* to an already-running cluster instead of starting
/// its own.
///
/// When set, the value is the connection URL to use. `autumn task`/`autumn build`
/// set this (to the URL the running daemon published, see [`PUBLISHED_URL_FILE`])
/// so a one-off command shares the daemon's live cluster rather than failing to
/// start a second postmaster on the daemon's locked data dir.
pub const MANAGED_PG_ATTACH_URL_ENV: &str = "AUTUMN_MANAGED_PG_ATTACH_URL";

/// File the provider writes with the running cluster's connection URL.
///
/// Written next to the data dir at mode `0600` so `autumn task`/`autumn build`
/// can discover it and attach via [`MANAGED_PG_ATTACH_URL_ENV`]. Lives in the
/// data dir's parent alongside `.autumn-pg-superuser` (the secret it embeds is
/// the same one).
pub const PUBLISHED_URL_FILE: &str = ".autumn-pg-url";

/// How long to allow each `initdb`/`start` step before surfacing a diagnostic.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// The managed provider that most recently started a cluster this process,
/// retained so abnormal boot-failure paths can stop the supervised child even
/// though they bypass `on_shutdown`. Last-writer-wins for the same reason.
static EMERGENCY_PROVIDER: std::sync::Mutex<Option<ManagedPostgresPoolProvider>> =
    std::sync::Mutex::new(None);

/// Synchronously stop the managed Postgres cluster started this process, if any.
///
/// Wired into abnormal boot-failure paths (e.g. a failed startup migration) that
/// call [`std::process::exit`] — which skips `on_shutdown` hooks and Rust
/// destructors and would otherwise orphan the supervised Postgres child while it
/// holds the data dir and port. Best-effort and idempotent: a no-op when no
/// managed cluster is running or it was already stopped cleanly.
pub(crate) fn emergency_stop() {
    let Some(provider) = EMERGENCY_PROVIDER.lock().ok().and_then(|g| g.clone()) else {
        return;
    };
    // These exit paths run on a blocking migration thread (no async runtime
    // entered), so drive the async stop to completion on a short-lived
    // current-thread runtime.
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(provider.stop()),
        Err(e) => {
            tracing::warn!(error = %e, "could not stop managed Postgres during emergency shutdown");
        }
    }
}

/// Async variant of [`emergency_stop`] for boot-failure paths that run *inside*
/// the async runtime (e.g. a socket-bind failure after the cluster started),
/// where [`emergency_stop`]'s nested blocking runtime would panic.
pub(crate) async fn emergency_stop_async() {
    let provider = EMERGENCY_PROVIDER.lock().ok().and_then(|g| g.clone());
    if let Some(provider) = provider {
        provider.stop().await;
    }
}

/// Enforce `0600` on the managed-cluster superuser password file, or exit
/// fatally. The file holds the local cluster's superuser password; if we cannot
/// keep it owner-only (a `chmod` rejected by the filesystem/ACL, or a file owned
/// by another user), refuse to boot rather than run with the secret readable by
/// other local users — matching the provider's other fatal-boot handling, and
/// safe to call before the server is started (no child to orphan).
#[cfg(unix)]
fn enforce_password_file_mode_or_exit(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::error!(error = %e, path = %path.display(),
            "managed Postgres: cannot enforce owner-only permissions on the superuser password file");
        eprintln!(
            "autumn: refusing to start managed Postgres — cannot set 0600 on {} ({e}). \
             The cluster superuser password would be readable by other local users.",
            path.display()
        );
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn enforce_password_file_mode_or_exit(_path: &Path) {}

/// Make the directory that holds the managed-cluster superuser password file
/// private (`0700`), or exit fatally. Rejects a symlink (a local user could
/// repoint it) and, by treating a failed `chmod` as fatal, a directory owned by
/// another user (a non-owner `chmod` errors). Best-effort against a root-owned
/// attacker dir, which only a full `st_uid` check would catch — but it closes
/// the common permissive-umask / shared-parent hole for non-root runs.
#[cfg(unix)]
fn harden_credential_dir_or_exit(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            reject_credential_dir(dir, "credential directory is a symlink");
        }
        Ok(_) => {
            if std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).is_err() {
                reject_credential_dir(
                    dir,
                    "cannot enforce owner-only (0700) permissions on the credential directory",
                );
            }
        }
        Err(_) => reject_credential_dir(dir, "cannot stat the credential directory"),
    }
}

#[cfg(unix)]
fn reject_credential_dir(dir: &Path, reason: &str) -> ! {
    tracing::error!(path = %dir.display(), "managed Postgres: {reason}");
    eprintln!(
        "autumn: refusing to start managed Postgres — {reason} for {}. The cluster \
         superuser password file lives here and could be replaced by another \
         local user.",
        dir.display()
    );
    std::process::exit(1);
}

#[cfg(not(unix))]
fn harden_credential_dir_or_exit(_dir: &Path) {}

/// Drop the current provider from the process-global emergency-stop slot.
fn clear_emergency_registration() {
    if let Ok(mut g) = EMERGENCY_PROVIDER.lock() {
        *g = None;
    }
}

/// Generate a random 24-character alphanumeric password for the managed cluster.
fn generate_password() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 24];
    // Fall back to a time-seeded value only if the OS RNG is unavailable; the
    // password protects a local single-user cluster.
    if getrandom::getrandom(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0u128, |d| d.as_nanos());
        let seed = nanos.to_le_bytes();
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = seed[i % seed.len()];
        }
    }
    bytes
        .iter()
        .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
        .collect()
}

/// A [`DatabasePoolProvider`] backed by a managed, locally-supervised Postgres.
///
/// `create_pool` runs `initdb` (idempotent), starts the server, ensures the
/// application database exists, and then builds the normal `deadpool` pool from
/// the resulting URL — reusing all of Autumn's existing pool logic. The running
/// server handle is retained so the child is not stopped when the call returns;
/// [`stop`](Self::stop) (wired to the daemon's `on_shutdown`) shuts it down.
#[derive(Clone)]
pub struct ManagedPostgresPoolProvider {
    data_dir: PathBuf,
    /// Explicit version requirement, or `None` to use `postgresql_embedded`'s
    /// own default. The default is what makes `managed-pg-bundled` work offline:
    /// `Settings::new()` resolves to the *exact* embedded archive version under
    /// the `bundled` feature (and `*` otherwise). Overriding it with a non-exact
    /// `*` would force `setup()` to resolve the version from the network even
    /// when an archive is compiled into the binary.
    version: Option<VersionReq>,
    instance: Arc<Mutex<Option<PostgreSQL>>>,
    /// The cluster's connection URL once resolved — set whether we *started* the
    /// cluster or *attached* to an externally-owned one. `create_topology` reads
    /// it for the migration URL, which is the only path that needs the URL when
    /// attached (there is no local `instance` to read it from).
    resolved_url: Arc<Mutex<Option<String>>>,
    /// Whether we attached to a cluster owned by another process (the daemon)
    /// rather than starting our own. When set, [`stop`](Self::stop) is a no-op:
    /// the cluster's lifecycle belongs to whoever started it.
    attached: Arc<AtomicBool>,
}

impl std::fmt::Debug for ManagedPostgresPoolProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedPostgresPoolProvider")
            .field("data_dir", &self.data_dir)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl Default for ManagedPostgresPoolProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ManagedPostgresPoolProvider {
    /// Construct a provider using the data dir from `AUTUMN_MANAGED_PG_DATA_DIR`
    /// (set by `autumn serve`) or a per-user default.
    #[must_use]
    pub fn new() -> Self {
        Self::with_data_dir(default_data_dir())
    }

    /// Construct a provider that stores its cluster under `data_dir`.
    #[must_use]
    pub fn with_data_dir(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            // Use `postgresql_embedded`'s default so a bundled build pins to its
            // embedded archive version (offline, exact) instead of `*`.
            version: None,
            instance: Arc::new(Mutex::new(None)),
            resolved_url: Arc::new(Mutex::new(None)),
            attached: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Directory the bundled/downloaded Postgres binaries are extracted to, kept
    /// under the managed data root (the data dir's parent) so it never depends on
    /// `$HOME`.
    fn installation_dir(&self) -> PathBuf {
        self.data_dir
            .parent()
            .unwrap_or(&self.data_dir)
            .join("postgresql")
    }

    /// Path of the published connection-URL file (data dir's parent, alongside
    /// the superuser password file).
    fn published_url_path(&self) -> PathBuf {
        self.data_dir
            .parent()
            .unwrap_or(&self.data_dir)
            .join(PUBLISHED_URL_FILE)
    }

    /// Publish the running cluster's URL (mode `0600`) so `autumn task`/`autumn
    /// build` can attach to it. Best-effort: a failure only means those one-off
    /// commands fall back to their own resolution; it must not fail the boot.
    fn publish_url(&self, url: &str) {
        let path = self.published_url_path();
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut f) => {
                use std::io::Write;
                // `mode(0o600)` only applies on create; an existing (older-build /
                // manually-created / symlinked) file keeps its old, possibly
                // permissive mode. The URL embeds the cluster superuser password,
                // so if we can't enforce owner-only we must NOT write it — skip
                // publishing (it's only an optimization for task/build attach) and
                // drop the truncated file rather than expose credentials.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                        .is_err()
                    {
                        tracing::warn!(path = %path.display(),
                            "managed Postgres: cannot enforce owner-only mode on the cluster URL file; not publishing");
                        let _ = std::fs::remove_file(&path);
                        return;
                    }
                }
                if let Err(e) = f.write_all(url.as_bytes()) {
                    tracing::warn!(error = %e, path = %path.display(),
                        "managed Postgres: failed to publish cluster URL for task/build attach");
                }
            }
            Err(e) => tracing::warn!(error = %e, path = %path.display(),
                "managed Postgres: failed to open cluster URL file for publishing"),
        }
    }

    /// Provision (if needed) and start the cluster, returning its connection URL.
    async fn ensure_running(&self) -> Result<String, postgresql_embedded::Error> {
        // Already started (e.g. a second pool, or repeated calls): reuse the
        // running instance instead of trying to start a second server on the
        // same — now locked — data dir.
        if let Ok(guard) = self.instance.lock()
            && let Some(pg) = guard.as_ref()
        {
            return Ok(pg.settings().url(MANAGED_DB_NAME));
        }

        // Attach mode: a cluster is already running (started by `autumn serve`)
        // and `autumn task`/`autumn build` set `AUTUMN_MANAGED_PG_ATTACH_URL` to
        // its published URL. Connect to it instead of starting a second
        // postmaster on the daemon's locked data dir — and never `stop()` it,
        // since its lifecycle belongs to the daemon.
        if let Some(url) = std::env::var_os(MANAGED_PG_ATTACH_URL_ENV)
            .and_then(|v| v.into_string().ok())
            .filter(|s| !s.trim().is_empty())
        {
            self.attached.store(true, Ordering::SeqCst);
            if let Ok(mut g) = self.resolved_url.lock() {
                *g = Some(url.clone());
            }
            return Ok(url);
        }

        let mut settings = Settings::new();
        settings.data_dir.clone_from(&self.data_dir);
        // Extract the Postgres binaries under the managed data root rather than
        // `Settings::new()`'s `$HOME/.theseus` default, which may be unset or
        // unwritable under launchd/systemd/container supervisors and would fail
        // before `initdb` even when the CLI-provided data dir is valid.
        settings.installation_dir = self.installation_dir();
        // Only override the default version when one was explicitly requested;
        // otherwise keep `Settings::new()`'s default so a `managed-pg-bundled`
        // build uses its exact embedded archive version (no network resolution).
        if let Some(version) = &self.version {
            settings.version = version.clone();
        }
        // Persistent cluster tied to the daemon, not a throwaway temp dir.
        settings.temporary = false;
        settings.timeout = Some(STARTUP_TIMEOUT);
        // Stable superuser credentials: `initdb` bakes the password into the
        // cluster on first boot only, but `Settings::new()` generates a *fresh*
        // random password (and a temp password file) every time. Persisting
        // them next to the data dir keeps later boots able to authenticate.
        let (password, password_file) = self.stable_credentials();
        settings.password = password;
        settings.password_file = password_file;

        let mut pg = PostgreSQL::new(settings);
        // `setup` is idempotent: it downloads/extracts and runs `initdb` only
        // when the data dir is not already initialized.
        pg.setup().await?;
        pg.start().await?;
        // The post-start database checks can still fail (auth, bootstrap-DB
        // connection, etc.). The server is already running by now, so stop it
        // before propagating — `create_pool` turns this error into
        // `process::exit(1)`, which skips `Drop`/`on_shutdown` and would
        // otherwise orphan the postmaster on the data dir/port.
        if let Err(e) = Self::ensure_app_database(&pg).await {
            let _ = pg.stop().await;
            return Err(e);
        }
        let url = pg.settings().url(MANAGED_DB_NAME);

        // Retain the handle so the child keeps running after this returns.
        if let Ok(mut guard) = self.instance.lock() {
            *guard = Some(pg);
        }
        if let Ok(mut g) = self.resolved_url.lock() {
            *g = Some(url.clone());
        }

        // Publish the URL so `autumn task`/`autumn build` can attach to this
        // running cluster instead of contending for its locked data dir.
        self.publish_url(&url);

        // Register for emergency shutdown so an abnormal boot exit (which
        // bypasses `on_shutdown`) still stops the supervised child instead of
        // orphaning it. The clone shares `instance`, so this is idempotent with
        // a normal `stop()`.
        if let Ok(mut g) = EMERGENCY_PROVIDER.lock() {
            *g = Some(self.clone());
        }

        Ok(url)
    }

    /// Ensure the application database exists in the running cluster. Separated
    /// so [`ensure_running`](Self::ensure_running) can stop the server if any of
    /// these post-start checks fail.
    async fn ensure_app_database(pg: &PostgreSQL) -> Result<(), postgresql_embedded::Error> {
        if !pg.database_exists(MANAGED_DB_NAME).await? {
            pg.create_database(MANAGED_DB_NAME).await?;
        }
        Ok(())
    }

    /// Stable superuser password + password-file path persisted alongside the
    /// cluster (in the data dir's parent, since `initdb` requires an empty data
    /// dir). Generated once on first boot and reused on every later boot.
    fn stable_credentials(&self) -> (String, PathBuf) {
        let parent = self
            .data_dir
            .parent()
            .unwrap_or(&self.data_dir)
            .to_path_buf();
        let _ = std::fs::create_dir_all(&parent);
        // Harden the directory that holds the superuser password file: even with
        // the file at `0600`, a local user with write access to a permissive
        // parent could *replace* it and feed us attacker-controlled credentials.
        // Failing closed here also rejects a parent owned by another user (a
        // non-owner `chmod` errors, except for root).
        harden_credential_dir_or_exit(&parent);
        let password_file = parent.join(".autumn-pg-superuser");

        if let Ok(existing) = std::fs::read_to_string(&password_file) {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                // An existing file (older build, manual creation) may have a
                // permissive mode; the create-time `0600` below never applied to
                // it. Tighten it before reusing — it holds the cluster superuser
                // password — and refuse to boot if we can't.
                enforce_password_file_mode_or_exit(&password_file);
                return (trimmed.to_owned(), password_file);
            }
        }

        let password = generate_password();
        // 0600 — the password grants superuser on the local cluster.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(mut f) = options.open(&password_file) {
            use std::io::Write;
            // `mode(0o600)` only applies when the file is newly created; an
            // existing empty/permissive file (interrupted first boot, manual
            // `touch`) keeps its old mode, so tighten it (failing closed) before
            // writing the superuser password into it.
            enforce_password_file_mode_or_exit(&password_file);
            let _ = f.write_all(password.as_bytes());
        }
        (password, password_file)
    }

    /// Stop the supervised Postgres child. Wire this to the daemon's
    /// `on_shutdown` so the cluster shuts down with the app.
    pub async fn stop(&self) {
        // Attached to a cluster owned by another process (the daemon): never stop
        // it or remove its published URL — `autumn task`/`autumn build` only
        // borrowed the running cluster.
        if self.attached.load(Ordering::SeqCst) {
            return;
        }
        let taken = self.instance.lock().ok().and_then(|mut guard| guard.take());
        let Some(pg) = taken else {
            // Nothing running (or already stopped): clear the emergency registration.
            clear_emergency_registration();
            return;
        };
        if let Err(e) = pg.stop().await {
            // The postmaster is still up. Put the handle back so a later
            // `stop()`/emergency stop can retry, and leave the emergency
            // registration in place — dropping both here would strand a running
            // cluster with no way to reach it (notably for direct/foreground
            // runs without the CLI's `postmaster.pid` reaper).
            tracing::warn!(error = %e, "failed to stop managed Postgres cleanly; will retry on next stop");
            if let Ok(mut guard) = self.instance.lock() {
                *guard = Some(pg);
            }
            return;
        }
        // Stopped cleanly: drop the emergency-stop registration so a later app in
        // the same process (integration tests) doesn't try to stop this now-dead
        // handle. The resolved URL is carried on the per-app topology, so there is
        // no process-global URL to clear.
        clear_emergency_registration();
        // Remove the published URL file so a later `autumn task`/`autumn build`
        // doesn't try to attach to a cluster that is now down.
        let _ = std::fs::remove_file(self.published_url_path());
    }
}

impl DatabasePoolProvider for ManagedPostgresPoolProvider {
    async fn create_pool(
        &self,
        config: &DatabaseConfig,
    ) -> Result<Option<Pool<AsyncPgConnection>>, PoolError> {
        // Reject a sharded config before starting anything. When the provider is
        // installed only as the *pool* provider (`.with_pool_provider`),
        // `resolve_shard_set` builds the shards directly from config and never
        // calls `create_shard_topology`, so its fail-fast is bypassed — without
        // this check the app would boot with control/migrations on the managed DB
        // but tenant shards on external URLs.
        if config.has_shards() {
            reject_managed_shards();
        }
        let url = match self.ensure_running().await {
            Ok(url) => url,
            Err(e) => {
                // A failed cluster start is fatal at boot — surface a clear
                // diagnostic instead of hanging or silently running DB-less.
                tracing::error!(error = %e, data_dir = %self.data_dir.display(),
                    "managed Postgres failed to provision/start");
                eprintln!(
                    "autumn: managed Postgres failed to start ({e}). \
                     Data dir: {}",
                    self.data_dir.display()
                );
                std::process::exit(1);
            }
        };

        // Build the normal pool from the managed URL; reuses all pool logic.
        let mut managed = config.clone();
        managed.primary_url = Some(url);
        managed.url = None;
        create_pool(&managed)
    }

    async fn create_topology(
        &self,
        config: &DatabaseConfig,
    ) -> Result<Option<crate::db::DatabaseTopology>, PoolError> {
        // The managed cluster is a single local primary. Ignore any external
        // `replica_url` from the original config so reads don't silently hit an
        // unrelated/stale replica while writes and migrations go to the managed
        // primary; the default impl would build that replica pool.
        let Some(primary) = self.create_pool(config).await? else {
            return Ok(None);
        };
        // After `create_pool`, the cluster is running; carry its URL on the
        // topology so startup migrations target this managed cluster — per-app,
        // with no process-global. Prefer the resolved URL (set whether we started
        // the cluster or attached to one), falling back to the retained handle.
        let migration_url =
            self.resolved_url
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .or_else(|| {
                    self.instance.lock().ok().and_then(|guard| {
                        guard.as_ref().map(|pg| pg.settings().url(MANAGED_DB_NAME))
                    })
                });
        Ok(Some(
            crate::db::DatabaseTopology::from_pools(primary, None)
                .with_migration_url(migration_url),
        ))
    }

    async fn create_shard_topology(
        &self,
        _shard: &crate::config::ShardConfig,
        _defaults: &DatabaseConfig,
    ) -> Result<crate::db::DatabaseTopology, PoolError> {
        // `setup_database` already started the cluster via `create_topology`;
        // `process::exit` skips `on_shutdown`, so stop the child first to avoid
        // orphaning it on the data dir/port, then fail fast.
        self.stop().await;
        reject_managed_shards();
    }
}

/// Print the "managed Postgres can't back shards" diagnostic and exit.
///
/// The managed cluster is a single local Postgres; `[[database.shards]]` needs
/// separate per-shard databases it can't provide, and silently pointing shards
/// at their external URLs while control/migrations use the managed DB would be a
/// split-brain. Shared by the control (`create_pool`) and shard
/// (`create_shard_topology`) paths.
fn reject_managed_shards() -> ! {
    tracing::error!(
        "managed Postgres does not support [[database.shards]]; remove the \
         shard configuration or use an external database"
    );
    eprintln!(
        "autumn: managed Postgres (--bundled-pg) does not support \
         [[database.shards]]. Remove shard config or use an external database."
    );
    std::process::exit(1);
}

/// Resolve the cluster data dir: the `AUTUMN_MANAGED_PG_DATA_DIR` override
/// (set per-project by `autumn serve`), then a per-user XDG-style default, then a
/// temp-dir fallback. The default/fallback are namespaced per project so two
/// unrelated apps using the provider directly (e.g. `cargo run`) don't share —
/// and contend for — one cluster.
fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(MANAGED_PG_DATA_DIR_ENV) {
        return PathBuf::from(dir);
    }
    // Layout is `<base>/<project>/pg`, not `<base>/pg/<project>`: the data dir's
    // *parent* holds the per-cluster `.autumn-pg-superuser` file (initdb needs an
    // empty data dir), so the project component must be the parent or every
    // direct-run project under one user would share a single superuser password.
    let project = project_data_namespace();
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local/share/autumn")
            .join(project)
            .join("pg");
    }
    // Windows (where HOME is usually unset): use a persistent app-data dir
    // rather than the volatile temp dir, which is cleared on reboot.
    if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_appdata)
            .join("autumn")
            .join(project)
            .join("pg");
    }
    std::env::temp_dir().join("autumn").join(project).join("pg")
}

/// A per-project namespace component for the fallback data dir, derived from the
/// app's manifest dir hashed with SHA-256. Stable across runs and toolchains so
/// an app keeps finding its cluster.
///
/// `#[autumn_web::main]` records the compile-time manifest dir via
/// `__set_macro_context` (not an exported env var), so resolve it through
/// [`config::OsEnv`], which surfaces that baked value for `AUTUMN_MANIFEST_DIR` —
/// otherwise installed binaries run from a common working dir would all hash the
/// same CWD and share one cluster. Falls back to the CWD only when no manifest
/// context exists.
fn project_data_namespace() -> String {
    use crate::config::Env;
    use sha2::{Digest, Sha256};
    let base = crate::config::OsEnv
        .var("AUTUMN_MANIFEST_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .and_then(|d| std::fs::canonicalize(&d).ok().or(Some(d)))
        .unwrap_or_else(|| PathBuf::from("."));
    let digest = Sha256::digest(base.to_string_lossy().as_bytes());
    hex::encode(&digest[..4])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_data_dir_honors_env_override() {
        temp_env::with_var(MANAGED_PG_DATA_DIR_ENV, Some("/tmp/custom/pg"), || {
            assert_eq!(default_data_dir(), PathBuf::from("/tmp/custom/pg"));
        });
    }

    #[test]
    fn with_data_dir_is_used() {
        let p = ManagedPostgresPoolProvider::with_data_dir(PathBuf::from("/var/lib/x"));
        assert_eq!(p.data_dir, PathBuf::from("/var/lib/x"));
    }

    #[test]
    fn attach_mode_uses_env_url_without_starting() {
        // With the attach env set, `ensure_running` must return that URL without
        // touching the (nonexistent) data dir, and `stop` must be a no-op.
        temp_env::with_var(
            MANAGED_PG_ATTACH_URL_ENV,
            Some("postgresql://u:p@localhost:5499/autumn"),
            || {
                let provider =
                    ManagedPostgresPoolProvider::with_data_dir(PathBuf::from("/nonexistent/pg"));
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let url = rt.block_on(provider.ensure_running()).unwrap();
                assert_eq!(url, "postgresql://u:p@localhost:5499/autumn");
                assert!(provider.attached.load(Ordering::SeqCst));
                // No-op in attach mode: must not panic or strand state.
                rt.block_on(provider.stop());
            },
        );
    }

    #[test]
    fn published_url_path_is_beside_data_dir() {
        let p = ManagedPostgresPoolProvider::with_data_dir(PathBuf::from("/var/lib/app/pg"));
        assert_eq!(
            p.published_url_path(),
            PathBuf::from("/var/lib/app").join(PUBLISHED_URL_FILE)
        );
    }
}
