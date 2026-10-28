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
    root: tempfile::TempDir,
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
            root,
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
            // An empty `CARGO_HOME` stops a user config from adding a triple
            // or a target directory. The fixture has no dependencies.
            .env("CARGO_HOME", self.root.path().join("cargo-home"))
            .env("CARGO_TARGET_DIR", self.app.join("target"))
            .env_remove("CARGO_BUILD_TARGET_DIR")
            // Only `source` sets the triple.
            .env_remove("CARGO_BUILD_TARGET");
        if matches!(self.source, TargetSource::Env) {
            cmd.env("CARGO_BUILD_TARGET", &self.triple);
        }
        cmd.output().expect("run autumn")
    }

    /// The logged modes. Each line is one start of the fixture.
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

/// The host triple from `rustc -vV`. Only a binary for it can start here.
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

/// Run `autumn routes`. Check that it starts the binary and reads its output.
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

/// Check the directory Cargo used. If it is not the expected one, the test
/// proves nothing.
fn assert_built_where_expected(fixture: &Fixture, profile: &str) {
    let with_triple = !matches!(fixture.source, TargetSource::None);
    let expected = fixture.binary(with_triple, profile);
    let other = fixture.binary(!with_triple, profile);
    assert!(expected.is_file(), "missing {}", expected.display());
    assert!(!other.exists(), "unexpected {}", other.display());
}

#[test]
fn routes_finds_binary_without_a_target_triple() {
    let fixture = Fixture::new(TargetSource::None);
    assert_routes_ok(&fixture);
    assert_built_where_expected(&fixture, "debug");
}

#[test]
fn routes_finds_binary_with_cargo_build_target_env() {
    let fixture = Fixture::new(TargetSource::Env);
    assert_routes_ok(&fixture);
    assert_built_where_expected(&fixture, "debug");
}

#[test]
fn routes_finds_binary_with_build_target_in_app_config() {
    let fixture = Fixture::new(TargetSource::AppConfig);
    assert_routes_ok(&fixture);
    assert_built_where_expected(&fixture, "debug");
}

#[test]
fn routes_finds_binary_with_build_target_in_parent_config() {
    let fixture = Fixture::new(TargetSource::ParentConfig);
    assert_routes_ok(&fixture);
    assert_built_where_expected(&fixture, "debug");
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
/// Most of these commands then fail, because the fixture prints no manifest.
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
    // `calibrate` builds the release profile. The others build the debug profile.
    assert_built_where_expected(&fixture, "debug");
    assert_built_where_expected(&fixture, "release");
}

#[test]
fn every_command_starts_binary_without_a_target_triple() {
    assert_every_command_starts_binary(TargetSource::None);
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
