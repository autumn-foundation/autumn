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
//! # Safety
//!
//! The manifest is project data. A contributor can change it. Thus:
//!
//! - Each `file` is an [`AssetPath`]. It cannot point outside `static/`.
//! - Reads and writes do not follow a symlink out of `static/`.
//! - `update` downloads only from `https` sources. `add --url` also accepts
//!   `http`, because the user gives that URL.
//! - The CLI prints the source of each download.

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

    /// An asset `file` is not a safe path under `static/`.
    #[error("unsafe asset path `{path}`: {reason}")]
    UnsafePath { path: String, reason: &'static str },

    /// The manifest file is not a regular file.
    #[error("unsafe manifest `{path}`: {reason}")]
    UnsafeManifest { path: String, reason: &'static str },

    /// An asset `source` is not a permitted URL.
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
///
/// The path uses `/` as the separator. Each part is a normal file name. The
/// check is the same on all platforms, because the manifest is committed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AssetPath(String);

impl AssetPath {
    /// Check `raw` and wrap it.
    ///
    /// # Errors
    ///
    /// [`AssetsError::UnsafePath`] when `raw` is not a safe path under
    /// `static/`. This includes Windows device names such as `con.js`.
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

    fn unsafe_path(&self, reason: &'static str) -> AssetsError {
        AssetsError::UnsafePath {
            path: self.0.clone(),
            reason,
        }
    }
}

/// Windows device names. A file with this stem opens the device.
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "CLOCK$",
    "CONIN$",
    "CONOUT$",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "COM\u{b9}",
    "COM\u{b2}",
    "COM\u{b3}",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "LPT\u{b9}",
    "LPT\u{b2}",
    "LPT\u{b3}",
];

/// The reason `raw` is not a safe path under `static/`, or `None`.
fn unsafe_reason(raw: &str) -> Option<&'static str> {
    if raw.is_empty() {
        return Some("path is empty");
    }
    if raw.starts_with('/') {
        return Some("path is absolute");
    }
    if raw.contains('\\') {
        return Some("path contains `\\`");
    }
    if raw.contains(':') {
        return Some("path contains `:`");
    }
    if raw.chars().any(char::is_control) {
        return Some("path contains a control character");
    }
    for part in raw.split('/') {
        if part.is_empty() {
            return Some("path has an empty part");
        }
        if part == "." || part == ".." {
            return Some("path has a `.` or `..` part");
        }
        // Windows removes a trailing dot or space. Thus `.. ` opens `..`.
        if part.ends_with('.') || part.ends_with(' ') {
            return Some("a part ends with `.` or a space");
        }
        let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ');
        if WINDOWS_DEVICE_NAMES
            .iter()
            .any(|device| stem.eq_ignore_ascii_case(device))
        {
            return Some("a part is a Windows device name");
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

impl AsRef<Path> for AssetPath {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
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

/// The URL schemes that [`check_source`] accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePolicy {
    /// Accept only `https`. Use this for a URL from the manifest.
    HttpsOnly,
    /// Accept `http` and `https`. Use this for a URL that the user gives.
    HttpOrHttps,
}

/// Check that `policy` accepts the scheme of `url`.
///
/// # Errors
///
/// [`AssetsError::UnsafeSource`] when `url` is not a valid URL, or when
/// `policy` does not accept its scheme.
pub fn check_source(url: &str, policy: SourcePolicy) -> Result<(), AssetsError> {
    let reject = |reason| {
        Err(AssetsError::UnsafeSource {
            source_url: url.to_owned(),
            reason,
        })
    };
    let Ok(parsed) = ::url::Url::parse(url) else {
        return reject("not a valid URL");
    };
    match (parsed.scheme(), policy) {
        ("https", _) | ("http", SourcePolicy::HttpOrHttps) => Ok(()),
        ("http", SourcePolicy::HttpsOnly) => {
            reject("a manifest source must use `https`; re-add the asset with `--url`")
        }
        _ => reject("only `http` and `https` are permitted"),
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
///
/// # Errors
///
/// [`AssetsError::UnsafeManifest`] when `path` is a symlink or is not a
/// regular file. [`AssetsError::Parse`] when an entry is not valid, for
/// example a `file` that is not an [`AssetPath`].
pub fn load_manifest(path: &Path) -> Result<VendorManifest, AssetsError> {
    let unsafe_manifest = |reason| AssetsError::UnsafeManifest {
        path: path.display().to_string(),
        reason,
    };
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(VendorManifest::default());
        }
        Err(e) => return Err(e.into()),
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(unsafe_manifest("the manifest is a symlink"));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(unsafe_manifest("the manifest is not a regular file"));
        }
        Ok(_) => {}
    }
    let s = fs::read_to_string(path)?;
    serde_json::from_str(&s).map_err(|e| AssetsError::Parse(e.to_string()))
}

/// Save `manifest` to `path`.
///
/// The write replaces `path`. It does not follow a symlink at `path`.
///
/// # Errors
///
/// [`AssetsError::Io`] for I/O errors.
pub fn save_manifest(path: &Path, manifest: &VendorManifest) -> Result<(), AssetsError> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::create_dir_all(dir)?;
    let json =
        serde_json::to_string_pretty(manifest).map_err(|e| AssetsError::Parse(e.to_string()))?;
    replace_file(dir, path, (json + "\n").as_bytes())
}

// ---------------------------------------------------------------------------
// Download helper
// ---------------------------------------------------------------------------

/// Download `url` and return the bytes.  Delegates to [`crate::http::fetch_bytes`]
/// so progress-bar logic is shared with `autumn setup`.
pub fn download_bytes(url: &str) -> Result<Vec<u8>, AssetsError> {
    Ok(crate::http::fetch_bytes(url)?)
}

/// How [`asset_dest`] handles a directory that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissingDir {
    /// Make the directory.
    Create,
    /// Stop and return `None`.
    Stop,
}

/// Find the full path of `file` under `static_dir`.
///
/// The function examines one part of the path at a time. A symlink is
/// permitted only when its target is in `static_dir`. `static_dir` itself
/// cannot be a symlink: `static -> .` would put `build.rs` in the project
/// root. Returns `None` when a directory does not exist and `missing` is
/// [`MissingDir::Stop`].
fn asset_dest(
    static_dir: &Path,
    file: &AssetPath,
    missing: MissingDir,
) -> Result<Option<PathBuf>, AssetsError> {
    match fs::symlink_metadata(static_dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(file.unsafe_path("the static directory is a symlink"));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(file.unsafe_path("the static directory is not a directory"));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match missing {
            MissingDir::Create => fs::create_dir_all(static_dir)?,
            MissingDir::Stop => return Ok(None),
        },
        Err(e) => return Err(e.into()),
    }
    let root = fs::canonicalize(static_dir)?;

    let (parents, name) = file
        .as_str()
        .rsplit_once('/')
        .unwrap_or(("", file.as_str()));
    let mut dir = root.clone();
    for part in parents.split('/').filter(|part| !part.is_empty()) {
        let next = dir.join(part);
        match fs::symlink_metadata(&next) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match missing {
                MissingDir::Create => {
                    fs::create_dir(&next)?;
                    dir = next;
                }
                MissingDir::Stop => return Ok(None),
            },
            Err(e) => return Err(e.into()),
            Ok(_) => {
                let real = fs::canonicalize(&next)?;
                if !real.starts_with(&root) {
                    return Err(file.unsafe_path("a symlink points outside `static/`"));
                }
                if !real.is_dir() {
                    return Err(file.unsafe_path("a part of the path is not a directory"));
                }
                dir = real;
            }
        }
    }

    let dest = dir.join(name);
    match fs::symlink_metadata(&dest) {
        Ok(meta) if meta.is_dir() => Err(file.unsafe_path("the path is a directory")),
        Ok(_) => Ok(Some(dest)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Some(dest)),
        Err(e) => Err(e.into()),
    }
}

/// Write `bytes` to `file` under `static_dir` and return the full path.
///
/// The write stays in `static_dir`. A symlink in the path must point into
/// `static_dir`. A symlink at the file is replaced. The write does not follow
/// it. On Unix, a new file gets mode `0644`, and an old file keeps its mode.
///
/// # Errors
///
/// [`AssetsError::UnsafePath`] when `static_dir` is a symlink, when a symlink
/// points outside `static_dir`, or when a part of the path has the wrong
/// type. [`AssetsError::Io`] for I/O errors.
pub fn write_asset(
    static_dir: &Path,
    file: &AssetPath,
    bytes: &[u8],
) -> Result<PathBuf, AssetsError> {
    let dest = asset_dest(static_dir, file, MissingDir::Create)?
        .ok_or_else(|| file.unsafe_path("a directory does not exist"))?;
    let dir = dest
        .parent()
        .ok_or_else(|| file.unsafe_path("the path has no directory"))?;
    replace_file(dir, &dest, bytes)?;
    Ok(dest)
}

/// Replace `dest` with `bytes`, through a new temp file in `dir`.
///
/// The temp file has a random name and is made with `O_EXCL`. Thus a planted
/// symlink cannot catch the write. The rename replaces a symlink at `dest`.
/// It does not follow it. A failed write keeps the old file.
fn replace_file(dir: &Path, dest: &Path, bytes: &[u8]) -> Result<(), AssetsError> {
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // The temp file has mode 0600. A web server user cannot read that.
        let mode = fs::symlink_metadata(dest)
            .ok()
            .filter(fs::Metadata::is_file)
            .map_or(0o644, |meta| meta.permissions().mode() & 0o777);
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    tmp.as_file().sync_all()?;
    tmp.persist(dest).map_err(|e| e.error)?;
    Ok(())
}

/// Find `file` under `static_dir` for a read.
///
/// Returns `None` when the file does not exist.
///
/// # Errors
///
/// [`AssetsError::UnsafePath`] when the path leaves `static_dir`, or when
/// the file is not a regular file.
fn asset_for_read(static_dir: &Path, file: &AssetPath) -> Result<Option<PathBuf>, AssetsError> {
    let Some(dest) = asset_dest(static_dir, file, MissingDir::Stop)? else {
        return Ok(None);
    };
    let real = match fs::canonicalize(&dest) {
        Ok(real) => real,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !real.starts_with(fs::canonicalize(static_dir)?) {
        return Err(file.unsafe_path("a symlink points outside `static/`"));
    }
    if !fs::metadata(&real)?.is_file() {
        return Err(file.unsafe_path("the asset is not a regular file"));
    }
    Ok(Some(real))
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
        let read = asset_for_read(static_dir, &asset.file).and_then(|found| {
            found.map_or_else(
                || Err(std::io::Error::from(std::io::ErrorKind::NotFound).into()),
                |path| Ok(fs::read(path)?),
            )
        });
        match read {
            Err(e @ AssetsError::UnsafePath { .. }) => {
                eprintln!("  UNSAFE   {name} — {e}");
                ok = false;
            }
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
    execute_add_with(
        spec,
        url_override,
        Path::new(VENDOR_MANIFEST_PATH),
        Path::new("static"),
        download_bytes,
    )
}

/// Testable core of `run_add`. Tests give a temp directory and a fake `fetch`.
///
/// All checks run before the download.
fn execute_add_with(
    spec: &str,
    url_override: Option<&str>,
    manifest_path: &Path,
    static_dir: &Path,
    fetch: impl Fn(&str) -> Result<Vec<u8>, AssetsError>,
) -> Result<(), AssetsError> {
    let (name, version) = parse_spec(spec)?;
    let (source, file) = resolve_source(&name, &version, url_override)?;
    let file = AssetPath::new(file)?;
    check_source(&source, SourcePolicy::HttpOrHttps)?;
    let mut manifest = load_manifest(manifest_path)?;
    asset_dest(static_dir, &file, MissingDir::Stop)?;

    println!("Downloading {name} {version} from {source}...");
    let bytes = fetch(&source)?;
    let sri = compute_sri(&bytes);
    write_asset(static_dir, &file, &bytes)?;

    manifest.assets.insert(
        name.clone(),
        VendorAsset {
            version: version.clone(),
            source,
            file,
            integrity: sri.clone(),
        },
    );
    save_manifest(manifest_path, &manifest)?;

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

/// One asset that `update` will download.
struct PendingUpdate {
    name: String,
    version: String,
    source: String,
    file: AssetPath,
}

impl PendingUpdate {
    fn from_manifest(name: &str, asset: &VendorAsset) -> Self {
        Self {
            name: name.to_owned(),
            version: asset.version.clone(),
            source: asset.source.clone(),
            file: asset.file.clone(),
        }
    }
}

/// Testable core of `run_update`: resolves, downloads, and re-pins assets.
///
/// Tests give a temp directory and a fake `fetch`. All checks run before the
/// first download. All downloads finish before the first write.
fn execute_update_with_manifest(
    name_spec: Option<&str>,
    manifest_path: &Path,
    static_dir: &Path,
    fetch: impl Fn(&str) -> Result<Vec<u8>, AssetsError>,
) -> Result<(), AssetsError> {
    let mut manifest = load_manifest(manifest_path)?;

    let pending: Vec<PendingUpdate> = match name_spec {
        None => manifest
            .assets
            .iter()
            .map(|(name, asset)| PendingUpdate::from_manifest(name, asset))
            .collect(),
        Some(spec) if spec.contains('@') => {
            // Re-pin to a different version.
            let (name, version) = parse_spec(spec)?;
            if !registry().iter().any(|(k, _)| *k == name.as_str()) {
                // We cannot find the URL of a new version of a `--url` asset.
                return Err(AssetsError::UpdateFailed(format!(
                    "{name} was added with --url and is not in the built-in registry; \
                     re-pin with `autumn assets add {name}@{version} --url <url>`"
                )));
            }
            let (source, file) = resolve_source(&name, &version, None)?;
            vec![PendingUpdate {
                name,
                version,
                source,
                file: AssetPath::new(file)?,
            }]
        }
        Some(spec) => {
            // Refresh the pinned version.
            let asset = manifest
                .assets
                .get(spec)
                .ok_or_else(|| AssetsError::UnknownPackage(spec.to_owned()))?;
            vec![PendingUpdate::from_manifest(spec, asset)]
        }
    };

    if pending.is_empty() {
        println!("No vendored assets to update.");
        return Ok(());
    }

    for item in &pending {
        check_source(&item.source, SourcePolicy::HttpsOnly)?;
        asset_dest(static_dir, &item.file, MissingDir::Stop)?;
    }

    let mut downloads = Vec::with_capacity(pending.len());
    for item in pending {
        println!(
            "Updating {} → {} from {}...",
            item.name, item.version, item.source
        );
        let bytes = fetch(&item.source)?;
        downloads.push((item, bytes));
    }

    for (item, bytes) in downloads {
        let sri = compute_sri(&bytes);
        write_asset(static_dir, &item.file, &bytes)?;
        println!("  Updated {} {}  {sri}", item.name, item.version);
        manifest.assets.insert(
            item.name,
            VendorAsset {
                version: item.version,
                source: item.source,
                file: item.file,
                integrity: sri,
            },
        );
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

    // --- Update of --url packages ---

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

    // --- File name from URL ---

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

    // --- CLI and framework schemas match ---

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

    /// This fetch fails the test. The command must stop before a download.
    fn no_network(url: &str) -> Result<Vec<u8>, AssetsError> {
        panic!("unexpected download of {url}");
    }

    /// This fetch returns bytes made from the URL.
    #[expect(clippy::unnecessary_wraps)]
    fn fake_fetch(url: &str) -> Result<Vec<u8>, AssetsError> {
        Ok(format!("// from {url}\n").into_bytes())
    }

    /// A project root with an empty `static/` and a sibling `outside/`.
    struct Project {
        root: tempfile::TempDir,
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
            root,
            static_dir,
            outside,
            manifest,
        }
    }

    /// Write a manifest with no check on `source` or `file`.
    /// Each entry is `(name, source, file)`.
    fn write_raw_manifest(path: &Path, entries: &[(&str, &str, &str)]) {
        let assets: serde_json::Map<String, serde_json::Value> = entries
            .iter()
            .map(|(name, source, file)| {
                let asset = serde_json::json!({
                    "version": "1.0.0",
                    "source": source,
                    "file": file,
                    "integrity": "sha384-abc",
                });
                ((*name).to_owned(), asset)
            })
            .collect();
        let json = serde_json::json!({ "version": "1", "assets": assets });
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

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    const SAFE_PATHS: &[&str] = &[
        "htmx.min.js",
        "js/htmx.min.js",
        "js/vendor/alpine.min.js",
        "css/site.css",
        ".well-known/x.js",
        "js/v1.2.3/lib.js",
        "js/console.js",
        "js/null.js",
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
        "\\\\?\\C:\\x.js",
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
        "js/CON .js",
        "js/COM1",
        "js/com\u{b9}.js",
        "js/lpt9.txt",
        "js/CONIN$",
        "js/conout$.js",
        "js/clock$",
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
        let entry = ("evil", "https://example.com/x.js", "../outside/x.js");
        write_raw_manifest(&p.manifest, &[entry]);
        let err = load_manifest(&p.manifest).expect_err("escaping file must not load");
        assert!(matches!(err, AssetsError::Parse(_)), "{err:?}");
        assert!(err.to_string().contains("unsafe asset path"), "{err}");
    }

    #[test]
    fn verify_rejects_escaping_file() {
        let p = project();
        let entry = ("evil", "https://example.com/x.js", "../outside/x.js");
        write_raw_manifest(&p.manifest, &[entry]);
        let err = verify_all(&p.manifest, &p.static_dir).expect_err("must fail");
        assert!(matches!(err, AssetsError::Parse(_)), "{err:?}");
    }

    #[test]
    fn update_all_rejects_escaping_file_before_download() {
        let p = project();
        let abs = p.outside.join("pwn.js");
        let abs = abs.to_str().unwrap().to_owned();
        for file in ["../outside/pwn.js", "js/../../outside/pwn.js", abs.as_str()] {
            write_raw_manifest(
                &p.manifest,
                &[("evil", "https://attacker.example/x.js", file)],
            );
            let before = fs::read(&p.manifest).unwrap();
            let err = execute_update_with_manifest(None, &p.manifest, &p.static_dir, no_network)
                .expect_err(&format!("{file:?} must be rejected"));
            assert!(
                err.to_string().contains("unsafe asset path"),
                "{file:?}: {err}"
            );
            assert_dir_empty(&p.outside);
            assert_eq!(fs::read(&p.manifest).unwrap(), before, "manifest changed");
        }
    }

    #[test]
    fn named_refresh_rejects_escaping_file_before_download() {
        let p = project();
        let entry = ("evil", "https://attacker.example/x.js", "../outside/pwn.js");
        write_raw_manifest(&p.manifest, &[entry]);
        let err =
            execute_update_with_manifest(Some("evil"), &p.manifest, &p.static_dir, no_network)
                .expect_err("escaping file must be rejected");
        assert!(err.to_string().contains("unsafe asset path"), "{err}");
        assert_dir_empty(&p.outside);
    }

    #[test]
    fn check_source_accepts_https_and_explicit_http() {
        let cdn = "https://cdn.jsdelivr.net/npm/htmx.org@2.0.4/dist/htmx.min.js";
        check_source(cdn, SourcePolicy::HttpsOnly).unwrap();
        check_source(cdn, SourcePolicy::HttpOrHttps).unwrap();
        check_source("http://localhost:8080/lib.js", SourcePolicy::HttpOrHttps).unwrap();
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
            for policy in [SourcePolicy::HttpsOnly, SourcePolicy::HttpOrHttps] {
                let err = check_source(url, policy).expect_err(&format!("{url:?} must fail"));
                assert!(
                    matches!(err, AssetsError::UnsafeSource { .. }),
                    "{url:?}: {err:?}"
                );
            }
        }
    }

    #[test]
    fn check_source_https_only_rejects_http() {
        let url = "http://169.254.169.254/latest/meta-data/";
        let err = check_source(url, SourcePolicy::HttpsOnly).expect_err("http must fail");
        assert!(matches!(err, AssetsError::UnsafeSource { .. }), "{err:?}");
    }

    #[test]
    fn update_rejects_bad_source_before_download() {
        let p = project();
        for source in [
            "file:///etc/passwd",
            "http://169.254.169.254/latest/meta-data/",
        ] {
            write_raw_manifest(&p.manifest, &[("evil", source, "js/evil.js")]);
            let err = execute_update_with_manifest(None, &p.manifest, &p.static_dir, no_network)
                .expect_err(&format!("{source:?} must be rejected"));
            assert!(matches!(err, AssetsError::UnsafeSource { .. }), "{err:?}");
            assert!(!p.static_dir.join("js").exists());
        }
    }

    #[test]
    fn update_all_writes_under_static_and_keeps_source() {
        let p = project();
        let source = "https://example.com/htmx.js";
        write_raw_manifest(&p.manifest, &[("htmx", source, "js/htmx.min.js")]);
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
        let entry = ("htmx", "https://example.com/old.js", "js/htmx.min.js");
        write_raw_manifest(&p.manifest, &[entry]);
        execute_update_with_manifest(Some("htmx@2.0.5"), &p.manifest, &p.static_dir, fake_fetch)
            .unwrap();
        let manifest = load_manifest(&p.manifest).unwrap();
        let asset = &manifest.assets["htmx"];
        assert!(asset.source.contains("htmx.org@2.0.5"), "{}", asset.source);
        assert_eq!(asset.file.as_str(), "js/htmx.min.js");
        assert!(p.static_dir.join("js/htmx.min.js").is_file());
        assert_dir_empty(&p.outside);
    }

    #[test]
    fn update_with_no_assets_writes_nothing() {
        let p = project();
        execute_update_with_manifest(None, &p.manifest, &p.static_dir, no_network).unwrap();
        assert!(!p.manifest.exists(), "update must not make a manifest");
        assert_dir_empty(&p.static_dir);
    }

    #[test]
    fn add_writes_under_static_and_records_asset() {
        let p = project();
        let url = "https://example.com/dist/my-lib.min.js";
        execute_add_with(
            "my-lib@1.0.0",
            Some(url),
            &p.manifest,
            &p.static_dir,
            fake_fetch,
        )
        .unwrap();
        let bytes = fs::read(p.static_dir.join("js/my-lib.min.js")).unwrap();
        assert_eq!(bytes, fake_fetch(url).unwrap());
        let manifest = load_manifest(&p.manifest).unwrap();
        let asset = &manifest.assets["my-lib"];
        assert_eq!(asset.source, url);
        assert_eq!(asset.file.as_str(), "js/my-lib.min.js");
        assert_eq!(asset.integrity, compute_sri(&bytes));
    }

    #[test]
    fn add_accepts_explicit_http_url() {
        let p = project();
        let url = "http://localhost:8080/my-lib.js";
        execute_add_with(
            "my-lib@1.0.0",
            Some(url),
            &p.manifest,
            &p.static_dir,
            fake_fetch,
        )
        .unwrap();
        assert!(p.static_dir.join("js/my-lib.js").is_file());
    }

    #[test]
    fn add_checks_manifest_before_download() {
        let p = project();
        let entry = ("evil", "https://example.com/x.js", "../outside/x.js");
        write_raw_manifest(&p.manifest, &[entry]);
        let err = execute_add_with("htmx@2.0.4", None, &p.manifest, &p.static_dir, no_network)
            .expect_err("bad manifest must stop add");
        assert!(err.to_string().contains("unsafe asset path"), "{err}");
        assert!(!p.static_dir.join("js").exists());
    }

    #[test]
    fn add_rejects_unsafe_url_basename_before_download() {
        let p = project();
        let url = "https://example.com/con.js";
        let err = execute_add_with(
            "lib@1.0.0",
            Some(url),
            &p.manifest,
            &p.static_dir,
            no_network,
        )
        .expect_err("device name must be rejected");
        assert!(matches!(err, AssetsError::UnsafePath { .. }), "{err:?}");
        let err = execute_add_with(
            "lib@1.0.0",
            Some("file:///etc/x.js"),
            &p.manifest,
            &p.static_dir,
            no_network,
        )
        .expect_err("file: URL must be rejected");
        assert!(matches!(err, AssetsError::UnsafeSource { .. }), "{err:?}");
        assert!(!p.manifest.exists());
    }

    #[test]
    fn write_asset_leaves_no_temp_file() {
        let tmp_dir = tempfile::TempDir::new().unwrap();
        let file = AssetPath::new("out.js").unwrap();
        let dest = write_asset(tmp_dir.path(), &file, b"data").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"data");
        let entries: Vec<_> = fs::read_dir(tmp_dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "only the asset stays: {entries:?}");
    }

    #[cfg(unix)]
    #[test]
    fn write_asset_gives_new_file_mode_0644() {
        let p = project();
        let file = AssetPath::new("js/htmx.min.js").unwrap();
        let dest = write_asset(&p.static_dir, &file, b"x").unwrap();
        assert_eq!(mode(&dest), 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn write_asset_keeps_mode_of_old_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let p = project();
        let dest = p.static_dir.join("htmx.min.js");
        fs::write(&dest, b"old").unwrap();
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o640)).unwrap();
        let file = AssetPath::new("htmx.min.js").unwrap();
        write_asset(&p.static_dir, &file, b"new").unwrap();
        assert_eq!(mode(&dest), 0o640);
        assert_eq!(fs::read(&dest).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn write_asset_rejects_symlink_out_of_static() {
        let p = project();
        std::os::unix::fs::symlink(&p.outside, p.static_dir.join("js")).unwrap();
        for raw in ["js/htmx.min.js", "js/new/dir/htmx.min.js"] {
            let file = AssetPath::new(raw).unwrap();
            let err = write_asset(&p.static_dir, &file, b"evil").expect_err(raw);
            assert!(matches!(err, AssetsError::UnsafePath { .. }), "{err:?}");
            assert_dir_empty(&p.outside);
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_asset_rejects_symlinked_static_root() {
        // `static -> .` would let `build.rs` land in the project root.
        let p = project();
        let link = p.root.path().join("static-link");
        std::os::unix::fs::symlink(p.root.path(), &link).unwrap();
        let file = AssetPath::new("build.rs").unwrap();
        let err = write_asset(&link, &file, b"evil").expect_err("symlinked root");
        assert!(matches!(err, AssetsError::UnsafePath { .. }), "{err:?}");
        assert!(!p.root.path().join("build.rs").exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_asset_follows_symlink_that_stays_in_static() {
        let p = project();
        fs::create_dir_all(p.static_dir.join("vendor")).unwrap();
        std::os::unix::fs::symlink("vendor", p.static_dir.join("js")).unwrap();
        let file = AssetPath::new("js/htmx.min.js").unwrap();
        write_asset(&p.static_dir, &file, b"ok").unwrap();
        assert_eq!(
            fs::read(p.static_dir.join("vendor/htmx.min.js")).unwrap(),
            b"ok"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_asset_replaces_symlinked_file_and_does_not_follow_it() {
        let p = project();
        let victim = p.outside.join("victim");
        fs::write(&victim, b"original").unwrap();
        fs::create_dir_all(p.static_dir.join("js")).unwrap();
        let dest = p.static_dir.join("js/htmx.min.js");
        std::os::unix::fs::symlink(&victim, &dest).unwrap();
        let file = AssetPath::new("js/htmx.min.js").unwrap();
        write_asset(&p.static_dir, &file, b"new").unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"original");
        assert!(
            !fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&dest).unwrap(), b"new");
    }

    #[cfg(unix)]
    #[test]
    fn write_asset_does_not_write_through_planted_temp_symlink() {
        // A fixed temp name next to the asset can be a planted symlink.
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
    fn update_checks_every_path_before_download() {
        // The second asset is unsafe. The first must not change.
        let p = project();
        std::os::unix::fs::symlink(&p.outside, p.static_dir.join("evil")).unwrap();
        write_raw_manifest(
            &p.manifest,
            &[
                ("a", "https://example.com/a.js", "js/a.js"),
                ("b", "https://example.com/b.js", "evil/b.js"),
            ],
        );
        let before = fs::read(&p.manifest).unwrap();
        let err = execute_update_with_manifest(None, &p.manifest, &p.static_dir, no_network)
            .expect_err("symlinked dir must be rejected");
        assert!(matches!(err, AssetsError::UnsafePath { .. }), "{err:?}");
        assert!(!p.static_dir.join("js").exists());
        assert_dir_empty(&p.outside);
        assert_eq!(fs::read(&p.manifest).unwrap(), before);
    }

    #[cfg(unix)]
    #[test]
    fn manifest_symlink_is_not_read_or_written() {
        let p = project();
        let target = p.outside.join("pwned.toml");
        std::os::unix::fs::symlink(&target, &p.manifest).unwrap();
        let err = execute_update_with_manifest(None, &p.manifest, &p.static_dir, no_network)
            .expect_err("symlinked manifest must be rejected");
        assert!(matches!(err, AssetsError::UnsafeManifest { .. }), "{err:?}");
        assert!(!target.exists());
        assert!(verify_all(&p.manifest, &p.static_dir).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn save_manifest_replaces_symlink_and_does_not_follow_it() {
        let p = project();
        let target = p.outside.join("victim.json");
        fs::write(&target, b"original").unwrap();
        std::os::unix::fs::symlink(&target, &p.manifest).unwrap();
        save_manifest(&p.manifest, &VendorManifest::default()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert!(
            !fs::symlink_metadata(&p.manifest)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_does_not_read_through_symlink_out_of_static() {
        let p = project();
        let secret = p.outside.join("id_rsa");
        fs::write(&secret, b"secret").unwrap();
        fs::create_dir_all(p.static_dir.join("js")).unwrap();
        std::os::unix::fs::symlink(&secret, p.static_dir.join("js/htmx.min.js")).unwrap();
        let mut manifest = VendorManifest::default();
        manifest.assets.insert(
            "htmx".into(),
            VendorAsset {
                version: "2.0.4".into(),
                source: "https://example.com".into(),
                file: AssetPath::new("js/htmx.min.js").unwrap(),
                integrity: compute_sri(b"secret"),
            },
        );
        save_manifest(&p.manifest, &manifest).unwrap();
        // The hash matches the secret. A read through the link reports OK.
        assert!(verify_all(&p.manifest, &p.static_dir).is_err());
    }

    proptest::proptest! {
        /// Every accepted path joins to a path under the root.
        #[test]
        fn accepted_paths_stay_under_root(raw in "[a-zA-Z0-9._ :/\\\\$-]{0,24}") {
            if let Ok(path) = AssetPath::new(raw.as_str()) {
                let root = Path::new("root");
                let joined = root.join(&path);
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
