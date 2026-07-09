//! Tauri desktop shell for reddit-clone.
//!
//! Lifecycle:
//!   1. Bind loopback:0 to find a free ephemeral port (no hardcoded port collision).
//!      Note: there is a brief window between dropping the listener and the sidecar
//!      binding the port; in practice this race is extremely rare on loopback.
//!   2. Spawn the autumn server sidecar with `AUTUMN_SERVER__PORT` set to that port.
//!      `AUTUMN_MANAGED_PG_DATA_DIR` is set to `<app-data-dir>/db` so the managed
//!      Postgres cluster (autumn feature #1119) persists across restarts.
//!      `AUTUMN_MANAGED_PG_ATTACH_URL` is cleared so an inherited attach URL cannot
//!      redirect the sidecar to a foreign cluster instead of the bundled one.
//!   3. Poll GET /health in a background thread until the server is ready (up to 30 s),
//!      then open the webview window pointing at http://127.0.0.1:<port>.
//!      On timeout, the app exits with a non-zero code rather than showing a blank window.
//!   4. On main window close, send SIGTERM for graceful shutdown (so on_shutdown hooks
//!      run, including ManagedPostgresPoolProvider::stop()), then force-kill after 3 s.

use std::net::TcpListener;
use tauri::{Manager, App};
use tauri_plugin_shell::{ShellExt, process::{CommandChild, CommandEvent}};

struct SidecarHandle(std::sync::Mutex<Option<CommandChild>>);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(SidecarHandle(std::sync::Mutex::new(None)))
        .setup(|app| setup(app))
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                // Only shut down the sidecar when the main window closes, not on
                // secondary windows (dialogs, settings panels, etc.).
                if window.label() == "main" {
                    let handle = window.app_handle();
                    if let Some(child) = handle
                        .state::<SidecarHandle>()
                        .0
                        .lock()
                        .unwrap()
                        .take()
                    {
                        // On Unix: send SIGTERM so autumn's tokio signal handler
                        // runs on_shutdown hooks (including ManagedPostgresPoolProvider
                        // ::stop()), then force-kill after 5 s.
                        // AUTUMN_SERVER__PRESTOP_GRACE_SECS is set to 0 above so the
                        // listener drain is skipped; the full 5 s budget is available
                        // for on_shutdown hooks (e.g. pg_ctl stop -m fast).
                        // On Windows: autumn only handles tokio::signal::ctrl_c()
                        // (CTRL_C_EVENT).  taskkill sends WM_CLOSE/CTRL_CLOSE_EVENT
                        // which autumn does not handle; graceful shutdown via external
                        // signal is not achievable without process-group manipulation,
                        // so force-kill immediately.
                        #[cfg(unix)]
                        let graceful_pid = child.pid();
                        std::thread::spawn(move || {
                            #[cfg(unix)]
                            {
                                let _ = std::process::Command::new("kill")
                                    .args(["-TERM", &graceful_pid.to_string()])
                                    .status();
                                std::thread::sleep(std::time::Duration::from_secs(5));
                                let _ = child.kill();
                            }
                            #[cfg(windows)]
                            {
                                let _ = child.kill();
                            }
                        });
                    }
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn setup(app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Find a free loopback port: bind :0, read the assigned port, then drop
    //    the listener so the autumn server can bind that same address.
    //    Note: there is a brief window between dropping the listener and the sidecar
    //    binding; in practice this race is extremely rare on loopback.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0")?;
        l.local_addr()?.port()
    };

    // 2. Persistent per-app data directories.
    //    Use subdirectories so distinct concerns don't share the same root.
    let data_root = app.path().app_data_dir()?;
    // Postgres cluster (#1119) in db/.  Create proactively; the sidecar won't if absent.
    let app_data_dir = data_root.join("db");
    std::fs::create_dir_all(&app_data_dir)?;
    // Local blob storage in blobs/.  Create before the sidecar spawns so we can
    // restrict the directory to owner-only — LocalBlobStore::new/put use
    // create_dir_all/write which inherit the process umask (typically 0755/0644),
    // leaving private uploads readable by other local accounts on multi-user systems.
    let blobs_dir = data_root.join("blobs");
    std::fs::create_dir_all(&blobs_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&blobs_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    // Per-install signing secret: autumn requires one in prod mode.  Generate 32
    // random bytes on first launch and persist them so tokens survive restarts.
    let signing_secret = load_or_generate_signing_secret(&data_root)?;

    // autumn.toml is bundled as a Tauri resource (see tauri.conf.json bundle.resources).
    // The sidecar's working directory is set to resource_dir so AutumnConfig finds it.
    //
    // Why CWD and not AUTUMN_MANIFEST_DIR env var:
    //   OsEnv::var("AUTUMN_MANIFEST_DIR") returns the compile-time CARGO_MANIFEST_DIR
    //   set by #[autumn_web::main], overriding the process environment.  That path
    //   doesn't exist on the installed machine, so find_config_file_named() falls back
    //   to PathBuf::from("autumn.toml") — i.e. the current working directory.
    //   Setting CWD to resource_dir makes that CWD fallback find the bundled config.
    let resource_dir = app.path().resource_dir()?;

    // 3. Spawn the autumn server sidecar (built with autumn-web/embed-assets + managed-pg-bundled).
    //    The sidecar() argument is the binary basename matching externalBin in tauri.conf.json.
    let (mut rx, child) = app
        .shell()
        .sidecar("reddit-clone")?
        // Working directory = resource dir so autumn.toml is found via CWD fallback.
        .current_dir(&resource_dir)
        .env("AUTUMN_SERVER__HOST", "127.0.0.1")
        .env("AUTUMN_SERVER__PORT", port.to_string())
        .env(
            "AUTUMN_MANAGED_PG_DATA_DIR",
            app_data_dir.to_string_lossy().as_ref(),
        )
        // Redirect local blob storage to a writable per-app location.
        // Default storage.local.root is "target/blobs" — relative to CWD (resource_dir),
        // which is read-only in installed bundles; the app would abort before opening the window.
        // Route blobs to {app-data-dir}/blobs where the process always has write access.
        .env(
            "AUTUMN_STORAGE__LOCAL__ROOT",
            data_root.join("blobs").to_string_lossy().as_ref(),
        )
        // Autumn's StorageConfig::backend_plan rejects local-backend configs in prod
        // mode unless this flag is set, aborting startup before the window opens.
        // A loopback-only desktop sidecar is single-user and the storage root is
        // already in app-data, so local storage is safe and intentional here.
        .env("AUTUMN_STORAGE__ALLOW_LOCAL_IN_PRODUCTION", "true")
        // Clear any inherited attach URL so the sidecar owns its bundled Postgres
        // cluster rather than connecting to a stale or foreign database.
        // ManagedPostgresPoolProvider checks AUTUMN_MANAGED_PG_ATTACH_URL before
        // AUTUMN_MANAGED_PG_DATA_DIR and returns it without starting a local cluster;
        // an empty value is ignored by the provider.
        .env("AUTUMN_MANAGED_PG_ATTACH_URL", "")
        // Override the compile-time manifest dir so config loading reads from
        // the bundled resource dir on all machines, including the developer's
        // machine where the source tree still exists.  autumn's OsEnv::var
        // checks the process env before the #[autumn_web::main] baked-in path
        // when AUTUMN_MANIFEST_DIR is set, so this overrides both the CWD
        // fallback and the macro-injected compile-time value.
        .env(
            "AUTUMN_MANIFEST_DIR",
            resource_dir.to_string_lossy().as_ref(),
        )
        // Encrypted credentials (config/credentials/<profile>.toml.enc) are bundled
        // into resource_dir/config/credentials/ by the staging script.  Autumn loads
        // them automatically when AUTUMN_MANIFEST_DIR is set.  If the app reads
        // secrets via `config.credentials()`, provide the decryption key via either:
        //   • AUTUMN_MASTER_KEY env var (hex string), or
        //   • resource_dir/config/master.key file (hex string, one line).
        // The key file path is `<AUTUMN_MANIFEST_DIR>/config/master.key`; it must
        // be staged alongside the .toml.enc files (do NOT ship it in the installer —
        // deliver it out-of-band, e.g. via OS keychain or a secure download on first
        // launch).  Leaving both absent is safe when the app has no credentials store:
        // autumn returns CredentialsStore::default() when no .toml.enc file is found.
        // Clear any inherited Unix-socket config so the sidecar always binds
        // TCP on the loopback address the probe polls.  Without this, an
        // inherited AUTUMN_SERVER__UNIX_SOCKET or AUTUMN_SERVE_FORCE_UNIX_SOCKET
        // would make the sidecar bind a socket path while the TCP health probe
        // times out and exits.
        .env("AUTUMN_SERVER__UNIX_SOCKET", "")
        .env("AUTUMN_SERVE_FORCE_UNIX_SOCKET", "")
        // The sidecar binds only on loopback (AUTUMN_SERVER__HOST=127.0.0.1), but
        // if the app's production autumn.toml sets trusted_hosts.hosts to the
        // public domain, Autumn's trusted-host middleware would reject the webview's
        // Host: 127.0.0.1 requests with a 400.  Override to allow loopback hosts
        // unconditionally — the server is loopback-only so no external traffic
        // can reach it regardless of this setting.
        .env("AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS", "127.0.0.1,localhost")
        // Per-install signing secret so autumn's prod-mode JWT signing always
        // has a valid secret and doesn't abort before binding the HTTP port.
        .env("AUTUMN_SECURITY__SIGNING_SECRET", &signing_secret)
        // Profile selection: in a debug Tauri build (`cargo tauri dev`) the
        // stage-sidecar script always produces a --release sidecar (AUTUMN_IS_DEBUG=0
        // baked in → prod profile), but the developer expects dev config (dev DB,
        // relaxed security, etc.).  Set AUTUMN_ENV=dev so the release sidecar still
        // loads dev settings.  In a release Tauri build (`cargo tauri build`) clear
        // it instead so the sidecar's baked-in AUTUMN_IS_DEBUG=0 selects prod.
        .env("AUTUMN_ENV", if cfg!(debug_assertions) { "dev" } else { "" })
        // Clear AUTUMN_PROFILE regardless — it is the legacy spelling of AUTUMN_ENV
        // and should never be inherited from the calling shell environment.
        .env("AUTUMN_PROFILE", "")
        // Skip the prestop listener-drain on desktop: no load balancer drains
        // connections to the loopback-only sidecar, so the 5-second default grace
        // (server.prestop_grace_secs) only delays managed-Postgres cleanup past
        // the force-kill window.  Setting it to 0 lets on_shutdown hooks (including
        // ManagedPostgresPoolProvider::stop()) run immediately after SIGTERM.
        .env("AUTUMN_SERVER__PRESTOP_GRACE_SECS", "0")
        // The webview loads the app over plain HTTP (http://127.0.0.1:<port>).
        // Autumn's prod profile sets session.secure = true, which emits the
        // `Secure` attribute on session/CSRF/flash cookies.  Browsers never send
        // Secure cookies over non-HTTPS origins, so sessions, auth, and flash
        // messages silently stop working on installed release bundles.
        // Setting secure=false is safe here: the sidecar is loopback-only and
        // no external network can reach it; cookie confidentiality is not at risk.
        .env("AUTUMN_SESSION__SECURE", "false")
        // Clear one-off mode flags inherited from the calling environment.
        // If any of these are set, AppBuilder::run() enters a non-serving mode
        // (asset fingerprinting, route dump, task execution) and exits before
        // binding the HTTP port — leaving the TCP health probe to time out.
        .env("AUTUMN_BUILD_STATIC", "")
        .env("AUTUMN_DUMP_ROUTES", "")
        .env("AUTUMN_LIST_TASKS", "")
        .env("AUTUMN_RUN_TASK", "")
        // ── Opt-in: auto-migrate managed-Postgres on first launch ──────────────
        // If this app wires ManagedPostgresPoolProvider (desktop-bundled Postgres),
        // uncomment the next line so a fresh local cluster is migrated before the
        // first request arrives.  Leave it commented out when the sidecar connects
        // to a remote / shared database — enabling it there would run pending
        // migrations against that shared DB on every desktop client launch.
        // .env("AUTUMN_DATABASE__AUTO_MIGRATE_IN_PRODUCTION", "true")
        // ───────────────────────────────────────────────────────────────────────
        .spawn()?;
    *app.state::<SidecarHandle>().0.lock().unwrap() = Some(child);

    // 4. Poll for server readiness in a background thread so setup() returns immediately
    //    and the Tauri event loop starts.  Blocking here freezes the UI and can trigger
    //    OS ANR watchdogs on macOS and Windows.
    //    We probe GET /health — the cheap readiness endpoint autumn always registers.
    //    Even if [health].path is customised to a different path, the server still
    //    accepts the TCP connection and returns a fast HTTP response (e.g. 404), which
    //    starts with "HTTP/" and is enough to confirm the server is up and routing.
    //    Using /health instead of / avoids timing out against a slow app root handler
    //    (e.g. a DB-backed dashboard that queries before writing headers).
    let handle = app.handle().clone();
    std::thread::spawn(move || {
        // Build SocketAddr directly to avoid repeated string formatting and parse() panics.
        let addr = std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            port,
        );
        let poll_timeout = std::time::Duration::from_millis(200);
        let mut ready = false;
        // 1500 × 200 ms = 300 s total — matches autumn serve's READY_TIMEOUT_MANAGED_PG.
        // A first-launch managed-Postgres cluster must initialise (pg_ctl init) and then
        // run migrations before serving HTTP; on slow disks this can take several minutes.
        for _ in 0..1500 {
            // Fail fast: if the sidecar has already terminated (bad bundled config,
            // migration panic, missing runtime dependency, …) there is no point
            // waiting the full 300 s for a TCP connection that will never arrive.
            while let Ok(event) = rx.try_recv() {
                match event {
                    CommandEvent::Stdout(line) => {
                        if let Ok(s) = std::str::from_utf8(&line) {
                            print!("{}", s);
                        }
                    }
                    CommandEvent::Stderr(line) => {
                        if let Ok(s) = std::str::from_utf8(&line) {
                            eprint!("{}", s);
                        }
                    }
                    CommandEvent::Terminated(p) => {
                        eprintln!(
                            "[reddit-clone] Sidecar exited before becoming ready \
                             (code={:?}, signal={:?}) — aborting.",
                            p.code, p.signal
                        );
                        if let Some(c) = handle
                            .state::<SidecarHandle>()
                            .0
                            .lock()
                            .unwrap()
                            .take()
                        {
                            let _ = c.kill();
                        }
                        handle.exit(1);
                        return;
                    }
                    _ => {}
                }
            }
            if let Ok(mut stream) =
                std::net::TcpStream::connect_timeout(&addr, poll_timeout)
            {
                // Bound the read so a silent connection doesn't stall the loop.
                let _ = stream.set_read_timeout(Some(poll_timeout));
                use std::io::{Read, Write};
                let req =
                    "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
                if stream.write_all(req.as_bytes()).is_ok() {
                    let mut buf = [0u8; 8];
                    // Any valid HTTP response (200, 301, 401, 404, …) means the server
                    // is up and routing — accept the `HTTP/` prefix regardless of status.
                    if stream.read(&mut buf).is_ok() && buf.starts_with(b"HTTP/") {
                        ready = true;
                        break;
                    }
                }
            }
            std::thread::sleep(poll_timeout);
        }
        if !ready {
            eprintln!(
                "[reddit-clone] Server did not become ready within 300 s — exiting."
            );
            // No window has been created yet, so WindowEvent::Destroyed cannot
            // fire.  Kill the sidecar explicitly before exiting so no orphaned
            // server process is left behind.
            if let Some(child) = handle
                .state::<SidecarHandle>()
                .0
                .lock()
                .unwrap()
                .take()
            {
                let _ = child.kill();
            }
            handle.exit(1);
            return;
        }
        if let Err(e) = tauri::WebviewWindowBuilder::new(
            &handle,
            "main",
            tauri::WebviewUrl::External(
                format!("http://127.0.0.1:{port}").parse().unwrap(),
            ),
        )
        .title("reddit-clone")
        .inner_size(1200.0, 800.0)
        .build()
        {
            eprintln!("[reddit-clone] Failed to open window: {e}");
            // The window was never created so Destroyed cannot clean up; kill
            // the sidecar here too.
            if let Some(child) = handle
                .state::<SidecarHandle>()
                .0
                .lock()
                .unwrap()
                .take()
            {
                let _ = child.kill();
            }
            handle.exit(1);
        }
    });

    Ok(())
}

/// Generate a 32-byte random signing secret on first launch, persist it to
/// `{data_root}/signing_secret.txt`, and return it as a hex string.
/// Autumn requires a signing secret in prod mode to sign JWTs / session tokens.
/// Without this, the release sidecar calls `fail_fast_on_invalid_signing_secret`
/// and exits before binding the HTTP port, leaving the TCP probe to time out.
/// Returns `Err` (and aborts startup) on RNG failure so no predictable all-zero
/// secret is silently accepted.
fn load_or_generate_signing_secret(
    data_root: &std::path::Path,
) -> Result<String, Box<dyn std::error::Error>> {
    let path = data_root.join("signing_secret.txt");
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_owned();
        if s.len() >= 32 {
            // Harden permissions on an existing file in case it was created by
            // an older generated shell (0644) or manually without a mode flag.
            // Failure is non-fatal: log and continue with the existing secret.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) = std::fs::set_permissions(
                    &path,
                    std::fs::Permissions::from_mode(0o600),
                ) {
                    eprintln!(
                        "[reddit-clone] warning: could not restrict signing_secret.txt \
                         permissions: {e}"
                    );
                }
            }
            return Ok(s);
        }
    }
    let mut bytes = [0u8; 32];
    // Propagate RNG failure — an all-zero secret would be trivially guessable.
    getrandom::getrandom(&mut bytes)?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    // Write with restricted permissions: signing secrets must not be world-readable.
    // On Unix create the file with mode 0600; on other platforms use the default ACLs.
    // Propagate write failures: a disk-full or permission error must abort startup
    // rather than silently returning an ephemeral secret that rotates every launch.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true).create(true).truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(hex.as_bytes()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, &hex)?;
    }
    Ok(hex)
}
