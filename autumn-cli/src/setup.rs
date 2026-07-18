//! Tailwind CSS CLI download and verification for `autumn setup`.
//!
//! Downloads the correct platform-specific Tailwind CSS v4.1.0 standalone binary,
//! verifies its SHA-256 checksum against the `sha256sums.txt` file published with
//! each release, and installs it to `target/autumn/tailwindcss`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Pinned Tailwind CSS release version.
const TAILWIND_VERSION: &str = "v4.1.0";

/// Base URL for Tailwind CSS release assets.
const RELEASE_BASE_URL: &str = "https://github.com/tailwindlabs/tailwindcss/releases/download";

/// Errors that can occur during the setup process.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    /// The current OS/architecture combination is not supported.
    #[error("unsupported platform: os={0}, arch={1}")]
    UnsupportedPlatform(String, String),

    /// A network request failed.
    #[error("download failed: {0}")]
    Download(#[from] reqwest::Error),

    /// The downloaded binary does not match its expected checksum.
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// The checksum we expected (from `sha256sums.txt`).
        expected: String,
        /// The checksum we actually computed.
        actual: String,
    },

    /// An I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Failed to parse `sha256sums.txt`.
    #[error("failed to parse checksum file: {0}")]
    ChecksumParse(String),
}

/// Run the `autumn setup` subcommand.
///
/// Downloads Tailwind CSS to `target/autumn/tailwindcss` (or `.exe` on Windows).
/// If the binary already exists and `force` is false, exits early.
pub fn run(force: bool) {
    if let Err(e) = execute(force) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

/// Inner implementation so tests can call this without `process::exit`.
fn execute(force: bool) -> Result<(), SetupError> {
    let binary_name = detect_platform(std::env::consts::OS, std::env::consts::ARCH)?;
    let install_dir = PathBuf::from("target/autumn");
    let dest = install_path(&install_dir);

    if !force && dest.exists() {
        println!("Tailwind CLI already installed at {}", dest.display());
        return Ok(());
    }

    fs::create_dir_all(&install_dir)?;

    let download_url = format!("{RELEASE_BASE_URL}/{TAILWIND_VERSION}/{binary_name}");
    let checksums_url = format!("{RELEASE_BASE_URL}/{TAILWIND_VERSION}/sha256sums.txt");

    println!("Downloading Tailwind CSS {TAILWIND_VERSION} ({binary_name})...");

    let expected_hash = fetch_expected_checksum(&checksums_url, &binary_name)?;
    let tmp_path = install_dir.join(".tailwindcss.tmp");
    download_with_progress(&download_url, &tmp_path)?;

    let actual_hash = sha256_file(&tmp_path)?;
    verify_checksum(&expected_hash, &actual_hash)?;

    fs::rename(&tmp_path, &dest)?;

    #[cfg(unix)]
    set_executable(&dest)?;

    println!("Tailwind CLI installed to {}", dest.display());
    Ok(())
}

/// Return the Tailwind release asset name for the given OS and architecture.
pub fn detect_platform(os: &str, arch: &str) -> Result<String, SetupError> {
    let platform = match (os, arch) {
        ("linux", "x86_64") => "tailwindcss-linux-x64",
        ("linux", "aarch64") => "tailwindcss-linux-arm64",
        ("macos", "x86_64") => "tailwindcss-macos-x64",
        ("macos", "aarch64") => "tailwindcss-macos-arm64",
        ("windows", "x86_64") => "tailwindcss-windows-x64.exe",
        _ => {
            return Err(SetupError::UnsupportedPlatform(
                os.to_owned(),
                arch.to_owned(),
            ));
        }
    };
    Ok(platform.to_owned())
}

fn install_path(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        dir.join("tailwindcss.exe")
    } else {
        dir.join("tailwindcss")
    }
}

fn fetch_expected_checksum(url: &str, binary_name: &str) -> Result<String, SetupError> {
    let body = reqwest::blocking::get(url)?.error_for_status()?.text()?;
    parse_checksum_file(&body, binary_name)
}

pub fn parse_checksum_file(body: &str, binary_name: &str) -> Result<String, SetupError> {
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hash_part = parts.next().unwrap_or_default();
        let file_part = parts.next().unwrap_or_default();

        let file_part = file_part.strip_prefix("./").unwrap_or(file_part);
        if file_part == binary_name {
            if hash_part.len() != 64 || !hash_part.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(SetupError::ChecksumParse(format!(
                    "expected 64-char hex digest, got: {hash_part}"
                )));
            }
            return Ok(hash_part.to_ascii_lowercase());
        }
    }

    Err(SetupError::ChecksumParse(format!(
        "no checksum found for {binary_name}"
    )))
}

pub fn sha256_file(path: &Path) -> Result<String, SetupError> {
    let data = fs::read(path)?;
    Ok(sha256_bytes(&data))
}

pub fn sha256_bytes(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex::encode(digest)
}

pub fn verify_checksum(expected: &str, actual: &str) -> Result<(), SetupError> {
    if expected == actual {
        Ok(())
    } else {
        Err(SetupError::ChecksumMismatch {
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        })
    }
}

fn download_with_progress(url: &str, dest: &Path) -> Result<(), SetupError> {
    let bytes = crate::http::fetch_bytes(url)?;
    let mut file = fs::File::create(dest)?;
    file.write_all(&bytes)?;
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), SetupError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    let mode = perms.mode() | 0o111;
    perms.set_mode(mode);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_platform_supported_combinations() {
        let cases = [
            ("linux", "x86_64", "tailwindcss-linux-x64"),
            ("linux", "aarch64", "tailwindcss-linux-arm64"),
            ("macos", "x86_64", "tailwindcss-macos-x64"),
            ("macos", "aarch64", "tailwindcss-macos-arm64"),
            ("windows", "x86_64", "tailwindcss-windows-x64.exe"),
        ];

        for (os, arch, expected) in cases {
            let name = detect_platform(os, arch)
                .unwrap_or_else(|_| panic!("should be supported: {os} {arch}"));
            assert_eq!(name, expected);
        }
    }

    #[test]
    fn detect_unsupported_os() {
        let err = detect_platform("freebsd", "x86_64").unwrap_err();
        assert!(matches!(err, SetupError::UnsupportedPlatform(_, _)));
        assert!(err.to_string().contains("freebsd"));
    }

    #[test]
    fn detect_unsupported_arch() {
        let err = detect_platform("linux", "riscv64").unwrap_err();
        assert!(matches!(err, SetupError::UnsupportedPlatform(_, _)));
        assert!(err.to_string().contains("riscv64"));
    }

    #[test]
    fn sha256_known_value() {
        let hash = sha256_bytes(b"");
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_hello_world() {
        let hash = sha256_bytes(b"hello world\n");
        assert_eq!(
            hash,
            "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
        );
    }

    #[test]
    fn verify_checksum_match() {
        let hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert!(verify_checksum(hash, hash).is_ok());
    }

    #[test]
    fn verify_checksum_mismatch() {
        let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let actual = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let err = verify_checksum(expected, actual).unwrap_err();
        assert!(matches!(err, SetupError::ChecksumMismatch { .. }));
        assert!(err.to_string().contains(expected));
        assert!(err.to_string().contains(actual));
    }

    #[test]
    fn parse_finds_correct_binary() {
        let body = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  ./tailwindcss-linux-x64
a948904f2f0f479b8f8564e9d7a8f22e32d13e73845f1b0ea0e2975a02c8b87f  ./tailwindcss-windows-x64.exe
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  ./tailwindcss-macos-arm64
";
        let hash = parse_checksum_file(body, "tailwindcss-windows-x64.exe").unwrap();
        assert_eq!(
            hash,
            "a948904f2f0f479b8f8564e9d7a8f22e32d13e73845f1b0ea0e2975a02c8b87f"
        );
    }

    #[test]
    fn parse_works_without_prefix() {
        let body = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  tailwindcss-linux-x64\n";
        let hash = parse_checksum_file(body, "tailwindcss-linux-x64").unwrap();
        assert_eq!(
            hash,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn parse_uppercase_hex() {
        let body = "A948904F2F0F479B8F8564E9D7A8F22E32D13E73845F1B0EA0E2975A02C8B87F  tailwindcss-linux-x64\n";
        let hash = parse_checksum_file(body, "tailwindcss-linux-x64").unwrap();
        assert_eq!(
            hash,
            "a948904f2f0f479b8f8564e9d7a8f22e32d13e73845f1b0ea0e2975a02c8b87f"
        );
    }

    #[test]
    fn parse_empty_file_fails() {
        let err = parse_checksum_file("", "tailwindcss-linux-x64").unwrap_err();
        assert!(matches!(err, SetupError::ChecksumParse(_)));
    }

    #[test]
    fn parse_missing_binary_fails() {
        let body = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  tailwindcss-linux-x64\n";
        let err = parse_checksum_file(body, "tailwindcss-windows-x64.exe").unwrap_err();
        assert!(matches!(err, SetupError::ChecksumParse(_)));
        assert!(err.to_string().contains("tailwindcss-windows-x64.exe"));
    }

    #[test]
    fn parse_non_hex_fails() {
        let body = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz  tailwindcss-linux-x64\n";
        let err = parse_checksum_file(body, "tailwindcss-linux-x64").unwrap_err();
        assert!(matches!(err, SetupError::ChecksumParse(_)));
    }

    #[test]
    fn sha256_file_matches_bytes() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        fs::write(tmp.path(), b"test data").unwrap();
        let file_hash = sha256_file(tmp.path()).unwrap();
        let byte_hash = sha256_bytes(b"test data");
        assert_eq!(file_hash, byte_hash);
    }

    #[test]
    fn install_path_is_correct() {
        let dir = Path::new("target/autumn");
        let path = install_path(dir);
        if cfg!(windows) {
            assert_eq!(path, PathBuf::from("target/autumn/tailwindcss.exe"));
        } else {
            assert_eq!(path, PathBuf::from("target/autumn/tailwindcss"));
        }
    }

    #[test]
    #[ignore = "requires network access to download Tailwind binary"]
    fn download_and_verify_tailwind() {
        let tmp = tempfile::TempDir::new().unwrap();
        let install_dir = tmp.path().join("target/autumn");
        fs::create_dir_all(&install_dir).unwrap();

        let binary_name = detect_platform(std::env::consts::OS, std::env::consts::ARCH).unwrap();
        let download_url = format!("{RELEASE_BASE_URL}/{TAILWIND_VERSION}/{binary_name}");
        let checksums_url = format!("{RELEASE_BASE_URL}/{TAILWIND_VERSION}/sha256sums.txt");

        let expected_hash = fetch_expected_checksum(&checksums_url, &binary_name).unwrap();
        let dest = install_dir.join(".tailwindcss.tmp");
        download_with_progress(&download_url, &dest).unwrap();

        let actual_hash = sha256_file(&dest).unwrap();
        verify_checksum(&expected_hash, &actual_hash).unwrap();

        let meta = fs::metadata(&dest).unwrap();
        assert!(
            meta.len() > 1_000_000,
            "binary too small: {} bytes",
            meta.len()
        );
    }
}
