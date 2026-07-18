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
    pub file: String,
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

/// Write `bytes` to `dest` atomically via a temp file so a failed write never
/// leaves a corrupt or partial file in place.
pub fn write_atomic(bytes: &[u8], dest: &Path) -> Result<(), AssetsError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
    }
    fs::rename(&tmp, dest)?;
    Ok(())
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
        let file_path = static_dir.join(&asset.file);
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

    println!("Downloading {name} {version}...");
    let bytes = download_bytes(&source_url)?;
    let sri = compute_sri(&bytes);

    let dest = PathBuf::from("static").join(&file_rel);
    write_atomic(&bytes, &dest)?;

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
    execute_update_with_manifest(name_spec, &manifest_path, Path::new("static"))
}

/// Testable core of `run_update`: resolves, downloads, and re-pins assets.
///
/// Separated from [`execute_update`] so tests can point at a temp directory
/// without touching the real `static/` tree.
fn execute_update_with_manifest(
    name_spec: Option<&str>,
    manifest_path: &Path,
    static_dir: &Path,
) -> Result<(), AssetsError> {
    let mut manifest = load_manifest(manifest_path)?;

    let to_update: Vec<(String, String, String, String)> = match name_spec {
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
                    resolve_source(&name, &version, None)?
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

    for (name, version, source_url, file_rel) in to_update {
        println!("Updating {name} → {version}...");
        let bytes = download_bytes(&source_url)?;
        let sri = compute_sri(&bytes);
        let dest = static_dir.join(&file_rel);
        write_atomic(&bytes, &dest)?;
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
                file: "js/htmx.min.js".into(),
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
                file: "js/htmx.min.js".into(),
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
                file: "js/htmx.min.js".into(),
                integrity: sri,
            },
        );
        save_manifest(&manifest_path, &manifest).unwrap();

        let result = verify_all(&manifest_path, &static_dir);
        assert!(result.is_ok(), "expected verify to pass");
    }

    #[test]
    fn write_atomic_does_not_leave_partial_on_rename() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let dest = tmp_dir.path().join("out.js");
        write_atomic(b"data", &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"data");
        // No leftover .tmp file.
        assert!(!dest.with_extension("tmp").exists());
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
                file: "js/my-custom-lib.min.js".into(),
                integrity: "sha384-abc".into(),
            },
        );
        save_manifest(&manifest_path, &manifest).unwrap();

        // Attempting to re-pin with @version should fail with a clear message.
        let result =
            execute_update_with_manifest(Some("my-custom-lib@2.0.0"), &manifest_path, &static_dir);
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
}
