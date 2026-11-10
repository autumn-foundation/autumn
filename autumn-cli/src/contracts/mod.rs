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

use autumn_macros_support::wire::Crates;
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
    /// Join the descriptors in this directory, with no build and no
    /// dependency graph. `None` runs the export pass.
    pub from: Option<PathBuf>,
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
    /// Accept an `http://` URL for `against`.
    pub allow_http: bool,
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
    let bundle = opts
        .from
        .as_deref()
        .map_or_else(|| export_pass(Path::new("."), &opts.pass), Bundle::from_dir)
        .unwrap_or_else(|e| fail(&e));
    let bytes = bundle.to_bytes();
    let Some(path) = &opts.out else {
        use std::io::Write as _;
        std::io::stdout()
            .write_all(&bytes)
            .unwrap_or_else(|e| fail(&format!("write stdout: {e}")));
        return;
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| fail(&format!("create {}: {e}", parent.display())));
    }
    std::fs::write(path, &bytes)
        .unwrap_or_else(|e| fail(&format!("write {}: {e}", path.display())));
    for unresolved in &bundle.unresolved {
        eprintln!("warning: event consumer not in the bundle: {unresolved}");
    }
    eprintln!(
        "Wrote {} endpoint(s), {} call(s), {} event(s) and {} event consumer(s) to {}.",
        bundle.endpoints.len(),
        bundle.callers.len(),
        bundle.events.len(),
        bundle.listeners.len(),
        path.display()
    );
}

/// Run `autumn contracts check`. Exits with the report's code.
pub fn run_check(opts: &CheckOptions) {
    let deployed =
        load_against(&opts.against, opts.allow_http, &opts.pass).unwrap_or_else(|e| fail(&e));
    let current = opts
        .current
        .as_deref()
        .map_or_else(|| export_pass(Path::new("."), &opts.pass), read_bundle)
        .unwrap_or_else(|e| fail(&e));
    let report = compat::compare(&deployed, &current);
    print!("{}", report.render(opts.order));
    std::process::exit(report.exit_code(opts.order));
}

/// Exit code: the tool could not run the check. It is not a verdict.
pub const EXIT_ERROR: i32 = 4;

fn fail(message: &str) -> ! {
    eprintln!("\u{2717} {message}");
    std::process::exit(EXIT_ERROR);
}

/// The largest bundle a URL may serve.
const MAX_BUNDLE_BYTES: u64 = 16 * 1024 * 1024;

/// Read and parse a bundle file.
fn read_bundle(path: &Path) -> Result<Bundle, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Bundle::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Load the deployed bundle from a file, a URL, or a git ref.
fn load_against(value: &str, allow_http: bool, pass: &PassOptions) -> Result<Bundle, String> {
    match Against::classify(value, Path::is_file) {
        Against::File(path) => read_bundle(&path),
        Against::Url(url) => fetch_bundle(&url, allow_http),
        Against::GitRef(git_ref) => export_at_ref(&git_ref, pass),
    }
}

/// Download a bundle.
///
/// An `http://` URL needs `--allow-http`: a party in the path could serve a
/// bundle with no callers, and every change would then read as safe. A
/// redirect may not go from `https` to `http`.
fn fetch_bundle(url: &str, allow_http: bool) -> Result<Bundle, String> {
    use std::io::Read as _;
    if url.starts_with("http://") && !allow_http {
        return Err(format!(
            "refusing plain-http bundle URL {url}; use https, or pass --allow-http"
        ));
    }
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            let downgrade = attempt.url().scheme() == "http"
                && attempt.previous().iter().any(|u| u.scheme() == "https");
            if downgrade {
                attempt.error("redirect from https to http")
            } else if attempt.previous().len() > 5 {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let response = client
        .get(url)
        .send()
        .map_err(|e| format!("fetch {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("fetch {url}: HTTP {status}"));
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_BUNDLE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("fetch {url}: {e}"))?;
    if bytes.len() as u64 > MAX_BUNDLE_BYTES {
        return Err(format!(
            "fetch {url}: the bundle is larger than {MAX_BUNDLE_BYTES} bytes"
        ));
    }
    Bundle::from_slice(&bytes).map_err(|e| format!("{url}: {e}"))
}

/// A temporary git worktree. Drop removes it, also after an error or a panic.
struct Worktree {
    top: PathBuf,
    path: PathBuf,
    _scratch: tempfile::TempDir,
}

impl Worktree {
    fn add(top: &Path, commit: &str) -> Result<Self, String> {
        let scratch = tempfile::tempdir().map_err(|e| format!("temp dir: {e}"))?;
        let path = scratch.path().join("tree");
        let text = path.to_string_lossy().into_owned();
        git(
            top,
            &["worktree", "add", "--detach", "--quiet", &text, commit],
        )?;
        Ok(Self {
            top: top.to_path_buf(),
            path,
            _scratch: scratch,
        })
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let text = self.path.to_string_lossy().into_owned();
        if git(&self.top, &["worktree", "remove", "--force", &text]).is_err() {
            let _ = std::fs::remove_dir_all(&self.path);
            let _ = git(&self.top, &["worktree", "prune"]);
        }
    }
}

/// Export a bundle from another commit, in a temporary git worktree.
///
/// The pass uses its own target directory under this workspace's target
/// directory. Cargo names a path crate's artifacts by its path relative to the
/// workspace root, so a shared directory would mix the two commits' crates.
/// The directory stays, so dependencies compile only once.
fn export_at_ref(git_ref: &str, pass: &PassOptions) -> Result<Bundle, String> {
    let commit = git(
        Path::new("."),
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{git_ref}^{{commit}}"),
        ],
    )
    .map_err(|e| format!("`{git_ref}` is not a bundle file, an http(s) URL, or a git ref: {e}"))?;
    let top = PathBuf::from(git(Path::new("."), &["rev-parse", "--show-toplevel"])?);
    let here = std::env::current_dir().map_err(|e| format!("current dir: {e}"))?;
    let here = here.canonicalize().unwrap_or(here);
    let top = top.canonicalize().unwrap_or(top);
    let relative = here
        .strip_prefix(&top)
        .unwrap_or_else(|_| Path::new(""))
        .to_path_buf();
    let target = target_dir(&metadata(Path::new("."))?)?.join("autumn-contracts-ref");

    let tree = Worktree::add(&top, &commit)?;
    let workspace = tree.path.join(&relative);
    if !workspace.is_dir() {
        return Err(format!(
            "`{}` does not exist at `{git_ref}`",
            relative.display()
        ));
    }
    let pass = packages_at(&workspace, pass, git_ref)?;
    run_pass(&workspace, &pass, Some(&target)).map_err(|e| format!("export at `{git_ref}`: {e}"))
}

/// Keep only the `-p` packages that exist in `workspace`. A release that adds
/// a service has no such package at an older commit.
fn packages_at(workspace: &Path, pass: &PassOptions, git_ref: &str) -> Result<PassOptions, String> {
    if pass.packages.is_empty() {
        return Ok(pass.clone());
    }
    let meta = metadata(workspace)?;
    let names: Vec<&str> = meta
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .map(|packages| {
            packages
                .iter()
                .filter_map(|p| p.get("name").and_then(serde_json::Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let (kept, dropped): (Vec<String>, Vec<String>) = pass
        .packages
        .iter()
        .cloned()
        .partition(|p| names.contains(&p.as_str()));
    if !dropped.is_empty() {
        eprintln!(
            "Not at `{git_ref}`, so not checked there: {}.",
            dropped.join(", ")
        );
    }
    if kept.is_empty() {
        return Err(format!("none of the -p packages exist at `{git_ref}`"));
    }
    Ok(PassOptions {
        packages: kept,
        features: pass.features.clone(),
    })
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

/// `cargo metadata --no-deps` for the workspace at `workspace`.
fn metadata(workspace: &Path) -> Result<serde_json::Value, String> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(workspace)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("run cargo metadata: {e}"))?;
    if !out.status.success() {
        return Err("cargo metadata failed".to_owned());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata: {e}"))
}

/// The dependency graph of the packages in `cargo metadata --no-deps`
/// output: normal and build dependencies, with the name each one has in
/// source. A dev-dependency is left out: the pass checks no test.
fn crates_of(meta: &serde_json::Value) -> Crates {
    let mut crates = Crates::default();
    let packages = meta
        .get("packages")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for package in packages {
        let Some(name) = package.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let deps = package
            .get("dependencies")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for dep in deps {
            if dep.get("kind").and_then(serde_json::Value::as_str) == Some("dev") {
                continue;
            }
            let Some(target) = dep.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let extern_name = dep
                .get("rename")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(target);
            crates.add_dependency(name, extern_name, target);
        }
    }
    crates
}

/// The target directory in `cargo metadata` output.
fn target_dir(meta: &serde_json::Value) -> Result<PathBuf, String> {
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
/// not run the macros. `AUTUMN_CONTRACT_STRICT` makes a failed descriptor
/// write a compile error, so a lost file cannot shrink the bundle.
fn run_pass(workspace: &Path, pass: &PassOptions, target: Option<&Path>) -> Result<Bundle, String> {
    let out = tempfile::Builder::new()
        .prefix("autumn-contracts-")
        .tempdir()
        .map_err(|e| format!("temp dir: {e}"))?;
    let mut cmd = Command::new("cargo");
    cmd.args(pass.cargo_args())
        .current_dir(workspace)
        .env("AUTUMN_CONTRACT_DIR", out.path())
        .env("AUTUMN_CONTRACT_STRICT", "1")
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
    let crates = crates_of(&metadata(workspace)?);
    Bundle::from_dir_with(out.path(), Some(&crates))
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
    fn the_dependency_graph_keeps_renames_and_leaves_out_dev_dependencies() {
        let meta = serde_json::json!({
            "packages": [{
                "name": "mesh-storefront",
                "dependencies": [
                    {"name": "mesh-catalog", "rename": null, "kind": null},
                    {"name": "shared-dto", "rename": "dto", "kind": null},
                    {"name": "test-kit", "rename": null, "kind": "dev"}
                ]
            }]
        });
        let crates = crates_of(&meta);
        assert_eq!(
            crates.dependency("mesh-storefront", "mesh_catalog"),
            Some("mesh-catalog")
        );
        assert_eq!(
            crates.dependency("mesh-storefront", "dto"),
            Some("shared-dto")
        );
        assert!(!crates.reaches("mesh-storefront", "test-kit"));
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
