//! `autumn assets` — pin, vendor, and integrity-verify JS dependencies.
//!
//! Downloads JS files into `static/js/`, computes a sha384 SRI hash, and
//! records `name → version → source URL → integrity` in
//! `static/.autumn-assets.json`. No Node/npm required; vendored files are
//! committed so release builds never touch the network.
//!
//! # Commands
//!
//! - `autumn assets add htmx@2.0.4` — download, hash, record.
//! - `autumn assets list`            — print pinned deps.
//! - `autumn assets update [name]`   — re-pin to recorded or new version.
//! - `autumn assets verify`          — recompute hashes and compare.
//!
//! The manifest is project data, and a contributor can change it. Each `file`
//! is an [`AssetPath`], so it cannot point outside `static/`. Each `source`
//! must be an `http` or `https` URL. The CLI prints the source of each
//! download.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use sha2::{Digest, Sha384};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum AssetsError {
    #[error("download failed: {0}")]
    Download(#[from] reqwest::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("integrity mismatch for {name}: expected {expected}, got {actual}")]
    IntegrityMismatch {
        name: String,
        expected: String,
        actual: String,
    },

    #[error(
        "unknown package `{0}`; use `--url` to specify a download URL, \
         or choose from the built-in registry: htmx, alpinejs, htmx-ext-sse, htmx-ext-ws"
    )]
    UnknownPackage(String),

    #[error("bad spec `{0}`: expected `<name>@<version>` (e.g. `htmx@2.0.4`)")]
    BadSpec(String),

    #[error("manifest parse error: {0}")]
    Parse(String),

    #[error("update failed: {0}")]
    UpdateFailed(String),

    #[error("unsafe asset path `{path}`: {reason}")]
    UnsafePath { path: String, reason: &'static str },

    #[error("unsafe asset source `{source_url}`: {reason}")]
    UnsafeSource {
        source_url: String,
        reason: &'static str,
    },
}

// ---------------------------------------------------------------------------
// Built-in package registry
// ---------------------------------------------------------------------------

/// A known package entry: npm package name, dist-relative path, output filename.
struct RegistryEntry {
    /// npm package name on jsdelivr.
    npm_pkg: &'static str,
    /// Path within the npm package to the dist file.
    dist_path: &'static str,
    /// Filename written to `static/js/`.
    output_file: &'static str,
}

const fn registry() -> &'static [(&'static str, RegistryEntry)] {
    &[
        (
            "htmx",
            RegistryEntry {
                npm_pkg: "htmx.org",
                dist_path: "dist/htmx.min.js",
                output_file: "htmx.min.js",
            },
        ),
        (
            "alpinejs",
            RegistryEntry {
                npm_pkg: "alpinejs",
                dist_path: "dist/cdn.min.js",
                output_file: "alpine.min.js",
            },
        ),
        (
            "htmx-ext-sse",
            RegistryEntry {
                npm_pkg: "htmx-ext-sse",
                dist_path: "dist/sse.min.js",
                output_file: "htmx-ext-sse.min.js",
            },
        ),
        (
            "htmx-ext-ws",
            RegistryEntry {
                npm_pkg: "htmx-ext-ws",
                dist_path: "dist/ws.min.js",
                output_file: "htmx-ext-ws.min.js",
            },
        ),
    ]
}

/// Resolve name+version to a download URL and output file path relative to `static/`.
///
/// If `url_override` is `Some`, it is used directly as the source; the output
/// file is derived from the registry when the name is known, or from the URL
/// basename otherwise.
pub fn resolve_source(
    name: &str,
    version: &str,
    url_override: Option<&str>,
) -> Result<(String, String), AssetsError> {
    let entry = registry().iter().find(|(k, _)| *k == name).map(|(_, e)| e);
    url_override.map_or_else(
        || {
            entry.map_or_else(
                || Err(AssetsError::UnknownPackage(name.to_owned())),
                |e| {
                    let url = format!(
                        "https://cdn.jsdelivr.net/npm/{}@{}/{}",
                        e.npm_pkg, version, e.dist_path
                    );
                    Ok((url, format!("js/{}", e.output_file)))
                },
            )
        },
        |url| {
            let file_rel = entry.map_or_else(
                || {
                    // Parse the URL properly so query strings and fragments are
                    // stripped before deriving the output filename.
                    let basename = ::url::Url::parse(url)
                        .ok()
                        .and_then(|u| {
                            u.path_segments()
                                .and_then(|mut segs| segs.next_back().map(str::to_owned))
                        })
                        .filter(|s| !s.is_empty() && !s.contains(".."))
                        .ok_or_else(|| {
                            AssetsError::BadSpec(format!(
                                "cannot derive a safe filename from URL: {url}"
                            ))
                        })?;
                    Ok::<String, AssetsError>(format!("js/{basename}"))
                },
                |e| Ok(format!("js/{}", e.output_file)),
            )?;
            Ok((url.to_owned(), file_rel))
        },
    )
}

// ---------------------------------------------------------------------------
// Spec parsing
// ---------------------------------------------------------------------------

/// Parse `"htmx@2.0.4"` into `("htmx", "2.0.4")`.
pub fn parse_spec(spec: &str) -> Result<(String, String), AssetsError> {
    let mut parts = spec.splitn(2, '@');
    let name = parts.next().unwrap_or_default().trim().to_owned();
    let version = parts
        .next()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| AssetsError::BadSpec(spec.to_owned()))?
        .to_owned();
    if name.is_empty() {
        return Err(AssetsError::BadSpec(spec.to_owned()));
    }
    Ok((name, version))
}

// ---------------------------------------------------------------------------
// SRI computation
// ---------------------------------------------------------------------------

/// Compute a `sha384-<base64>` SRI hash for `bytes`.
pub fn compute_sri(bytes: &[u8]) -> String {
    let digest = Sha384::digest(bytes);
    let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
    format!("sha384-{b64}")
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// A file path relative to `static/`. It cannot point outside `static/`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AssetPath(String);

impl AssetPath {
    /// Check `raw` and wrap it.
    ///
    /// # Errors
    ///
    /// [`AssetsError::UnsafePath`] when `raw` can point outside `static/`.
    pub fn new(raw: impl Into<String>) -> Result<Self, AssetsError> {
        let raw = raw.into();
        match unsafe_reason(&raw) {
            None => Ok(Self(raw)),
            Some(reason) => Err(AssetsError::UnsafePath { path: raw, reason }),
        }
    }

    /// The path as written in the manifest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `/`-separated parts of the path. Each part is a normal name.
    fn parts(&self) -> std::str::Split<'_, char> {
        self.0.split('/')
    }
}

/// Windows device names. A file with this stem opens the device.
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Why `raw` is not a safe path under `static/`, or `None` when it is safe.
///
/// The checks do not use the host OS, so a path is safe on all platforms.
fn unsafe_reason(raw: &str) -> Option<&'static str> {
    if raw.is_empty() {
        return Some("path is empty");
    }
    if raw.starts_with('/') {
        return Some("path is absolute");
    }
    if raw.contains('\\') {
        return Some("use `/` as the separator");
    }
    if raw.contains(':') {
        return Some("`:` is not permitted (drive or stream)");
    }
    if raw.chars().any(char::is_control) {
        return Some("control characters are not permitted");
    }
    for part in raw.split('/') {
        if part.is_empty() {
            return Some("path has an empty part");
        }
        if part == "." || part == ".." {
            return Some("`.` and `..` parts are not permitted");
        }
        // Windows removes a trailing dot or space, so `.. ` opens `..`.
        if part.ends_with('.') || part.ends_with(' ') {
            return Some("a part cannot end with `.` or a space");
        }
        let stem = part.split('.').next().unwrap_or(part);
        if WINDOWS_DEVICE_NAMES
            .iter()
            .any(|device| stem.eq_ignore_ascii_case(device))
        {
            return Some("a Windows device name is not permitted");
        }
    }
    None
}

impl TryFrom<String> for AssetPath {
    type Error = AssetsError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(raw)
    }
}

impl From<AssetPath> for String {
    fn from(path: AssetPath) -> Self {
        path.0
    }
}

impl std::fmt::Display for AssetPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Check that `url` is an `http` or `https` URL.
///
/// # Errors
///
/// [`AssetsError::UnsafeSource`] for any other URL.
pub fn check_source(url: &str) -> Result<(), AssetsError> {
    let reject = |reason| {
        Err(AssetsError::UnsafeSource {
            source_url: url.to_owned(),
            reason,
        })
    };
    match ::url::Url::parse(url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => Ok(()),
        Ok(_) => reject("only `http` and `https` are permitted"),
        Err(_) => reject("not a valid URL"),
    }
}

/// Path of the vendor manifest relative to the project root.
pub const VENDOR_MANIFEST_PATH: &str = "static/.autumn-assets.json";

/// In-memory vendor manifest — mirrors the framework's `VendorManifest`.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct VendorManifest {
    pub version: String,
    pub assets: BTreeMap<String, VendorAsset>,
}

/// Metadata for one vendored asset — mirrors the framework's `VendorAsset`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct VendorAsset {
    pub version: String,
    pub source: String,
    pub file: AssetPath,
    pub integrity: String,
}

impl Default for VendorManifest {
    fn default() -> Self {
        Self {
            version: "1".into(),
            assets: BTreeMap::new(),
        }
    }
}

/// Load the vendor manifest from `path`.
///
/// Returns a default (empty) manifest when the file does not exist, but
/// propagates a parse error when the file exists with invalid JSON — otherwise
/// a corrupt manifest (e.g. from a merge conflict) would silently become empty
/// and the next `save_manifest` would overwrite every other pinned asset.
pub fn load_manifest(path: &Path) -> Result<VendorManifest, AssetsError> {
    if !path.exists() {
        return Ok(VendorManifest::default());
    }
    let s = fs::read_to_string(path)?;
    serde_json::from_str(&s).map_err(|e| AssetsError::Parse(e.to_string()))
}

pub fn save_manifest(path: &Path, manifest: &VendorManifest) -> Result<(), AssetsError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json =
        serde_json::to_string_pretty(manifest).map_err(|e| AssetsError::Parse(e.to_string()))?;
    fs::write(path, json + "\n")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Download helper
// ---------------------------------------------------------------------------

/// Download `url` and return the bytes.  Delegates to [`crate::http::fetch_bytes`]
/// so progress-bar logic is shared with `autumn setup`.
pub fn download_bytes(url: &str) -> Result<Vec<u8>, AssetsError> {
    Ok(crate::http::fetch_bytes(url)?)
}

/// Write `bytes` to `file` under `static_dir` and return the full path.
///
/// `static_dir` can be a symlink. No part below it can be a symlink, so the
/// write stays under `static_dir`. The bytes go to a new temp file first,
/// then a rename replaces the asset. A failed write keeps the old asset.
///
/// # Errors
///
/// [`AssetsError::UnsafePath`] when a part of the path is a symlink or is not
/// a directory. [`AssetsError::Io`] for I/O errors.
pub fn write_asset(
    static_dir: &Path,
    file: &AssetPath,
    bytes: &[u8],
) -> Result<PathBuf, AssetsError> {
    let unsafe_path = |reason| AssetsError::UnsafePath {
        path: file.as_str().to_owned(),
        reason,
    };
    let parts: Vec<&str> = file.parts().collect();
    let (name, parents) = parts
        .split_last()
        .ok_or_else(|| unsafe_path("path is empty"))?;

    fs::create_dir_all(static_dir)?;
    let mut dir = static_dir.to_path_buf();
    for part in parents {
        dir.push(part);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(unsafe_path("a part of the path is a symlink"));
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(unsafe_path("a part of the path is not a directory")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&dir)?,
            Err(e) => return Err(e.into()),
        }
    }

    let dest = dir.join(name);
    match fs::symlink_metadata(&dest) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(unsafe_path("the file is a symlink"));
        }
        Ok(meta) if meta.is_dir() => return Err(unsafe_path("the path is a directory")),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    // `new_in` makes a new file with a random name and `O_EXCL`. It does not
    // follow a symlink that is already in the tree.
    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(&dest).map_err(|e| e.error)?;
    Ok(dest)
}

// ---------------------------------------------------------------------------
// Verify
// ---------------------------------------------------------------------------

/// Verify all vendored files in the manifest against their recorded integrity.
///
/// Exits non-zero on the first mismatch; reports all mismatches before
/// exiting so the user can see every broken file at once.
pub fn run_verify(manifest_path: &Path, static_dir: &Path) {
    if let Err(e) = verify_all(manifest_path, static_dir) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

pub fn verify_all(manifest_path: &Path, static_dir: &Path) -> Result<(), AssetsError> {
    let manifest = load_manifest(manifest_path)?;
    if manifest.assets.is_empty() {
        println!("No vendored assets recorded in {}", manifest_path.display());
        return Ok(());
    }
    let mut ok = true;
    for (name, asset) in &manifest.assets {
        let file_path = static_dir.join(asset.file.as_str());
        match fs::read(&file_path) {
            Err(e) => {
                eprintln!("  MISSING  {name} — {}: {e}", file_path.display());
                ok = false;
            }
            Ok(bytes) => {
                let actual = compute_sri(&bytes);
                if actual == asset.integrity {
                    println!("  OK       {name}  {}", asset.integrity);
                } else {
                    eprintln!("  TAMPERED {name}");
                    eprintln!("    expected: {}", asset.integrity);
                    eprintln!("    actual:   {actual}");
                    ok = false;
                }
            }
        }
    }
    if ok {
        Ok(())
    } else {
        Err(AssetsError::IntegrityMismatch {
            name: "one or more assets".into(),
            expected: "(see above)".into(),
            actual: "(see above)".into(),
        })
    }
}

// ---------------------------------------------------------------------------
// Public command runners
// ---------------------------------------------------------------------------

pub fn run_add(spec: &str, url_override: Option<&str>) {
    if let Err(e) = execute_add(spec, url_override) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

fn execute_add(spec: &str, url_override: Option<&str>) -> Result<(), AssetsError> {
    let (name, version) = parse_spec(spec)?;
    let (source_url, file_rel) = resolve_source(&name, &version, url_override)?;
    let file_rel = AssetPath::new(file_rel)?;
    check_source(&source_url)?;

    println!("Downloading {name} {version} from {source_url}...");
    let bytes = download_bytes(&source_url)?;
    let sri = compute_sri(&bytes);

    write_asset(Path::new("static"), &file_rel, &bytes)?;

    let manifest_path = PathBuf::from(VENDOR_MANIFEST_PATH);
    let mut manifest = load_manifest(&manifest_path)?;
    manifest.assets.insert(
        name.clone(),
        VendorAsset {
            version: version.clone(),
            source: source_url,
            file: file_rel,
            integrity: sri.clone(),
        },
    );
    save_manifest(&manifest_path, &manifest)?;

    println!("  Added {name} {version}");
    println!("  Integrity: {sri}");
    println!("  Manifest:  {}", manifest_path.display());
    Ok(())
}

pub fn run_list() {
    let manifest_path = PathBuf::from(VENDOR_MANIFEST_PATH);
    let manifest = match load_manifest(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Error loading manifest: {e}");
            std::process::exit(1);
        }
    };
    if manifest.assets.is_empty() {
        println!("No vendored assets. Run `autumn assets add <name>@<version>` to add one.");
        return;
    }
    println!("{:<20} {:<12} INTEGRITY", "NAME", "VERSION");
    println!("{}", "-".repeat(80));
    for (name, asset) in &manifest.assets {
        println!("{:<20} {:<12} {}", name, asset.version, asset.integrity);
    }
}

pub fn run_update(name: Option<&str>) {
    if let Err(e) = execute_update(name) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

fn execute_update(name_spec: Option<&str>) -> Result<(), AssetsError> {
    let manifest_path = PathBuf::from(VENDOR_MANIFEST_PATH);
    execute_update_with_manifest(
        name_spec,
        &manifest_path,
        Path::new("static"),
        download_bytes,
    )
}

/// Testable core of `run_update`: resolves, downloads, and re-pins assets.
///
/// Tests give a temp directory and a fake `fetch`.
fn execute_update_with_manifest(
    name_spec: Option<&str>,
    manifest_path: &Path,
    static_dir: &Path,
    fetch: impl Fn(&str) -> Result<Vec<u8>, AssetsError>,
) -> Result<(), AssetsError> {
    let mut manifest = load_manifest(manifest_path)?;

    let to_update: Vec<(String, String, String, AssetPath)> = match name_spec {
        None => {
            // Update all.
            manifest
                .assets
                .iter()
                .map(|(n, a)| {
                    (
                        n.clone(),
                        a.version.clone(),
                        a.source.clone(),
                        a.file.clone(),
                    )
                })
                .collect()
        }
        Some(spec) => {
            if spec.contains('@') {
                // Re-pin to a different version.
                let (name, version) = parse_spec(spec)?;
                let in_registry = registry().iter().any(|(k, _)| *k == name.as_str());
                let (source_url, file_rel) = if in_registry {
                    let (url, file) = resolve_source(&name, &version, None)?;
                    (url, AssetPath::new(file)?)
                } else {
                    // Package was added with `--url`; we cannot derive the new
                    // version's URL automatically.  The user must re-add it:
                    return Err(AssetsError::UpdateFailed(format!(
                        "{name} was added with --url and is not in the built-in registry; \
                         re-pin with `autumn assets add {name}@{version} --url <url>`"
                    )));
                };
                vec![(name, version, source_url, file_rel)]
            } else {
                // Refresh the existing pinned version.
                let asset = manifest
                    .assets
                    .get(spec)
                    .ok_or_else(|| AssetsError::UnknownPackage(spec.to_owned()))?;
                vec![(
                    spec.to_owned(),
                    asset.version.clone(),
                    asset.source.clone(),
                    asset.file.clone(),
                )]
            }
        }
    };

    for (_, _, source_url, _) in &to_update {
        check_source(source_url)?;
    }

    for (name, version, source_url, file_rel) in to_update {
        println!("Updating {name} → {version} from {source_url}...");
        let bytes = fetch(&source_url)?;
        let sri = compute_sri(&bytes);
        write_asset(static_dir, &file_rel, &bytes)?;
        manifest.assets.insert(
            name.clone(),
            VendorAsset {
                version: version.clone(),
                source: source_url,
                file: file_rel,
                integrity: sri.clone(),
            },
        );
        println!("  Updated {name} {version}  {sri}");
    }

    save_manifest(manifest_path, &manifest)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_spec ---

    #[test]
    fn parse_spec_happy_path() {
        let (name, version) = parse_spec("htmx@2.0.4").unwrap();
        assert_eq!(name, "htmx");
        assert_eq!(version, "2.0.4");
    }

    #[test]
    fn parse_spec_with_prerelease() {
        let (name, version) = parse_spec("alpinejs@3.14.1").unwrap();
        assert_eq!(name, "alpinejs");
        assert_eq!(version, "3.14.1");
    }

    #[test]
    fn parse_spec_missing_version_errors() {
        let err = parse_spec("htmx").unwrap_err();
        assert!(matches!(err, AssetsError::BadSpec(_)));
        assert!(err.to_string().contains("htmx"));
    }

    #[test]
    fn parse_spec_empty_string_errors() {
        assert!(parse_spec("").is_err());
        assert!(parse_spec("@").is_err());
    }

    // --- resolve_source ---

    #[test]
    fn resolve_htmx_builds_jsdelivr_url() {
        let (url, file) = resolve_source("htmx", "2.0.4", None).unwrap();
        assert!(
            url.contains("htmx.org@2.0.4"),
            "URL should contain package@version: {url}"
        );
        assert!(
            url.contains("jsdelivr"),
            "URL should be from jsdelivr: {url}"
        );
        assert_eq!(file, "js/htmx.min.js");
    }

    #[test]
    fn resolve_alpinejs_builds_jsdelivr_url() {
        let (url, file) = resolve_source("alpinejs", "3.14.1", None).unwrap();
        assert!(url.contains("alpinejs@3.14.1"), "{url}");
        assert_eq!(file, "js/alpine.min.js");
    }

    #[test]
    fn resolve_unknown_without_url_errors() {
        let err = resolve_source("__not_in_registry__", "1.0.0", None).unwrap_err();
        assert!(matches!(err, AssetsError::UnknownPackage(_)));
    }

    #[test]
    fn resolve_url_override_for_known_package() {
        let custom = "https://example.com/htmx.custom.js";
        let (url, file) = resolve_source("htmx", "2.0.4", Some(custom)).unwrap();
        assert_eq!(url, custom);
        assert_eq!(file, "js/htmx.min.js"); // still uses the registry output name
    }

    #[test]
    fn resolve_url_override_for_unknown_package() {
        let custom = "https://example.com/my-lib.min.js";
        let (url, file) = resolve_source("my-lib", "1.0.0", Some(custom)).unwrap();
        assert_eq!(url, custom);
        assert_eq!(file, "js/my-lib.min.js");
    }

    // --- compute_sri ---

    #[test]
    fn compute_sri_empty_input_known_value() {
        // SHA-384 of empty string, base64-encoded.
        let sri = compute_sri(b"");
        assert_eq!(
            sri,
            "sha384-OLBgp1GsljhM2TJ+sbHjaiH9txEUvgdDTAzHv2P24donTt6/529l+9Ua0vFImLlb"
        );
    }

    #[test]
    fn compute_sri_prefix() {
        assert!(compute_sri(b"hello").starts_with("sha384-"));
    }

    #[test]
    fn compute_sri_deterministic() {
        assert_eq!(compute_sri(b"data"), compute_sri(b"data"));
    }

    // --- manifest round-trip ---

    #[test]
    fn manifest_json_round_trip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut manifest = VendorManifest::default();
        manifest.assets.insert(
            "htmx".into(),
            VendorAsset {
                version: "2.0.4".into(),
                source: "https://cdn.jsdelivr.net/npm/htmx.org@2.0.4/dist/htmx.min.js".into(),
                file: AssetPath::new("js/htmx.min.js").unwrap(),
                integrity: "sha384-abc".into(),
            },
        );
        save_manifest(tmp.path(), &manifest).unwrap();
        let loaded = load_manifest(tmp.path()).unwrap();
        assert_eq!(loaded.assets.len(), 1);
        let htmx = loaded.assets.get("htmx").unwrap();
        assert_eq!(htmx.version, "2.0.4");
        assert_eq!(htmx.integrity, "sha384-abc");
    }

    #[test]
    fn load_manifest_missing_file_returns_default() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let missing = tmp_dir.path().join("does-not-exist.json");
        let manifest = load_manifest(&missing).unwrap();
        assert!(manifest.assets.is_empty());
        assert_eq!(manifest.version, "1");
    }

    #[test]
    fn load_manifest_corrupt_json_propagates_error() {
        // A corrupt manifest (e.g. from a merge conflict) must NOT silently
        // become an empty manifest — otherwise a subsequent save would wipe
        // every other pinned asset.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmp.path(), b"{ this is not valid json <<<<<<< HEAD").unwrap();
        let err = load_manifest(tmp.path()).unwrap_err();
        assert!(
            matches!(err, AssetsError::Parse(_)),
            "expected Parse error for corrupt manifest, got: {err:?}"
        );
    }

    #[test]
    fn verify_all_errors_on_corrupt_manifest() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let static_dir = tmp_dir.path().to_path_buf();
        let manifest_path = static_dir.join(".autumn-assets.json");
        fs::write(&manifest_path, b"not json").unwrap();
        let result = verify_all(&manifest_path, &static_dir);
        assert!(
            matches!(result, Err(AssetsError::Parse(_))),
            "verify must surface a parse error rather than report an empty manifest"
        );
    }

    // --- verify detects tampering ---

    #[test]
    fn verify_detects_tampered_file() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let static_dir = tmp_dir.path().to_path_buf();
        let js_dir = static_dir.join("js");
        fs::create_dir_all(&js_dir).unwrap();

        // Write a file with known content.
        let original = b"console.log('htmx');";
        let good_sri = compute_sri(original);
        fs::write(js_dir.join("htmx.min.js"), original).unwrap();

        // Record the good hash in the manifest.
        let manifest_path = static_dir.join(".autumn-assets.json");
        let mut manifest = VendorManifest::default();
        manifest.assets.insert(
            "htmx".into(),
            VendorAsset {
                version: "2.0.4".into(),
                source: "https://example.com".into(),
                file: AssetPath::new("js/htmx.min.js").unwrap(),
                integrity: good_sri,
            },
        );
        save_manifest(&manifest_path, &manifest).unwrap();

        // Tamper the file.
        fs::write(js_dir.join("htmx.min.js"), b"console.log('evil');").unwrap();

        // Verify should fail.
        let result = verify_all(&manifest_path, &static_dir);
        assert!(result.is_err(), "expected verify to fail after tampering");
    }

    #[test]
    fn verify_passes_for_good_file() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let static_dir = tmp_dir.path().to_path_buf();
        let js_dir = static_dir.join("js");
        fs::create_dir_all(&js_dir).unwrap();

        let content = b"console.log('htmx');";
        let sri = compute_sri(content);
        fs::write(js_dir.join("htmx.min.js"), content).unwrap();

        let manifest_path = static_dir.join(".autumn-assets.json");
        let mut manifest = VendorManifest::default();
        manifest.assets.insert(
            "htmx".into(),
            VendorAsset {
                version: "2.0.4".into(),
                source: "https://example.com".into(),
                file: AssetPath::new("js/htmx.min.js").unwrap(),
                integrity: sri,
            },
        );
        save_manifest(&manifest_path, &manifest).unwrap();

        let result = verify_all(&manifest_path, &static_dir);
        assert!(result.is_ok(), "expected verify to pass");
    }

    #[test]
    fn write_asset_leaves_no_temp_file() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let file = AssetPath::new("out.js").unwrap();
        let dest = write_asset(tmp_dir.path(), &file, b"data").unwrap();
        assert_eq!(dest, tmp_dir.path().join("out.js"));
        assert_eq!(fs::read(&dest).unwrap(), b"data");
        let entries: Vec<_> = fs::read_dir(tmp_dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "only the asset stays: {entries:?}");
    }

    // --- Fix #2: execute_update for --url packages ---

    #[test]
    fn execute_update_with_version_for_url_package_errors_with_hint() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let static_dir = tmp_dir.path().to_path_buf();
        let manifest_path = static_dir.join(".autumn-assets.json");

        // Simulate a package added with --url (not in the built-in registry).
        let mut manifest = VendorManifest::default();
        manifest.assets.insert(
            "my-custom-lib".into(),
            VendorAsset {
                version: "1.0.0".into(),
                source: "https://example.com/my-custom-lib.min.js".into(),
                file: AssetPath::new("js/my-custom-lib.min.js").unwrap(),
                integrity: "sha384-abc".into(),
            },
        );
        save_manifest(&manifest_path, &manifest).unwrap();

        // Attempting to re-pin with @version should fail with a clear message.
        let result = execute_update_with_manifest(
            Some("my-custom-lib@2.0.0"),
            &manifest_path,
            &static_dir,
            no_network,
        );
        let err = result.expect_err("expected error for --url package re-pin");
        assert!(
            matches!(err, AssetsError::UpdateFailed(_)),
            "expected UpdateFailed variant, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("--url"), "error should mention --url: {msg}");
        assert!(
            msg.contains("my-custom-lib"),
            "error should mention the package name: {msg}"
        );
    }

    // --- Fix #3: URL→filename sanitization ---

    #[test]
    fn resolve_url_with_query_string_strips_it() {
        // A URL with ?v= query: the derived filename must NOT include the query string.
        let custom = "https://example.com/lib.min.js?v=1.2.3";
        let (url, file) = resolve_source("unknown-pkg", "1.0.0", Some(custom)).unwrap();
        assert_eq!(url, custom);
        assert_eq!(file, "js/lib.min.js", "query string must be stripped");
    }

    #[test]
    fn resolve_url_with_trailing_slash_errors() {
        // A URL ending in / has an empty last path segment — cannot derive filename.
        let custom = "https://example.com/path/";
        let err = resolve_source("unknown-pkg", "1.0.0", Some(custom)).unwrap_err();
        assert!(
            matches!(err, AssetsError::BadSpec(_)),
            "expected BadSpec for trailing-slash URL"
        );
    }

    // --- Fix #4: Schema-identity test (CLI types ↔ framework types) ---

    #[test]
    fn cli_and_web_vendor_manifest_schemas_are_compatible() {
        let json = r#"{
            "version": "1",
            "assets": {
                "htmx": {
                    "version": "2.0.4",
                    "source": "https://example.com",
                    "file": "js/htmx.min.js",
                    "integrity": "sha384-abc"
                }
            }
        }"#;

        // Must parse as CLI type.
        let cli: VendorManifest = serde_json::from_str(json).unwrap();
        assert_eq!(cli.assets["htmx"].version, "2.0.4");
        assert_eq!(cli.assets["htmx"].integrity, "sha384-abc");

        // Must also parse as the framework type.
        let fw: autumn_web::assets::VendorManifest = serde_json::from_str(json).unwrap();
        assert_eq!(fw.assets["htmx"].version, "2.0.4");
        assert_eq!(fw.assets["htmx"].file, "js/htmx.min.js");

        // CLI → JSON → framework round-trip must preserve all fields.
        let serialized = serde_json::to_string(&cli).unwrap();
        let fw2: autumn_web::assets::VendorManifest = serde_json::from_str(&serialized).unwrap();
        assert_eq!(fw2.assets["htmx"].integrity, "sha384-abc");
        assert_eq!(fw2.assets["htmx"].source, "https://example.com");
    }

    // --- Path and source containment ---

    /// A fetch that fails the test: the case must stop before any download.
    fn no_network(url: &str) -> Result<Vec<u8>, AssetsError> {
        panic!("unexpected download of {url}");
    }

    /// A fetch that returns bytes made from the URL.
    #[allow(clippy::unnecessary_wraps)]
    fn fake_fetch(url: &str) -> Result<Vec<u8>, AssetsError> {
        Ok(format!("// from {url}\n").into_bytes())
    }

    /// A project root with an empty `static/` and a sibling `outside/`.
    struct Project {
        _root: tempfile::TempDir,
        static_dir: PathBuf,
        outside: PathBuf,
        manifest: PathBuf,
    }

    fn project() -> Project {
        let root = tempfile::TempDir::new().unwrap();
        let static_dir = root.path().join("static");
        let outside = root.path().join("outside");
        fs::create_dir_all(&static_dir).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let manifest = static_dir.join(".autumn-assets.json");
        Project {
            _root: root,
            static_dir,
            outside,
            manifest,
        }
    }

    /// Write a manifest with one asset, with no check on `file`.
    fn write_raw_manifest(path: &Path, name: &str, source: &str, file: &str) {
        let json = serde_json::json!({
            "version": "1",
            "assets": {
                name: {
                    "version": "1.0.0",
                    "source": source,
                    "file": file,
                    "integrity": "sha384-abc",
                }
            }
        });
        fs::write(path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    }

    fn assert_dir_empty(dir: &Path) {
        let entries: Vec<_> = fs::read_dir(dir).unwrap().collect();
        assert!(
            entries.is_empty(),
            "{} must stay empty: {entries:?}",
            dir.display()
        );
    }

    const SAFE_PATHS: &[&str] = &[
        "htmx.min.js",
        "js/htmx.min.js",
        "js/vendor/alpine.min.js",
        "css/site.css",
        ".well-known/x.js",
        "js/v1.2.3/lib.js",
    ];

    const ESCAPING_PATHS: &[&str] = &[
        "",
        "/etc/passwd",
        "//server/share/x.js",
        "../x.js",
        "..",
        ".",
        "js/../../x.js",
        "js/..",
        "./js/x.js",
        "js/./x.js",
        "js//x.js",
        "js/",
        "C:\\Windows\\x.js",
        "C:/Windows/x.js",
        "C:x.js",
        "\\\\server\\share\\x.js",
        "\\x.js",
        "js\\..\\..\\x.js",
        "js/x.js:stream",
        "js/x\0.js",
        "js/\n.js",
        "js/.. /x.js",
        "js/... /x.js",
        "js/x.",
        "js/x ",
        "NUL",
        "js/con.js",
        "js/COM1",
        "js/lpt9.txt",
    ];

    #[test]
    fn asset_path_accepts_normal_relative_paths() {
        for raw in SAFE_PATHS {
            let path = AssetPath::new(*raw).unwrap_or_else(|e| panic!("{raw:?}: {e}"));
            assert_eq!(path.as_str(), *raw);
        }
    }

    #[test]
    fn asset_path_rejects_paths_that_can_escape() {
        for raw in ESCAPING_PATHS {
            let err = AssetPath::new(*raw).expect_err(&format!("{raw:?} must be rejected"));
            assert!(
                matches!(err, AssetsError::UnsafePath { .. }),
                "{raw:?}: wrong error {err:?}"
            );
        }
    }

    #[test]
    fn asset_path_rejects_host_absolute_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let abs = tmp.path().join("pwn.js");
        let raw = abs.to_str().unwrap();
        assert!(AssetPath::new(raw).is_err(), "{raw:?} must be rejected");
    }

    #[test]
    fn asset_path_serializes_as_a_plain_string() {
        let path = AssetPath::new("js/htmx.min.js").unwrap();
        assert_eq!(serde_json::to_string(&path).unwrap(), "\"js/htmx.min.js\"");
        let back: AssetPath = serde_json::from_str("\"js/htmx.min.js\"").unwrap();
        assert_eq!(back, path);
    }

    #[test]
    fn load_manifest_rejects_escaping_file() {
        let p = project();
        write_raw_manifest(
            &p.manifest,
            "evil",
            "https://example.com/x.js",
            "../outside/x.js",
        );
        let err = load_manifest(&p.manifest).expect_err("escaping file must not load");
        assert!(matches!(err, AssetsError::Parse(_)), "{err:?}");
        assert!(err.to_string().contains("unsafe asset path"), "{err}");
    }

    #[test]
    fn verify_rejects_escaping_file() {
        let p = project();
        write_raw_manifest(
            &p.manifest,
            "evil",
            "https://example.com/x.js",
            "../outside/x.js",
        );
        assert!(verify_all(&p.manifest, &p.static_dir).is_err());
    }

    #[test]
    fn update_all_rejects_escaping_file_before_download() {
        let p = project();
        let abs = p.outside.join("pwn.js");
        let abs = abs.to_str().unwrap().to_owned();
        for file in ["../outside/pwn.js", "js/../../outside/pwn.js", abs.as_str()] {
            write_raw_manifest(&p.manifest, "evil", "https://attacker.example/x.js", file);
            let before = fs::read(&p.manifest).unwrap();
            let err = execute_update_with_manifest(None, &p.manifest, &p.static_dir, no_network)
                .expect_err(&format!("{file:?} must be rejected"));
            assert!(
                err.to_string().contains("unsafe asset path"),
                "{file:?}: {err}"
            );
            assert_dir_empty(&p.outside);
            assert_eq!(
                fs::read(&p.manifest).unwrap(),
                before,
                "manifest must not change"
            );
        }
    }

    #[test]
    fn named_refresh_rejects_escaping_file_before_download() {
        let p = project();
        write_raw_manifest(
            &p.manifest,
            "evil",
            "https://attacker.example/x.js",
            "../outside/pwn.js",
        );
        let err =
            execute_update_with_manifest(Some("evil"), &p.manifest, &p.static_dir, no_network)
                .expect_err("escaping file must be rejected");
        assert!(err.to_string().contains("unsafe asset path"), "{err}");
        assert_dir_empty(&p.outside);
    }

    #[test]
    fn check_source_accepts_http_and_https() {
        check_source("https://cdn.jsdelivr.net/npm/htmx.org@2.0.4/dist/htmx.min.js").unwrap();
        check_source("http://localhost:8080/lib.js").unwrap();
    }

    #[test]
    fn check_source_rejects_other_schemes() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/x.js",
            "data:text/javascript,alert(1)",
            "not a url",
            "",
        ] {
            let err = check_source(url).expect_err(&format!("{url:?} must be rejected"));
            assert!(
                matches!(err, AssetsError::UnsafeSource { .. }),
                "{url:?}: {err:?}"
            );
        }
    }

    #[test]
    fn update_rejects_bad_source_before_download() {
        let p = project();
        write_raw_manifest(&p.manifest, "evil", "file:///etc/passwd", "js/evil.js");
        let err = execute_update_with_manifest(None, &p.manifest, &p.static_dir, no_network)
            .expect_err("file: source must be rejected");
        assert!(matches!(err, AssetsError::UnsafeSource { .. }), "{err:?}");
        assert!(!p.static_dir.join("js").exists());
    }

    #[test]
    fn update_all_writes_under_static_and_keeps_source() {
        let p = project();
        let source = "https://example.com/htmx.js";
        write_raw_manifest(&p.manifest, "htmx", source, "js/htmx.min.js");
        execute_update_with_manifest(None, &p.manifest, &p.static_dir, fake_fetch).unwrap();

        let bytes = fs::read(p.static_dir.join("js/htmx.min.js")).unwrap();
        assert_eq!(bytes, fake_fetch(source).unwrap());
        let manifest = load_manifest(&p.manifest).unwrap();
        let asset = &manifest.assets["htmx"];
        assert_eq!(asset.source, source);
        assert_eq!(asset.file.as_str(), "js/htmx.min.js");
        assert_eq!(asset.integrity, compute_sri(&bytes));
        assert_dir_empty(&p.outside);
    }

    #[test]
    fn named_repin_uses_registry_source_and_path() {
        let p = project();
        write_raw_manifest(
            &p.manifest,
            "htmx",
            "https://example.com/old.js",
            "js/htmx.min.js",
        );
        execute_update_with_manifest(Some("htmx@2.0.5"), &p.manifest, &p.static_dir, fake_fetch)
            .unwrap();
        let manifest = load_manifest(&p.manifest).unwrap();
        let asset = &manifest.assets["htmx"];
        assert!(asset.source.contains("htmx.org@2.0.5"), "{}", asset.source);
        assert_eq!(asset.file.as_str(), "js/htmx.min.js");
        assert!(p.static_dir.join("js/htmx.min.js").is_file());
        assert_dir_empty(&p.outside);
    }

    #[cfg(unix)]
    #[test]
    fn write_asset_rejects_symlinked_ancestor() {
        let p = project();
        std::os::unix::fs::symlink(&p.outside, p.static_dir.join("js")).unwrap();
        let file = AssetPath::new("js/htmx.min.js").unwrap();
        let err = write_asset(&p.static_dir, &file, b"evil").expect_err("symlinked dir");
        assert!(matches!(err, AssetsError::UnsafePath { .. }), "{err:?}");
        assert_dir_empty(&p.outside);
    }

    #[cfg(unix)]
    #[test]
    fn write_asset_rejects_symlinked_destination() {
        let p = project();
        let victim = p.outside.join("victim");
        fs::write(&victim, b"original").unwrap();
        fs::create_dir_all(p.static_dir.join("js")).unwrap();
        std::os::unix::fs::symlink(&victim, p.static_dir.join("js/htmx.min.js")).unwrap();
        let file = AssetPath::new("js/htmx.min.js").unwrap();
        let err = write_asset(&p.static_dir, &file, b"evil").expect_err("symlinked file");
        assert!(matches!(err, AssetsError::UnsafePath { .. }), "{err:?}");
        assert_eq!(fs::read(&victim).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn write_asset_does_not_write_through_planted_temp_symlink() {
        let p = project();
        let victim = p.outside.join("victim");
        fs::write(&victim, b"original").unwrap();
        fs::create_dir_all(p.static_dir.join("js")).unwrap();
        std::os::unix::fs::symlink(&victim, p.static_dir.join("js/htmx.min.tmp")).unwrap();
        let file = AssetPath::new("js/htmx.min.js").unwrap();
        write_asset(&p.static_dir, &file, b"new").unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"original");
        assert_eq!(
            fs::read(p.static_dir.join("js/htmx.min.js")).unwrap(),
            b"new"
        );
    }

    #[cfg(unix)]
    #[test]
    fn update_rejects_symlinked_ancestor() {
        let p = project();
        std::os::unix::fs::symlink(&p.outside, p.static_dir.join("js")).unwrap();
        write_raw_manifest(
            &p.manifest,
            "htmx",
            "https://example.com/x.js",
            "js/htmx.min.js",
        );
        let err = execute_update_with_manifest(None, &p.manifest, &p.static_dir, fake_fetch)
            .expect_err("symlinked dir must be rejected");
        assert!(matches!(err, AssetsError::UnsafePath { .. }), "{err:?}");
        assert_dir_empty(&p.outside);
    }

    #[cfg(unix)]
    #[test]
    fn write_asset_allows_symlinked_static_root() {
        let p = project();
        let link = p.outside.parent().unwrap().join("static-link");
        std::os::unix::fs::symlink(&p.static_dir, &link).unwrap();
        let file = AssetPath::new("js/htmx.min.js").unwrap();
        write_asset(&link, &file, b"ok").unwrap();
        assert_eq!(
            fs::read(p.static_dir.join("js/htmx.min.js")).unwrap(),
            b"ok"
        );
    }

    proptest::proptest! {
        /// Every accepted path joins to a path under the root.
        #[test]
        fn accepted_paths_stay_under_root(raw in "[a-zA-Z0-9._ :/\\\\-]{0,24}") {
            if let Ok(path) = AssetPath::new(raw.as_str()) {
                let root = Path::new("root");
                let joined = root.join(path.as_str());
                proptest::prop_assert!(joined.starts_with(root));
                for part in joined.components() {
                    proptest::prop_assert!(
                        matches!(part, std::path::Component::Normal(_)),
                        "{raw:?} gives {part:?}"
                    );
                }
            }
        }
    }
}
