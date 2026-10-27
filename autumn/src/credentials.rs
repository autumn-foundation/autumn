//! Encrypted credentials store for production secrets.
//!
//! Provides AES-256-GCM encrypted TOML credential files, master key resolution,
//! and a typed accessor integrated with [`crate::config::AutumnConfig`].
//!
//! # File format
//!
//! Encrypted files use the following binary layout:
//!
//! ```text
//! [version: 1 byte = 0x01][nonce: 12 bytes][ciphertext + GCM tag: N + 16 bytes]
//! ```
//!
//! The master key is 32 bytes (256 bits) encoded as 64 lowercase hex characters.
//! Encrypted files are stored at `config/credentials/<env>.toml.enc`.
//!
//! # Master key resolution
//!
//! Keys are resolved in this order (first found wins):
//! 1. `AUTUMN_MASTER_KEY` environment variable
//! 2. `config/master.key` file
//!
//! Both sources must supply a 64-character hex string encoding a 32-byte key.

use std::path::{Path, PathBuf};

use thiserror::Error;

const FORMAT_VERSION: u8 = 0x01;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const KEY_HEX_LEN: usize = KEY_LEN * 2;

/// Errors produced by the credentials subsystem.
#[derive(Debug, Error)]
pub enum CredentialsError {
    /// No master key was found in any expected location.
    #[error("no master key found; tried: AUTUMN_MASTER_KEY env var, config/master.key file")]
    NoKeyFound,

    /// A key source was found but the decryption operation failed.
    #[error("decryption failed using key from {key_source}: invalid key or corrupted ciphertext")]
    DecryptionFailed {
        /// Human-readable description of where the key came from.
        key_source: String,
    },

    /// The key material was found but is not valid hex or wrong length.
    #[error("invalid master key in {key_source}: expected 64 hex characters, got {len}")]
    InvalidKeyFormat {
        /// Where the bad key was found.
        key_source: String,
        /// Actual length encountered.
        len: usize,
    },

    /// The encrypted file has an unrecognised format version.
    #[error("unsupported credentials file version: {0:#04x}")]
    UnsupportedVersion(u8),

    /// The encrypted file is too short to contain the version byte + nonce.
    #[error("credentials file is truncated or corrupt")]
    FileTruncated,

    /// I/O error reading a credentials or key file.
    #[error("credentials I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// TOML parse error after decryption.
    #[error("credentials TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),
}

/// A resolved, validated 32-byte AES-256 master key with its provenance.
#[derive(Clone)]
pub struct MasterKey {
    bytes: [u8; KEY_LEN],
    /// Human-readable description of where this key came from.
    pub source: String,
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MasterKey")
            .field("source", &self.source)
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

impl MasterKey {
    fn from_hex(hex_str: &str, key_source: String) -> Result<Self, CredentialsError> {
        let hex_str = hex_str.trim();
        if hex_str.len() != KEY_HEX_LEN {
            return Err(CredentialsError::InvalidKeyFormat {
                key_source,
                len: hex_str.len(),
            });
        }
        let decoded = hex::decode(hex_str).map_err(|_| CredentialsError::InvalidKeyFormat {
            key_source: key_source.clone(),
            len: hex_str.len(),
        })?;
        let mut bytes = [0u8; KEY_LEN];
        bytes.copy_from_slice(&decoded);
        Ok(Self {
            bytes,
            source: key_source,
        })
    }

    /// Construct a `MasterKey` from a hex string (public helper for tests and tooling).
    ///
    /// # Errors
    ///
    /// Returns an error if `hex_str` is not a valid 64-character hex string.
    pub fn from_hex_pub(hex_str: &str) -> Result<Self, CredentialsError> {
        Self::from_hex(hex_str, "supplied hex string".to_owned())
    }

    /// Generate a new random master key (used during `autumn new` scaffolding).
    ///
    /// # Panics
    ///
    /// Panics if the operating system's random number generator is unavailable.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; KEY_LEN];
        getrandom::fill(&mut bytes).expect("OS RNG failed");
        Self {
            bytes,
            source: "generated".to_owned(),
        }
    }

    /// Encode the key as a 64-character lowercase hex string for storage.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.bytes)
    }
}

/// Resolve the master key from the environment or key file.
///
/// Resolution order (first found wins):
/// 1. `AUTUMN_MASTER_KEY` environment variable
/// 2. `config/master.key` file (relative to `base_dir`)
///
/// # Errors
///
/// Returns `Err(CredentialsError::NoKeyFound)` if neither source is present.
/// Returns `Err(CredentialsError::InvalidKeyFormat)` if a source contains invalid hex.
/// Returns `Err(CredentialsError::Io)` if the key file cannot be read.
pub fn resolve_master_key(base_dir: &Path) -> Result<MasterKey, CredentialsError> {
    resolve_master_key_with_env(base_dir, &OsEnvReader)
}

trait EnvReader {
    fn var(&self, key: &str) -> Option<String>;
}

struct OsEnvReader;
impl EnvReader for OsEnvReader {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

fn resolve_master_key_with_env(
    base_dir: &Path,
    env: &dyn EnvReader,
) -> Result<MasterKey, CredentialsError> {
    if let Some(val) = env.var("AUTUMN_MASTER_KEY") {
        return MasterKey::from_hex(&val, "AUTUMN_MASTER_KEY env var".to_owned());
    }

    let key_path = base_dir.join("config/master.key");
    if key_path.exists() {
        let contents = std::fs::read_to_string(&key_path)?;
        return MasterKey::from_hex(
            contents.trim(),
            format!("config/master.key file ({})", key_path.display()),
        );
    }

    Err(CredentialsError::NoKeyFound)
}

/// Encrypt `plaintext` with AES-256-GCM using `key`.
///
/// Returns the binary ciphertext in the documented format:
/// `[0x01][12-byte nonce][ciphertext + 16-byte GCM tag]`
///
/// # Panics
///
/// Panics if the OS RNG is unavailable.
#[must_use]
pub fn encrypt(key: &MasterKey, plaintext: &[u8]) -> Vec<u8> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};

    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes).expect("OS RNG failed");

    let cipher = Aes256Gcm::new_from_slice(&key.bytes).expect("key is always 32 bytes");
    let nonce = <&Nonce<_>>::from(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .expect("AES-GCM encryption cannot fail for valid inputs");

    let mut output = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
    output.push(FORMAT_VERSION);
    output.extend_from_slice(&nonce_bytes);
    output.extend_from_slice(&ciphertext);
    output
}

/// Decrypt `ciphertext` bytes produced by [`encrypt`].
///
/// # Errors
///
/// # Errors
///
/// - [`CredentialsError::FileTruncated`] if the data is too short.
/// - [`CredentialsError::UnsupportedVersion`] for unknown version bytes.
/// - [`CredentialsError::DecryptionFailed`] if the key is wrong or data is corrupted.
///
/// # Panics
///
/// Panics if the cipher cannot be constructed (should never happen; key is always 32 bytes).
pub fn decrypt(key: &MasterKey, ciphertext: &[u8]) -> Result<Vec<u8>, CredentialsError> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};

    if ciphertext.len() < 1 + NONCE_LEN {
        return Err(CredentialsError::FileTruncated);
    }

    let version = ciphertext[0];
    if version != FORMAT_VERSION {
        return Err(CredentialsError::UnsupportedVersion(version));
    }

    let nonce_bytes = &ciphertext[1..=NONCE_LEN];
    let data = &ciphertext[1 + NONCE_LEN..];

    let cipher = Aes256Gcm::new_from_slice(&key.bytes).expect("key is always 32 bytes");
    let nonce = <&Nonce<_>>::from(nonce_bytes);

    cipher
        .decrypt(nonce, data)
        .map_err(|_| CredentialsError::DecryptionFailed {
            key_source: key.source.clone(),
        })
}

/// An in-memory store of decrypted credentials loaded from a TOML file.
///
/// Constructed by [`load_credentials`] and accessible from `AutumnConfig`
/// via `config.credentials()`.
#[derive(Debug, Clone, Default)]
pub struct CredentialsStore {
    table: toml::Table,
}

impl CredentialsStore {
    /// Retrieve a credential value by key and deserialize it into type `T`.
    ///
    /// Returns `None` if the key does not exist.
    /// Returns `None` if deserialization into `T` fails.
    #[must_use]
    pub fn get<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.table
            .get(key)
            .and_then(|v| T::deserialize(v.clone()).ok())
    }

    /// Returns `true` if the store contains no credentials.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// Returns the number of top-level credential keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.table.len()
    }

    /// Returns an iterator over all top-level credential key names.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.table.keys().map(String::as_str)
    }
}

/// Load and decrypt `config/credentials/<env>.toml.enc` from `base_dir`.
///
/// If the file does not exist, returns an empty [`CredentialsStore`] so that
/// existing apps without a credentials directory continue to boot unchanged.
///
/// # Errors
///
/// Returns an error if the file exists but:
/// - No master key can be resolved ([`CredentialsError::NoKeyFound`])
/// - The key is invalid ([`CredentialsError::InvalidKeyFormat`])
/// - Decryption fails ([`CredentialsError::DecryptionFailed`])
/// - An I/O error occurs ([`CredentialsError::Io`])
/// - The decrypted content is not valid TOML ([`CredentialsError::Toml`])
pub fn load_credentials(env: &str, base_dir: &Path) -> Result<CredentialsStore, CredentialsError> {
    load_credentials_with_env(env, base_dir, &OsEnvReader)
}

/// Load credentials with an optional master key override (used during config loading
/// so the config system's own `Env` abstraction can supply the master key in tests).
pub(crate) fn load_credentials_with_key_override(
    env_name: &str,
    base_dir: &Path,
    master_key_override: Option<&str>,
) -> Result<CredentialsStore, CredentialsError> {
    struct OverrideEnvReader<'a> {
        key_value: Option<&'a str>,
    }
    impl EnvReader for OverrideEnvReader<'_> {
        fn var(&self, key: &str) -> Option<String> {
            if key == "AUTUMN_MASTER_KEY" {
                return self.key_value.map(ToString::to_string);
            }
            std::env::var(key).ok()
        }
    }
    let reader = OverrideEnvReader {
        key_value: master_key_override,
    };
    load_credentials_with_env(env_name, base_dir, &reader)
}

fn load_credentials_with_env(
    env_name: &str,
    base_dir: &Path,
    env: &dyn EnvReader,
) -> Result<CredentialsStore, CredentialsError> {
    let enc_path = base_dir
        .join("config/credentials")
        .join(format!("{env_name}.toml.enc"));

    if !enc_path.exists() {
        return Ok(CredentialsStore::default());
    }

    let key = resolve_master_key_with_env(base_dir, env)?;
    let ciphertext = std::fs::read(&enc_path)?;
    let plaintext = decrypt(&key, &ciphertext)?;
    let toml_str = String::from_utf8(plaintext).map_err(|_| CredentialsError::FileTruncated)?;
    let table: toml::Table = toml::from_str(&toml_str)?;

    Ok(CredentialsStore { table })
}

/// Path to the encrypted credentials file for a given environment.
#[must_use]
pub fn credentials_path(env: &str, base_dir: &Path) -> PathBuf {
    base_dir
        .join("config/credentials")
        .join(format!("{env}.toml.enc"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    struct MockEnvReader(HashMap<String, String>);

    impl EnvReader for MockEnvReader {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn mock_env(key: &str, val: &str) -> MockEnvReader {
        let mut m = HashMap::new();
        m.insert(key.to_owned(), val.to_owned());
        MockEnvReader(m)
    }

    fn empty_env() -> MockEnvReader {
        MockEnvReader(HashMap::new())
    }

    fn fresh_key() -> MasterKey {
        MasterKey::generate()
    }

    // ── format ────────────────────────────────────────────────────────────────

    #[test]
    fn format_version_byte_is_0x01() {
        let key = fresh_key();
        let ct = encrypt(&key, b"hello");
        assert_eq!(ct[0], 0x01, "first byte must be the version sentinel 0x01");
    }

    #[test]
    fn nonce_is_12_bytes_after_version() {
        let key = fresh_key();
        let ct = encrypt(&key, b"hello");
        assert!(ct.len() > NONCE_LEN, "ciphertext too short");
    }

    // ── roundtrip ─────────────────────────────────────────────────────────────

    #[test]
    fn encrypt_decrypt_roundtrip_returns_original() {
        let key = fresh_key();
        let plain = b"stripe_key = \"sk_test_abc123\"";
        let ct = encrypt(&key, plain);
        let recovered = decrypt(&key, &ct).expect("decryption failed");
        assert_eq!(recovered, plain);
    }

    #[test]
    fn roundtrip_empty_plaintext() {
        let key = fresh_key();
        let ct = encrypt(&key, b"");
        let recovered = decrypt(&key, &ct).expect("decryption of empty plaintext failed");
        assert_eq!(recovered, b"");
    }

    #[test]
    fn roundtrip_large_payload() {
        let key = fresh_key();
        let plain: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let ct = encrypt(&key, &plain);
        let recovered = decrypt(&key, &ct).unwrap();
        assert_eq!(recovered, plain);
    }

    #[test]
    fn two_encryptions_of_same_plaintext_differ_due_to_random_nonce() {
        let key = fresh_key();
        let ct1 = encrypt(&key, b"same");
        let ct2 = encrypt(&key, b"same");
        assert_ne!(ct1, ct2, "nonces must be randomised per encryption");
    }

    // ── wrong key ────────────────────────────────────────────────────────────

    #[test]
    fn wrong_key_returns_decryption_failed() {
        let key1 = fresh_key();
        let key2 = fresh_key();
        let ct = encrypt(&key1, b"secret");
        let err = decrypt(&key2, &ct).unwrap_err();
        assert!(
            matches!(err, CredentialsError::DecryptionFailed { .. }),
            "expected DecryptionFailed, got {err}"
        );
    }

    #[test]
    fn decryption_failed_error_names_key_source() {
        let mut key = fresh_key();
        key.source = "config/master.key file".to_owned();
        let other_key = fresh_key();
        let ct = encrypt(&other_key, b"x");
        let err = decrypt(&key, &ct).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("config/master.key file"),
            "error should name the key source: {msg}"
        );
    }

    // ── truncated / bad version ───────────────────────────────────────────────

    #[test]
    fn too_short_ciphertext_returns_file_truncated() {
        let key = fresh_key();
        let err = decrypt(&key, &[0x01, 0x00]).unwrap_err();
        assert!(matches!(err, CredentialsError::FileTruncated));
    }

    #[test]
    fn unsupported_version_byte_is_rejected() {
        let key = fresh_key();
        let mut buf = vec![0xFF; 1 + NONCE_LEN + 16];
        buf[0] = 0xFF;
        let err = decrypt(&key, &buf).unwrap_err();
        assert!(matches!(err, CredentialsError::UnsupportedVersion(0xFF)));
    }

    // ── master key resolution ────────────────────────────────────────────────

    #[test]
    fn no_key_found_when_env_missing_and_no_file() {
        let tmp = TempDir::new().unwrap();
        let err = resolve_master_key_with_env(tmp.path(), &empty_env()).unwrap_err();
        assert!(
            matches!(err, CredentialsError::NoKeyFound),
            "expected NoKeyFound, got {err}"
        );
    }

    #[test]
    fn no_key_found_error_mentions_both_sources() {
        let tmp = TempDir::new().unwrap();
        let err = resolve_master_key_with_env(tmp.path(), &empty_env()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("AUTUMN_MASTER_KEY"), "{msg}");
        assert!(msg.contains("config/master.key"), "{msg}");
    }

    #[test]
    fn resolves_key_from_env_var() {
        let tmp = TempDir::new().unwrap();
        let key = MasterKey::generate();
        let hex = key.to_hex();
        let env = mock_env("AUTUMN_MASTER_KEY", &hex);
        let resolved = resolve_master_key_with_env(tmp.path(), &env).unwrap();
        assert_eq!(resolved.bytes, key.bytes);
        assert!(
            resolved.source.contains("AUTUMN_MASTER_KEY"),
            "source should mention env var"
        );
    }

    #[test]
    fn resolves_key_from_master_key_file() {
        let tmp = TempDir::new().unwrap();
        let key = MasterKey::generate();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/master.key"), key.to_hex()).unwrap();
        let resolved = resolve_master_key_with_env(tmp.path(), &empty_env()).unwrap();
        assert_eq!(resolved.bytes, key.bytes);
        assert!(
            resolved.source.contains("config/master.key"),
            "source should mention file path"
        );
    }

    #[test]
    fn env_var_takes_precedence_over_key_file() {
        let tmp = TempDir::new().unwrap();
        let env_key = MasterKey::generate();
        let file_key = MasterKey::generate();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/master.key"), file_key.to_hex()).unwrap();
        let env = mock_env("AUTUMN_MASTER_KEY", &env_key.to_hex());
        let resolved = resolve_master_key_with_env(tmp.path(), &env).unwrap();
        assert_eq!(resolved.bytes, env_key.bytes, "env var must win over file");
    }

    #[test]
    fn invalid_hex_in_env_var_returns_error() {
        let tmp = TempDir::new().unwrap();
        let env = mock_env("AUTUMN_MASTER_KEY", "not-valid-hex");
        let err = resolve_master_key_with_env(tmp.path(), &env).unwrap_err();
        assert!(
            matches!(err, CredentialsError::InvalidKeyFormat { .. }),
            "expected InvalidKeyFormat"
        );
    }

    // ── CredentialsStore ────────────────────────────────────────────────────

    #[test]
    fn credentials_store_get_string() {
        let table: toml::Table = toml::from_str("stripe_key = \"sk_test_abc\"\n").unwrap();
        let store = CredentialsStore { table };
        let val: Option<String> = store.get("stripe_key");
        assert_eq!(val.as_deref(), Some("sk_test_abc"));
    }

    #[test]
    fn credentials_store_get_missing_key_returns_none() {
        let store = CredentialsStore::default();
        let val: Option<String> = store.get("nonexistent");
        assert!(val.is_none());
    }

    #[test]
    fn credentials_store_is_empty_when_default() {
        let store = CredentialsStore::default();
        assert!(store.is_empty());
    }

    #[test]
    fn credentials_store_keys_lists_top_level() {
        let table: toml::Table = toml::from_str("a = \"x\"\nb = \"y\"\n").unwrap();
        let store = CredentialsStore { table };
        let mut keys: Vec<&str> = store.keys().collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["a", "b"]);
    }

    // ── load_credentials ────────────────────────────────────────────────────

    #[test]
    fn load_credentials_returns_empty_when_no_enc_file() {
        let tmp = TempDir::new().unwrap();
        let store = load_credentials_with_env("development", tmp.path(), &empty_env()).unwrap();
        assert!(store.is_empty(), "no file → empty store");
    }

    #[test]
    fn load_credentials_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let key = MasterKey::generate();
        let plaintext = "stripe_key = \"sk_live_test\"\n";
        let ct = encrypt(&key, plaintext.as_bytes());
        std::fs::create_dir_all(tmp.path().join("config/credentials")).unwrap();
        std::fs::write(
            tmp.path().join("config/credentials/development.toml.enc"),
            &ct,
        )
        .unwrap();
        let env = mock_env("AUTUMN_MASTER_KEY", &key.to_hex());
        let store = load_credentials_with_env("development", tmp.path(), &env).unwrap();
        let val: Option<String> = store.get("stripe_key");
        assert_eq!(val.as_deref(), Some("sk_live_test"));
    }

    #[test]
    fn load_credentials_wrong_key_returns_decryption_error() {
        let tmp = TempDir::new().unwrap();
        let key1 = MasterKey::generate();
        let key2 = MasterKey::generate();
        let ct = encrypt(&key1, b"x = \"y\"\n");
        std::fs::create_dir_all(tmp.path().join("config/credentials")).unwrap();
        std::fs::write(
            tmp.path().join("config/credentials/development.toml.enc"),
            &ct,
        )
        .unwrap();
        let env = mock_env("AUTUMN_MASTER_KEY", &key2.to_hex());
        let err = load_credentials_with_env("development", tmp.path(), &env).unwrap_err();
        assert!(matches!(err, CredentialsError::DecryptionFailed { .. }));
    }

    #[test]
    fn load_credentials_no_key_returns_no_key_found() {
        let tmp = TempDir::new().unwrap();
        let key = MasterKey::generate();
        let ct = encrypt(&key, b"x = \"y\"\n");
        std::fs::create_dir_all(tmp.path().join("config/credentials")).unwrap();
        std::fs::write(
            tmp.path().join("config/credentials/development.toml.enc"),
            &ct,
        )
        .unwrap();
        let err = load_credentials_with_env("development", tmp.path(), &empty_env()).unwrap_err();
        assert!(matches!(err, CredentialsError::NoKeyFound));
    }

    // ── MasterKey::generate + to_hex ─────────────────────────────────────────

    #[test]
    fn generated_key_hex_is_64_chars() {
        let key = MasterKey::generate();
        assert_eq!(key.to_hex().len(), KEY_HEX_LEN);
    }

    #[test]
    fn master_key_generate_is_random() {
        let k1 = MasterKey::generate();
        let k2 = MasterKey::generate();
        assert_ne!(k1.bytes, k2.bytes, "two generated keys must differ");
    }

    #[test]
    fn master_key_debug_does_not_leak_bytes() {
        let key = MasterKey::generate();
        let dbg = format!("{key:?}");
        assert!(dbg.contains("REDACTED"), "Debug impl must redact key bytes");
        assert!(!dbg.contains(&key.to_hex()));
    }

    #[test]
    fn credentials_path_returns_expected_path() {
        let tmp = TempDir::new().unwrap();
        let p = credentials_path("production", tmp.path());
        assert_eq!(p, tmp.path().join("config/credentials/production.toml.enc"));
    }

    #[test]
    fn credentials_store_len_returns_count() {
        let table: toml::Table = toml::from_str("a = \"x\"\nb = \"y\"\n").unwrap();
        let store = CredentialsStore { table };
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn credentials_store_is_not_empty_when_populated() {
        let table: toml::Table = toml::from_str("x = \"v\"\n").unwrap();
        let store = CredentialsStore { table };
        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn load_credentials_returns_toml_error_for_invalid_toml() {
        let tmp = TempDir::new().unwrap();
        let key = MasterKey::generate();
        let ct = encrypt(&key, b"this is not valid toml @@@\x00");
        std::fs::create_dir_all(tmp.path().join("config/credentials")).unwrap();
        std::fs::write(
            tmp.path().join("config/credentials/development.toml.enc"),
            &ct,
        )
        .unwrap();
        let env = mock_env("AUTUMN_MASTER_KEY", &key.to_hex());
        let err = load_credentials_with_env("development", tmp.path(), &env).unwrap_err();
        assert!(
            matches!(err, CredentialsError::Toml(_)),
            "invalid TOML should return Toml error, got {err}"
        );
    }

    #[test]
    fn load_credentials_returns_file_truncated_for_non_utf8_plaintext() {
        let tmp = TempDir::new().unwrap();
        let key = MasterKey::generate();
        // Encrypt bytes that are valid AES-GCM output but not valid UTF-8 TOML
        let non_utf8 = vec![0xC0u8, 0x80u8, 0xFF, 0xFE]; // invalid UTF-8 sequence
        let ct = encrypt(&key, &non_utf8);
        std::fs::create_dir_all(tmp.path().join("config/credentials")).unwrap();
        std::fs::write(
            tmp.path().join("config/credentials/development.toml.enc"),
            &ct,
        )
        .unwrap();
        let env = mock_env("AUTUMN_MASTER_KEY", &key.to_hex());
        let err = load_credentials_with_env("development", tmp.path(), &env).unwrap_err();
        assert!(
            matches!(err, CredentialsError::FileTruncated),
            "non-UTF-8 plaintext should return FileTruncated, got {err}"
        );
    }

    #[test]
    fn invalid_hex_in_key_file_returns_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/master.key"), "not-valid-hex-at-all").unwrap();
        let err = resolve_master_key_with_env(tmp.path(), &empty_env()).unwrap_err();
        assert!(
            matches!(err, CredentialsError::InvalidKeyFormat { .. }),
            "bad hex in key file should return InvalidKeyFormat, got {err}"
        );
    }

    #[test]
    fn load_credentials_with_key_override_some_uses_override() {
        let tmp = TempDir::new().unwrap();
        let key = MasterKey::generate();
        let ct = encrypt(&key, b"api_key = \"secret\"\n");
        std::fs::create_dir_all(tmp.path().join("config/credentials")).unwrap();
        std::fs::write(
            tmp.path().join("config/credentials/production.toml.enc"),
            &ct,
        )
        .unwrap();
        let store =
            load_credentials_with_key_override("production", tmp.path(), Some(&key.to_hex()))
                .unwrap();
        let val: Option<String> = store.get("api_key");
        assert_eq!(val.as_deref(), Some("secret"));
    }

    #[test]
    fn load_credentials_with_key_override_none_falls_back_to_env() {
        let tmp = TempDir::new().unwrap();
        let key = MasterKey::generate();
        let ct = encrypt(&key, b"x = \"y\"\n");
        std::fs::create_dir_all(tmp.path().join("config/credentials")).unwrap();
        std::fs::write(tmp.path().join("config/credentials/dev.toml.enc"), &ct).unwrap();
        // No override and no env var → NoKeyFound
        let err = load_credentials_with_key_override("dev", tmp.path(), None).unwrap_err();
        assert!(matches!(err, CredentialsError::NoKeyFound));
    }
}
