//! Live cold-start onboarding measurement driver for `autumn dev-loop-bench
//! --cold-start` (issue #977).
//!
//! This module is the **orchestration** half of the cold-start benchmark: it
//! scaffolds a throwaway project, repoints its `autumn-web` dependency at the
//! repository's local source, compiles it from a cold target, starts the built
//! binary, and times the journey from `autumn new` to the first HTTP 200. All of
//! that is subprocess / TCP / filesystem I/O that cannot be exercised in unit
//! tests, so this file is excluded from coverage (see `codecov.yml`), mirroring
//! `build.rs` and `dev.rs`.
//!
//! Before the samples, one untimed step does the large network work of a
//! first build: toolchain install, first index update, crate downloads. The
//! report shows its time apart from the samples.
//!
//! The **pure**, unit-tested half — budgets, statistics, budget checking, report
//! building, and the dry-run budget table — lives in
//! [`crate::dev_loop_bench`]; this driver calls into it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::dev_loop_bench::{
    COLD_BUILD_ENV, ChangeClass, ColdStartPreparation, DbOutcome, budget_for,
    build_cold_start_report, cargo_executable_path, cold_start_fetch_args, emit_report,
    format_cold_start_budget_table, record_preparation,
};

/// Which scaffolded project shape to measure for cold start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdStartShape {
    /// No-database `hello` shape (the gated onboarding budget).
    Hello,
    /// Database-backed shape (informational only in this slice).
    Db,
}

const fn class_for(shape: ColdStartShape) -> ChangeClass {
    match shape {
        ColdStartShape::Hello => ChangeClass::ColdStartHello,
        ColdStartShape::Db => ChangeClass::ColdStartDb,
    }
}

/// Locate the workspace `autumn-web` crate so a throwaway project can be built
/// against the repository's actual source (a genuine clean compile) rather than
/// a possibly-unpublished crates.io version.
///
/// Honours the `AUTUMN_BENCH_AUTUMN_WEB_PATH` override, otherwise walks up from
/// the current directory looking for an `autumn/Cargo.toml` whose package is
/// `autumn-web`.
pub fn locate_autumn_web() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AUTUMN_BENCH_AUTUMN_WEB_PATH") {
        let pb = PathBuf::from(p);
        if pb.join("Cargo.toml").is_file() {
            return Some(pb);
        }
    }
    let cwd = std::env::current_dir().ok()?;
    for ancestor in cwd.ancestors() {
        let candidate = ancestor.join("autumn");
        let manifest = candidate.join("Cargo.toml");
        if manifest.is_file()
            && let Ok(s) = std::fs::read_to_string(&manifest)
            && s.contains("name = \"autumn-web\"")
        {
            return Some(candidate);
        }
    }
    None
}

/// Canonicalized workspace `autumn-web` path, resolved once per process.
///
/// The repo location is invariant, so the ancestor walk + file reads +
/// canonicalize run only on the first call and are cached for the rest. In the
/// cold-start gate the first call is the untimed preparation step, so no
/// sample's timed window includes them.
pub fn cached_autumn_web() -> Option<PathBuf> {
    static AUTUMN_WEB: OnceLock<Option<PathBuf>> = OnceLock::new();
    AUTUMN_WEB
        .get_or_init(|| locate_autumn_web().and_then(|p| std::fs::canonicalize(p).ok()))
        .clone()
}

/// Append a `[patch.crates-io]` override pointing `autumn-web` at local source.
///
/// The scaffolded `Cargo.toml` already declares its own (empty) `[workspace]`
/// table, so it is a standalone workspace root and a `[patch]` section is valid
/// there. The patch applies regardless of whether the dependency is the plain
/// `autumn-web = "x"` form or the daemon/`features` table form.
pub fn repoint_autumn_web(project_dir: &Path, autumn_web: &Path) -> Result<(), String> {
    let manifest = project_dir.join("Cargo.toml");
    let mut content =
        std::fs::read_to_string(&manifest).map_err(|e| format!("read Cargo.toml: {e}"))?;
    // `{:?}` quotes and escapes the path into a valid TOML basic string.
    let patch = format!(
        "\n[patch.crates-io]\nautumn-web = {{ path = {:?} }}\n",
        autumn_web.display().to_string()
    );
    content.push_str(&patch);
    std::fs::write(&manifest, content).map_err(|e| format!("write Cargo.toml: {e}"))?;
    Ok(())
}

/// The scaffold options of a cold-start shape.
fn options_for(shape: ColdStartShape) -> crate::new::GenerateOptions {
    match shape {
        // The DB-free daemon starter compiles and serves without Postgres.
        ColdStartShape::Hello => crate::new::GenerateOptions {
            with_daemon: true,
            ..Default::default()
        },
        // The database-backed shape uses the bundled managed-Postgres provider so
        // the app self-provisions a real Postgres (via `postgresql_embedded`),
        // runs migrations, and connects before serving — a genuine DB-backed
        // onboarding cold start with no external service required.
        ColdStartShape::Db => crate::new::GenerateOptions {
            with_bundled_pg: true,
            ..Default::default()
        },
    }
}

/// Scaffold a throwaway project of this shape and point it at the local
/// `autumn-web`. Returns the project directory.
fn scaffold(shape: ColdStartShape, parent: &Path, project_name: &str) -> Result<PathBuf, String> {
    let autumn_web = cached_autumn_web().ok_or_else(|| {
        "could not locate (or canonicalize) the workspace autumn-web crate; \
         set AUTUMN_BENCH_AUTUMN_WEB_PATH to its directory"
            .to_string()
    })?;
    // Quiet scaffold: keep stdout clean so `--cold-start --json` emits only the
    // JSON report.
    crate::new::generate_with_quiet(project_name, parent, options_for(shape))
        .map_err(|e| format!("autumn new failed: {e}"))?;
    let project_dir = parent.join(project_name);
    repoint_autumn_web(&project_dir, &autumn_web)?;
    Ok(project_dir)
}

/// Remove the inherited target settings and set [`COLD_BUILD_ENV`].
fn apply_cold_env(cargo: &mut Command) {
    cargo
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET");
    for (name, value) in COLD_BUILD_ENV {
        cargo.env(name, value);
    }
}

/// The `cargo fetch` command of the preparation step.
fn fetch_command(project_dir: &Path, target: Option<&str>) -> Command {
    let mut fetch = Command::new("cargo");
    fetch
        .args(cold_start_fetch_args(target))
        .current_dir(project_dir);
    apply_cold_env(&mut fetch);
    fetch
}

/// The timed `cargo build` command.
///
/// An explicit `--target` overrides a config-file `build.target` and keeps the
/// artifact host-runnable. Without a known host triple, Cargo's default
/// applies (the host, unless a config file overrides it).
fn build_command(project_dir: &Path, target: Option<&str>) -> Command {
    let mut build = Command::new("cargo");
    build
        .args(["build", "--message-format=json", "--target-dir"])
        .arg(project_dir.join("target"))
        .current_dir(project_dir);
    // No inherited target settings, no compiler wrapper (e.g. `sccache`), and
    // no incremental mode. See `COLD_BUILD_ENV`.
    apply_cold_env(&mut build);
    if let Some(triple) = target {
        build.arg("--target").arg(triple);
    }
    build
}

/// Do the toolchain install and the crate downloads of a first build before
/// the clock starts.
///
/// `cargo fetch` in a throwaway scaffold installs the toolchain that the
/// scaffold pins, updates the index and downloads the crates. It compiles
/// nothing and its project is deleted, so each sample still builds from cold.
/// A sample has no lockfile, so it still checks the index. `cargo fetch` runs
/// no build script, so a sample with bundled Postgres still downloads the
/// archive. See [`ColdStartPreparation`].
fn prepare_cold_start(shape: ColdStartShape) -> Result<ColdStartPreparation, String> {
    let tmp = tempfile::tempdir().map_err(|e| format!("create temp dir: {e}"))?;
    let start = Instant::now();
    let project_dir = scaffold(shape, tmp.path(), "coldstart_prepare")?;

    let output = fetch_command(&project_dir, cached_host_target_triple().as_deref())
        .output()
        .map_err(|e| format!("cargo fetch spawn failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo fetch failed for the scaffolded project:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // The scaffold's own `rust-toolchain.toml` selects this compiler.
    let rust_version = Command::new("rustc")
        .arg("--version")
        .current_dir(&project_dir)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|version| !version.is_empty());

    Ok(ColdStartPreparation {
        elapsed_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        rust_version,
    })
}

/// Measure a single cold-start journey end-to-end: scaffold a throwaway project,
/// compile it from a cold target, start it, and time until the first HTTP 200.
///
/// Returns the elapsed milliseconds from just before `autumn new` to the first
/// successful response on `/`.
fn measure_cold_start_once(shape: ColdStartShape) -> Result<u64, String> {
    let tmp = tempfile::tempdir().map_err(|e| format!("create temp dir: {e}"))?;
    let project_name = "coldstart_app";

    // Start the clock just before `autumn new` so the whole onboarding journey
    // (scaffold → first clean compile → boot → first 200) is captured.
    let start = Instant::now();

    let project_dir = scaffold(shape, tmp.path(), project_name)?;

    let bin = cold_build(&project_dir, project_name)?;

    // Pick the port the app binds and we poll, *after* the minutes-long build and
    // immediately before spawning, to minimise the window between reserving the
    // ephemeral port (the listener is dropped right away) and the child binding
    // it. A fresh port per sample also avoids colliding with a lingering
    // TIME_WAIT socket from the previous sample. An explicit AUTUMN_BENCH_PORT
    // override is honoured as-is.
    let port = match std::env::var("AUTUMN_BENCH_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
    {
        Some(p) => p,
        None => reserve_free_port()?,
    };

    let mut cmd = Command::new(&bin);
    cmd.current_dir(&project_dir);
    // Isolate the child from the surrounding environment: clear every inherited
    // `AUTUMN_*` var so app-mode settings can't change how the scaffolded app
    // boots or what it binds. Without this, e.g. `AUTUMN_ENV=prod` would trigger
    // production boot checks, `AUTUMN_BUILD_STATIC=1` would render static output
    // and exit instead of serving, and a stray `AUTUMN_SERVER__UNIX_SOCKET`
    // would make the app bind a socket instead of the TCP port we poll.
    for (key, _) in std::env::vars() {
        if key.starts_with("AUTUMN_") {
            cmd.env_remove(&key);
        }
    }
    // Then pin exactly the host+port we poll (env vars are the highest-precedence
    // config source, overriding autumn.toml).
    cmd.env("AUTUMN_SERVER__HOST", "127.0.0.1")
        .env("AUTUMN_SERVER__PORT", port.to_string());
    // For the DB shape, keep the bundled managed-Postgres cluster inside the
    // throwaway temp dir so the run is self-contained: otherwise the provider
    // defaults to `$HOME/.local/share/autumn/...` and leaves a full Postgres
    // data/extraction directory behind after the tempdir is removed.
    if matches!(shape, ColdStartShape::Db) {
        cmd.env(
            "AUTUMN_MANAGED_PG_DATA_DIR",
            project_dir.join("managed-pg-data"),
        );
    }
    cmd
        // Discard the app's own logs so they never interleave with the JSON
        // report on stdout; the harness reports progress on stderr separately.
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to start the built server binary: {e}"))?;

    // Poll the configured root route until the first 200 or the deadline.
    // The poll timeout is measured from *now* (after the multi-minute build), not
    // from `start`: a slow cold compile must not eat the boot-poll window and make
    // a server that boots fine look like a serve failure. `start` still drives the
    // measured duration recorded below.
    let url = format!("http://127.0.0.1:{port}/");
    let deadline = Instant::now() + Duration::from_millis(budget_for(class_for(shape)).max_ms * 2);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| format!("build http client: {e}"))?;

    let mut measured = None;
    while Instant::now() < deadline {
        // If the app exited (e.g. the chosen port is already in use → bind
        // failure → process exit), stop immediately so we never record a 200
        // served by some other process listening on this port.
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "scaffolded server exited before serving (exit: {status}); \
                 port {port} may already be in use"
            ));
        }
        // Accept only a 200 whose body proves it came from OUR scaffolded app:
        // its index page renders the project name. This rules out a 200 served by
        // some unrelated service that happens to answer on this port during the
        // child's boot-toward-bind-failure window.
        if let Ok(resp) = client.get(&url).send()
            && resp.status().is_success()
            && resp.text().is_ok_and(|body| body.contains(project_name))
        {
            measured = Some(u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX));
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Graceful shutdown so the app's on-shutdown hooks run — notably the
    // managed-Postgres provider stopping its supervised cluster on the DB shape,
    // which otherwise orphans Postgres processes/data between samples.
    stop_child(&mut child);

    measured.ok_or_else(|| "server did not return HTTP 200 before the deadline".to_string())
}

/// Compile the scaffolded project from a cold target and return its built binary.
///
/// Cold build into the project's own (empty) `target/`. Removes every inherited
/// Cargo target/triple override so the parent's warm cache is never reused, asks
/// Cargo for JSON artifact messages so we learn the *exact* built binary path,
/// and pins both the target dir and the host triple explicitly because
/// `env_remove` only neutralises the env-var forms — a `.cargo/config.toml`
/// `build.target-dir` / `build.target` (anywhere up the tree) could otherwise
/// redirect artifacts into a shared warm dir or cross-compile an unrunnable
/// binary. Compiler wrappers (e.g. `sccache`) are disabled so the measurement is
/// a genuine first clean compile.
fn cold_build(project_dir: &Path, project_name: &str) -> Result<PathBuf, String> {
    let output = build_command(project_dir, cached_host_target_triple().as_deref())
        .output()
        .map_err(|e| format!("cargo build spawn failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo build failed for the scaffolded project:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    cargo_executable_path(&output.stdout, project_name).ok_or_else(|| {
        "could not determine the built binary path from cargo's JSON output".to_string()
    })
}

/// Determine the host target triple via `rustc -vV`.
///
/// Used to pin the measured cold build to the host with an explicit `--target`,
/// so a config-file `build.target` cannot cross-compile an artifact the harness
/// then fails to execute. Returns `None` if `rustc` can't be run or parsed, in
/// which case the caller falls back to Cargo's default target.
fn host_target_triple() -> Option<String> {
    let out = Command::new("rustc").arg("-vV").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(|triple| triple.trim().to_string())
}

/// Host target triple, resolved once per process.
///
/// The triple is invariant, so the `rustc -vV` subprocess runs only on the first
/// call. In the gate that is the untimed preparation step, which keeps it out of
/// every sample's timed window.
fn cached_host_target_triple() -> Option<String> {
    static HOST_TRIPLE: OnceLock<Option<String>> = OnceLock::new();
    HOST_TRIPLE.get_or_init(host_target_triple).clone()
}

/// Reserve a currently-free TCP port on loopback.
///
/// Binds an ephemeral port, reads it, then drops the listener so the scaffolded
/// app can bind it. There is a tiny race between release and the child's bind,
/// but the per-iteration `try_wait()` and the response-body check make a
/// mis-attributed sample impossible even if the port is lost.
pub fn reserve_free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("failed to reserve a free port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("failed to read reserved port: {e}"))?
        .port();
    Ok(port)
}

/// Stop a scaffolded benchmark server, preferring a graceful SIGTERM so the
/// app runs its shutdown hooks (e.g. stopping a bundled Postgres cluster) before
/// falling back to SIGKILL if it does not exit in time.
///
/// Shared by every live driver that spawns a scaffolded app (cold-start,
/// overload) so they all shut down their child processes identically.
pub fn stop_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // Graceful SIGTERM first (via the shared helper) so the app's on_shutdown
        // hooks run — notably the managed-Postgres provider stopping its cluster.
        // Note: a *forced* fallback can still leave that Postgres behind, because
        // it `setsid`s into its own process group (see process::stop_postmaster);
        // the SIGTERM path is what reaps it.
        if let Err(e) = crate::process::signal_terminate(child.id()) {
            eprintln!("  Warning: failed to SIGTERM cold-start server: {e}");
        }
        if crate::process::wait_with_timeout(child, Duration::from_secs(10)).is_err() {
            crate::process::force_kill(child.id());
            let _ = child.wait();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Prepare, then run the cold-start measurement `runs` times. Returns the
/// preparation and the samples (ms).
fn measure_cold_start(
    shape: ColdStartShape,
    runs: u32,
) -> Result<(ColdStartPreparation, Vec<u64>), String> {
    eprintln!("  cold-start {shape:?} preparation (not in the samples)…");
    let preparation = prepare_cold_start(shape)?;
    eprintln!("    → {} ms", preparation.elapsed_ms);
    let mut samples = Vec::with_capacity(runs as usize);
    for i in 1..=runs {
        eprintln!("  cold-start {shape:?} run {i}/{runs}…");
        let ms = measure_cold_start_once(shape)?;
        eprintln!("    → {ms} ms");
        samples.push(ms);
    }
    Ok((preparation, samples))
}

/// Run the `autumn dev-loop-bench --cold-start` command.
///
/// In `--dry-run` mode it prints the cold-start budget table with no build or
/// server. Otherwise it measures the no-DB `hello` cold start (gated) and,
/// when `include_db` is set, the database-backed shape (informational).
// Mirrors the flag set of [`crate::dev_loop_bench::run`] (json /
// fail_on_regression / dry_run) plus the cold-start-only `include_db` toggle;
// grouping these into a struct would not improve the single CLI call site.
#[allow(clippy::fn_params_excessive_bools)]
pub fn run_cold_start(
    runs: u32,
    output: Option<&str>,
    json: bool,
    fail_on_regression: bool,
    dry_run: bool,
    include_db: bool,
) -> i32 {
    if dry_run {
        print!("{}", format_cold_start_budget_table(include_db));
        return 0;
    }

    // A measurement run with zero samples would publish an all-zero, "passing"
    // report without compiling or starting anything. Refuse it so the gate can
    // never go green on no data.
    if runs == 0 {
        eprintln!("Error: --runs must be at least 1 for a cold-start measurement.");
        return 1;
    }

    eprintln!(
        "autumn dev-loop-bench --cold-start: measuring onboarding cold start ({runs} run(s))"
    );
    eprintln!(
        "This scaffolds throwaway projects and compiles them from a cold target — \
         expect minutes per run.\n"
    );

    let (preparation, hello_samples) = match measure_cold_start(ColdStartShape::Hello, runs) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: cold-start (hello) measurement failed: {e}");
            return 1;
        }
    };

    let db = if include_db {
        match measure_cold_start(ColdStartShape::Db, runs) {
            Ok((_, s)) => DbOutcome::Measured(s),
            Err(e) => {
                // The DB shape is informational; a failure must not break the run,
                // but it is recorded in the report rather than silently dropped.
                eprintln!("Warning: informational DB cold-start measurement failed: {e}");
                DbOutcome::Failed(e)
            }
        }
    } else {
        DbOutcome::NotRequested
    };

    let mut report = build_cold_start_report(&hello_samples, &db);
    record_preparation(&mut report, &preparation);
    emit_report(&report, json, output, fail_on_regression)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// What a command does with one environment variable.
    #[derive(Debug, PartialEq, Eq)]
    enum EnvSetting {
        /// The command inherits the variable.
        Inherited,
        /// The command removes the variable.
        Removed,
        /// The command sets the variable to this value.
        Set(String),
    }

    fn env_of(cargo: &Command, name: &str) -> EnvSetting {
        cargo
            .get_envs()
            .find(|(key, _)| key.to_str() == Some(name))
            .map_or(EnvSetting::Inherited, |(_, value)| {
                value.map_or(EnvSetting::Removed, |v| {
                    EnvSetting::Set(v.to_string_lossy().into_owned())
                })
            })
    }

    fn args_of(cargo: &Command) -> Vec<String> {
        cargo
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn assert_cold_env(cargo: &Command) {
        for removed in [
            "CARGO_TARGET_DIR",
            "CARGO_BUILD_TARGET_DIR",
            "CARGO_BUILD_TARGET",
        ] {
            assert_eq!(env_of(cargo, removed), EnvSetting::Removed, "{removed}");
        }
        for wrapper in [
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "CARGO_BUILD_RUSTC_WRAPPER",
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
        ] {
            assert_eq!(
                env_of(cargo, wrapper),
                EnvSetting::Set(String::new()),
                "{wrapper}"
            );
        }
        assert_eq!(
            env_of(cargo, "CARGO_INCREMENTAL"),
            EnvSetting::Set("0".to_owned())
        );
        // The driver leaves every other variable as it is.
        assert_eq!(env_of(cargo, "CARGO_HOME"), EnvSetting::Inherited);
    }

    /// The preparation step downloads in the project directory. It compiles
    /// nothing and names no target directory.
    #[test]
    fn the_fetch_command_runs_in_the_project_with_the_cold_env() {
        let project = Path::new("/tmp/project");
        let fetch = fetch_command(project, Some("x86_64-unknown-linux-gnu"));
        assert_eq!(fetch.get_program(), "cargo");
        assert_eq!(
            args_of(&fetch),
            ["fetch", "--target", "x86_64-unknown-linux-gnu"]
        );
        assert_eq!(fetch.get_current_dir(), Some(project));
        assert_cold_env(&fetch);
    }

    #[test]
    fn the_build_command_uses_the_project_target_dir_and_the_cold_env() {
        let project = Path::new("/tmp/project");
        let build = build_command(project, Some("x86_64-unknown-linux-gnu"));
        let args = args_of(&build);
        assert_eq!(args[0], "build");
        let target_dir = project.join("target").to_string_lossy().into_owned();
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--target-dir" && pair[1] == target_dir),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--target" && pair[1] == "x86_64-unknown-linux-gnu"),
            "{args:?}"
        );
        assert_eq!(build.get_current_dir(), Some(project));
        assert_cold_env(&build);
        // Without a known host triple, the build names no target.
        let args = args_of(&build_command(project, None));
        assert!(!args.iter().any(|arg| arg == "--target"), "{args:?}");
    }

    #[test]
    fn class_for_maps_each_shape_to_its_change_class() {
        assert_eq!(
            class_for(ColdStartShape::Hello),
            ChangeClass::ColdStartHello
        );
        assert_eq!(class_for(ColdStartShape::Db), ChangeClass::ColdStartDb);
    }

    #[test]
    fn locate_autumn_web_honours_env_override() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("Cargo.toml"), "name = \"autumn-web\"\n")
            .expect("write manifest");
        let found = temp_env::with_var(
            "AUTUMN_BENCH_AUTUMN_WEB_PATH",
            Some(tmp.path().as_os_str()),
            locate_autumn_web,
        );
        assert_eq!(found.as_deref(), Some(tmp.path()));
    }

    #[test]
    fn repoint_autumn_web_appends_patch_section() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path();
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"coldstart_app\"\n",
        )
        .expect("write manifest");
        let web = tmp.path().join("autumn");
        repoint_autumn_web(project, &web).expect("repoint should succeed");
        let content = std::fs::read_to_string(project.join("Cargo.toml")).expect("read back");
        assert!(content.contains("[patch.crates-io]"));
        assert!(content.contains("autumn-web = { path ="));
    }

    #[test]
    fn reserve_free_port_returns_nonzero() {
        let port = reserve_free_port().expect("should reserve a port");
        assert!(port > 0);
    }

    // ── run_cold_start ────────────────────────────────────────────────────

    #[test]
    fn run_cold_start_dry_run_returns_zero() {
        assert_eq!(run_cold_start(1, None, false, false, true, false), 0);
        assert_eq!(run_cold_start(1, None, true, false, true, true), 0);
    }

    #[test]
    fn run_cold_start_rejects_zero_runs() {
        // --runs 0 must not publish an all-zero "passing" report without
        // measuring anything; it returns a non-zero exit instead.
        let exit = run_cold_start(0, None, false, false, false, false);
        assert_eq!(exit, 1, "zero runs must be rejected");
    }

    #[test]
    fn run_cold_start_dry_run_ignores_zero_runs() {
        // Dry-run never measures, so runs == 0 is harmless there.
        assert_eq!(run_cold_start(0, None, false, false, true, false), 0);
    }

    // ── live driver (slow: compiles a fresh project) ──────────────────────

    #[test]
    #[ignore = "compiles a throwaway project from a cold target; run with --ignored"]
    fn cold_start_live_hello_measures_a_positive_duration() {
        let ms = measure_cold_start_once(ColdStartShape::Hello)
            .expect("live cold-start measurement should succeed");
        assert!(ms > 0, "measured cold-start duration must be positive");
    }
}
