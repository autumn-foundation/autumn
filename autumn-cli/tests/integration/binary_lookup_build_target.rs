//! Binary lookup when Cargo builds for a target triple (issue #2638).
//!
//! With `build.target`, Cargo writes the binary to
//! `<target>/<triple>/<profile>/<bin>`, not `<target>/<profile>/<bin>`. Each
//! command that builds the app and then runs it must find the binary there.
//!
//! The fixture app has no dependencies, so each build takes about one second.
//! When it starts, it writes its `AUTUMN_*` mode variables to a log file. The
//! log shows that the command found and started the binary.

use std::path::PathBuf;
use std::process::{Command, Output};

const FIXTURE: &str = "lookup_fixture";
const LOG_ENV: &str = "LOOKUP_FIXTURE_LOG";

const FIXTURE_MAIN: &str = r#"use std::io::Write;

fn main() {
    let mut modes: Vec<String> = std::env::vars()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with("AUTUMN_DUMP_") || key == "AUTUMN_BUILD_STATIC")
        .collect();
    modes.sort();
    if let Ok(log) = std::env::var("LOOKUP_FIXTURE_LOG") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .expect("open launch log");
        writeln!(file, "{}", modes.join(",")).expect("write launch log");
    }
    if std::env::var_os("AUTUMN_DUMP_ROUTES").is_some() {
        print!("[]");
    }
}
"#;

/// Where Cargo gets the target triple from.
enum TargetSource {
    /// No triple. Cargo uses `<target>/<profile>/`.
    None,
    /// `CARGO_BUILD_TARGET`.
    Env,
    /// `build.target` in `<app>/.cargo/config.toml`.
    AppConfig,
    /// `build.target` in `<app>/../.cargo/config.toml`.
    ParentConfig,
}

/// A fixture app in its own temporary directory.
struct Fixture {
    _root: tempfile::TempDir,
    app: PathBuf,
    log: PathBuf,
    triple: String,
    source: TargetSource,
}

impl Fixture {
    fn new(source: TargetSource) -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let app = root.path().join("app");
        std::fs::create_dir_all(app.join("src")).expect("mkdir src");
        std::fs::write(
            app.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{FIXTURE}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
                 [workspace]\n"
            ),
        )
        .expect("write Cargo.toml");
        std::fs::write(app.join("src/main.rs"), FIXTURE_MAIN).expect("write main.rs");

        let triple = host_triple();
        let config_dir = match source {
            TargetSource::AppConfig => Some(app.clone()),
            TargetSource::ParentConfig => Some(root.path().to_path_buf()),
            TargetSource::None | TargetSource::Env => None,
        };
        if let Some(dir) = config_dir {
            std::fs::create_dir_all(dir.join(".cargo")).expect("mkdir .cargo");
            std::fs::write(
                dir.join(".cargo/config.toml"),
                format!("[build]\ntarget = \"{triple}\"\n"),
            )
            .expect("write config.toml");
        }

        let log = root.path().join("launch.log");
        Self {
            _root: root,
            app,
            log,
            triple,
            source,
        }
    }

    /// Run `autumn <args>` in the fixture app.
    fn autumn(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_autumn"));
        cmd.args(args)
            .current_dir(&self.app)
            .env(LOG_ENV, &self.log)
            // Keep the build in the fixture's own `target/`.
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET");
        if matches!(self.source, TargetSource::Env) {
            cmd.env("CARGO_BUILD_TARGET", &self.triple);
        }
        cmd.output().expect("run autumn")
    }

    /// The mode lists the fixture logged, one per start.
    fn launches(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The fixture binary path for `profile`, with or without the triple.
    fn binary(&self, with_triple: bool, profile: &str) -> PathBuf {
        let mut path = self.app.join("target");
        if with_triple {
            path.push(&self.triple);
        }
        path.push(profile);
        path.push(FIXTURE);
        if cfg!(windows) {
            path.set_extension("exe");
        }
        path
    }
}

/// The host triple from `rustc -vV`. The fixture must run on this machine.
fn host_triple() -> String {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .expect("run rustc -vV");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("host line in rustc -vV")
        .trim()
        .to_owned()
}

fn describe(output: &Output) -> String {
    format!(
        "exit={:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Run `autumn routes` and check that it finds, starts and reads the binary.
fn assert_routes_ok(fixture: &Fixture) {
    let output = fixture.autumn(&["routes", "--format", "json"]);
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "[]",
        "{}",
        describe(&output)
    );
}

/// Check that Cargo used the triple directory, so the test is not a no-op.
fn assert_built_under_triple(fixture: &Fixture, profile: &str) {
    let with_triple = fixture.binary(true, profile);
    let without = fixture.binary(false, profile);
    assert!(with_triple.is_file(), "missing {}", with_triple.display());
    assert!(!without.exists(), "unexpected {}", without.display());
}

#[test]
fn routes_finds_binary_without_a_target_triple() {
    let fixture = Fixture::new(TargetSource::None);
    assert_routes_ok(&fixture);
    assert!(fixture.binary(false, "debug").is_file());
}

#[test]
fn routes_finds_binary_with_cargo_build_target_env() {
    let fixture = Fixture::new(TargetSource::Env);
    assert_routes_ok(&fixture);
    assert_built_under_triple(&fixture, "debug");
}

#[test]
fn routes_finds_binary_with_build_target_in_app_config() {
    let fixture = Fixture::new(TargetSource::AppConfig);
    assert_routes_ok(&fixture);
    assert_built_under_triple(&fixture, "debug");
}

#[test]
fn routes_finds_binary_with_build_target_in_parent_config() {
    let fixture = Fixture::new(TargetSource::ParentConfig);
    assert_routes_ok(&fixture);
    assert_built_under_triple(&fixture, "debug");
}

/// A second run compiles nothing. Cargo must still report the path.
#[test]
fn routes_finds_binary_with_build_target_when_build_is_fresh() {
    let fixture = Fixture::new(TargetSource::Env);
    assert_routes_ok(&fixture);
    assert_routes_ok(&fixture);
    assert_eq!(fixture.launches().len(), 2, "{:?}", fixture.launches());
}

/// The commands that build the app and then run it, with the mode each sets.
const BUILD_AND_RUN: &[(&[&str], &str)] = &[
    (&["routes"], "AUTUMN_DUMP_ROUTES"),
    (&["graph", "show"], "AUTUMN_DUMP_GRAPH"),
    (&["agents", "manifest"], "AUTUMN_DUMP_AGENT_AUTHORITY"),
    (&["data-flow"], "AUTUMN_DUMP_DATA_FLOW"),
    (&["openapi", "export"], "AUTUMN_DUMP_OPENAPI"),
    (&["calibrate"], "AUTUMN_DUMP_ROUTES"),
    (&["build", "--debug"], "AUTUMN_BUILD_STATIC"),
];

/// Check that each [`BUILD_AND_RUN`] command starts the binary it built.
///
/// Most of these commands then stop, because the fixture prints no manifest.
/// This checks only that the binary started in the correct mode.
fn assert_every_command_starts_binary(source: TargetSource) {
    let fixture = Fixture::new(source);
    for (args, mode) in BUILD_AND_RUN {
        let before = fixture.launches().len();
        let output = fixture.autumn(args);
        let launches = fixture.launches();
        assert!(
            launches[before..]
                .iter()
                .any(|line| line.split(',').any(|m| m == *mode)),
            "`autumn {}` did not start the binary in mode {mode}; log {launches:?}\n{}",
            args.join(" "),
            describe(&output)
        );
    }
    // `calibrate` builds release; the others build debug.
    assert_built_under_triple(&fixture, "debug");
    assert_built_under_triple(&fixture, "release");
}

#[test]
fn every_command_starts_binary_with_cargo_build_target_env() {
    assert_every_command_starts_binary(TargetSource::Env);
}

#[test]
fn every_command_starts_binary_with_build_target_in_app_config() {
    assert_every_command_starts_binary(TargetSource::AppConfig);
}

#[test]
fn every_command_starts_binary_with_build_target_in_parent_config() {
    assert_every_command_starts_binary(TargetSource::ParentConfig);
}
