//! `autumn contracts`: publish a release's wire contract and check the next
//! release against it.
//!
//! `export` runs a dedicated `cargo check` pass and joins the descriptors it
//! writes into one sorted bundle. `check` compares the current build's bundle
//! with a deployed one, in both deploy windows, and names a safe deploy order
//! when only one exists. See `docs/guide/wire-contracts.md`.

pub mod bundle;
pub mod compat;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use bundle::Bundle;
use compat::Order;

use crate::routes::CargoFeatures;

/// Which crates the export pass checks.
#[derive(Debug, Clone, Default)]
pub struct PassOptions {
    /// `-p` packages. Empty means the whole workspace.
    pub packages: Vec<String>,
    /// Feature flags for `cargo check`.
    pub features: CargoFeatures,
}

impl PassOptions {
    /// The `cargo check` arguments for this selection.
    #[must_use]
    pub fn cargo_args(&self) -> Vec<String> {
        let mut args = vec!["check".to_owned(), "--quiet".to_owned()];
        if self.packages.is_empty() {
            args.push("--workspace".to_owned());
        }
        for package in &self.packages {
            args.push("--package".to_owned());
            args.push(package.clone());
        }
        args.extend(self.features.to_args());
        args
    }
}

/// Options for `autumn contracts export`.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Where to write the bundle. `None` writes to stdout.
    pub out: Option<PathBuf>,
    /// The crates to check.
    pub pass: PassOptions,
}

/// Options for `autumn contracts check`.
#[derive(Debug, Clone)]
pub struct CheckOptions {
    /// The deployed release: a bundle file, an `http(s)` URL, or a git ref.
    pub against: String,
    /// A bundle for the current build. `None` runs the export pass.
    pub current: Option<PathBuf>,
    /// The deploy order the operator plans.
    pub order: Order,
    /// The crates to check.
    pub pass: PassOptions,
}

/// Where the deployed bundle comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Against {
    /// A bundle file on disk.
    File(PathBuf),
    /// A bundle at an `http://` or `https://` URL.
    Url(String),
    /// A git ref: export a bundle from that commit.
    GitRef(String),
}

impl Against {
    /// Classify an `--against` value. A file that exists wins, then a URL,
    /// then a git ref.
    #[must_use]
    pub fn classify(value: &str, is_file: impl Fn(&Path) -> bool) -> Self {
        if is_file(Path::new(value)) {
            Self::File(PathBuf::from(value))
        } else if value.starts_with("https://") || value.starts_with("http://") {
            Self::Url(value.to_owned())
        } else {
            Self::GitRef(value.to_owned())
        }
    }
}

/// Run `autumn contracts export`.
pub fn run_export(opts: &ExportOptions) {
    let bundle = export_pass(Path::new("."), &opts.pass).unwrap_or_else(|e| fail(&e));
    let bytes = bundle.to_bytes();
    match &opts.out {
        Some(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .unwrap_or_else(|e| fail(&format!("create {}: {e}", parent.display())));
            }
            std::fs::write(path, &bytes)
                .unwrap_or_else(|e| fail(&format!("write {}: {e}", path.display())));
            eprintln!(
                "Wrote {} endpoint(s) and {} call(s) to {}.",
                bundle.endpoints.len(),
                bundle.callers.len(),
                path.display()
            );
        }
        None => {
            use std::io::Write as _;
            std::io::stdout()
                .write_all(&bytes)
                .unwrap_or_else(|e| fail(&format!("write stdout: {e}")));
        }
    }
}

/// Run `autumn contracts check`. Exits with the report's code.
pub fn run_check(opts: &CheckOptions) {
    let deployed = load_against(&opts.against, &opts.pass).unwrap_or_else(|e| fail(&e));
    let current = match &opts.current {
        Some(path) => read_bundle(path).unwrap_or_else(|e| fail(&e)),
        None => export_pass(Path::new("."), &opts.pass).unwrap_or_else(|e| fail(&e)),
    };
    let report = compat::compare(&deployed, &current);
    print!("{}", report.render(opts.order));
    std::process::exit(report.exit_code(opts.order));
}

fn fail(message: &str) -> ! {
    eprintln!("\u{2717} {message}");
    std::process::exit(1);
}

/// Read and parse a bundle file.
fn read_bundle(path: &Path) -> Result<Bundle, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Bundle::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Load the deployed bundle from a file, a URL, or a git ref.
fn load_against(value: &str, pass: &PassOptions) -> Result<Bundle, String> {
    match Against::classify(value, Path::is_file) {
        Against::File(path) => read_bundle(&path),
        Against::Url(url) => fetch_bundle(&url),
        Against::GitRef(git_ref) => export_at_ref(&git_ref, pass),
    }
}

/// Download a bundle.
fn fetch_bundle(url: &str) -> Result<Bundle, String> {
    let response = reqwest::blocking::get(url).map_err(|e| format!("fetch {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("fetch {url}: HTTP {status}"));
    }
    let bytes = response.bytes().map_err(|e| format!("fetch {url}: {e}"))?;
    Bundle::from_slice(&bytes).map_err(|e| format!("{url}: {e}"))
}

/// Export a bundle from another commit, in a temporary git worktree.
///
/// The pass uses this workspace's target directory, so the dependencies
/// compile once for both commits.
fn export_at_ref(git_ref: &str, pass: &PassOptions) -> Result<Bundle, String> {
    let commit = git(
        Path::new("."),
        &["rev-parse", "--verify", &format!("{git_ref}^{{commit}}")],
    )
    .map_err(|e| format!("`{git_ref}` is not a bundle file, an http(s) URL, or a git ref: {e}"))?;
    let top = PathBuf::from(git(Path::new("."), &["rev-parse", "--show-toplevel"])?);
    let here = std::env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let here = here.canonicalize().unwrap_or(here);
    let top = top.canonicalize().unwrap_or(top);
    let relative = here
        .strip_prefix(&top)
        .unwrap_or(Path::new(""))
        .to_path_buf();
    let target_dir = target_dir(Path::new("."))?;

    let scratch = tempfile::tempdir().map_err(|e| format!("temp dir: {e}"))?;
    let tree = scratch.path().join("tree");
    let tree_text = tree.to_string_lossy().into_owned();
    git(
        &top,
        &[
            "worktree", "add", "--detach", "--quiet", &tree_text, &commit,
        ],
    )?;
    let result = with_target_dir(&tree.join(&relative), pass, &target_dir);
    // Remove the worktree whatever the pass did.
    let _ = git(&top, &["worktree", "remove", "--force", &tree_text]);
    result.map_err(|e| format!("export at `{git_ref}`: {e}"))
}

fn with_target_dir(
    workspace: &Path,
    pass: &PassOptions,
    target_dir: &Path,
) -> Result<Bundle, String> {
    run_pass(workspace, pass, Some(target_dir))
}

/// Run git and return its trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("run git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The workspace's target directory, from `cargo metadata`.
fn target_dir(workspace: &Path) -> Result<PathBuf, String> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err("cargo metadata failed".to_owned());
    }
    let meta: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata: {e}"))?;
    meta.get("target_directory")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| "cargo metadata has no `target_directory`".to_owned())
}

/// Export the bundle of the workspace at `workspace`.
fn export_pass(workspace: &Path, pass: &PassOptions) -> Result<Bundle, String> {
    run_pass(workspace, pass, None)
}

/// The dedicated pass: `cargo check` with a new, empty contract directory.
///
/// The wire macros read `AUTUMN_CONTRACT_DIR` through `option_env!`, so cargo
/// rebuilds every crate that uses them when the value changes. A new
/// directory each time makes every wire macro run and write its descriptor,
/// so a cached build cannot leave a descriptor out. The pass also turns off a
/// rustc wrapper such as sccache: a wrapper that replays a cached compile does
/// not run the macros.
fn run_pass(workspace: &Path, pass: &PassOptions, target: Option<&Path>) -> Result<Bundle, String> {
    let out = tempfile::Builder::new()
        .prefix("autumn-contracts-")
        .tempdir()
        .map_err(|e| format!("temp dir: {e}"))?;
    let mut cmd = Command::new("cargo");
    cmd.args(pass.cargo_args())
        .current_dir(workspace)
        .env("AUTUMN_CONTRACT_DIR", out.path())
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(target) = target {
        cmd.env("CARGO_TARGET_DIR", target);
    }
    eprintln!(
        "Building the wire contract (cargo {}).",
        pass.cargo_args().join(" ")
    );
    let status = cmd.status().map_err(|e| format!("run cargo: {e}"))?;
    if !status.success() {
        return Err("cargo check failed; the contract needs a build that compiles".to_owned());
    }
    Bundle::from_dir(out.path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_exists_is_a_bundle() {
        assert_eq!(
            Against::classify("deployed.json", |_| true),
            Against::File(PathBuf::from("deployed.json"))
        );
    }

    #[test]
    fn an_http_value_is_a_url() {
        assert_eq!(
            Against::classify("https://example.com/c.json", |_| false),
            Against::Url("https://example.com/c.json".to_owned())
        );
        assert_eq!(
            Against::classify("http://localhost/c.json", |_| false),
            Against::Url("http://localhost/c.json".to_owned())
        );
    }

    #[test]
    fn anything_else_is_a_git_ref() {
        assert_eq!(
            Against::classify("origin/main", |_| false),
            Against::GitRef("origin/main".to_owned())
        );
    }

    #[test]
    fn the_pass_checks_the_workspace_unless_packages_are_named() {
        let all = PassOptions::default();
        assert!(all.cargo_args().contains(&"--workspace".to_owned()));
        let some = PassOptions {
            packages: vec!["mesh-catalog".to_owned()],
            features: CargoFeatures::default(),
        };
        let args = some.cargo_args();
        assert!(!args.contains(&"--workspace".to_owned()));
        assert!(args.windows(2).any(|w| w == ["--package", "mesh-catalog"]));
    }
}
