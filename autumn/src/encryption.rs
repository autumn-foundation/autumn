//! At-rest attribute encryption for `#[repository]`/`#[model]` columns.
//!
//! This module provides declarative, transparent encryption of sensitive data
//! columns (OAuth tokens, government IDs, PHI, ...). A field marked `#[encrypted]`
//! on a `#[model]` struct persists as opaque ciphertext on disk while remaining a
//! plain `String` in Rust code — `repo.find(id)` and `repo.update(..)` see
//! plaintext, the database column holds an envelope.
//!
//! # On-disk envelope format
//!
//! Every encrypted value is stored as a base64 ([`base64::engine::general_purpose::STANDARD`])
//! string wrapping the following binary envelope:
//!
//! ```text
//! byte  0      magic   = 0xA7        (Autumn attribute encryption)
//! byte  1      version = 0x01
//! byte  2      alg     = 0x01        (AES-256-GCM)
//! byte  3      mode    = 0x00 randomized | 0x01 deterministic
//! bytes 4..8   key_id  : u32 big-endian (which data key encrypted this value)
//! bytes 8..20  nonce   : 12 bytes
//! bytes 20..   ciphertext + 16-byte AES-GCM authentication tag
//! ```
//!
//! The envelope is self-describing: an external decryption tool, given the master
//! key material and the documented key-derivation below, can decode the header,
//! select the data key by `key_id`, and decrypt. See `docs/guide/attribute-encryption.md`.
//!
//! # Key derivation (documented for external tooling)
//!
//! Master key material is sourced from the encrypted credentials store (see
//! [`crate::credentials`]) under the [`CREDENTIALS_NAMESPACE`] namespace
//! (`active_record_encryption.primary_key`, `.deterministic_key`,
//! `.key_derivation_salt`, `.retired_keys`). Each 64-hex master key is turned into
//! a 32-byte AES-256 data key:
//!
//! ```text
//! data_key   = HMAC-SHA256(master_bytes, b"autumn:data:v1:" || salt)   // randomized
//! det_key    = HMAC-SHA256(master_bytes, b"autumn:det:v1:"  || salt)   // deterministic
//! // key_id identifies the key that encrypted the value, so it is derived from
//! // the mode-specific key: data_key for randomized envelopes (mode 0x00),
//! // det_key for deterministic ones (mode 0x01).
//! key_id     = u32::from_be_bytes(SHA256(b"autumn:id:v1:" || derived_key)[0..4])
//! ```
//!
//! `key_id` is a stable function of the derived key, so a key keeps the same id
//! across reboots and config reorderings — which is what makes rotation safe:
//! retiring a key leaves previously written rows decryptable by `key_id` lookup.
//!
//! # Modes
//!
//! * **Randomized** (default): a fresh random nonce per write. Equal plaintexts
//!   produce different ciphertexts. Equality lookups (`WHERE col = ?`) are *not*
//!   supported. This is the safe default.
//! * **Deterministic** (explicit opt-in): the nonce is derived
//!   `HMAC-SHA256(det_key, plaintext)[0..12]`, so equal plaintexts produce equal
//!   ciphertext and `WHERE col = deterministic_ciphertext(value)` works. The
//!   tradeoff — equality of plaintext leaks through equality of ciphertext — is
//!   why deterministic mode must be requested explicitly (`#[encrypted(deterministic)]`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAGIC: u8 = 0xA7;
const VERSION: u8 = 0x01;
const ALG_AES_256_GCM: u8 = 0x01;
const MODE_RANDOMIZED: u8 = 0x00;
const MODE_DETERMINISTIC: u8 = 0x01;
const NONCE_LEN: usize = 12;
const HEADER_LEN: usize = 1 + 1 + 1 + 1 + 4 + NONCE_LEN; // magic+ver+alg+mode+key_id+nonce = 20

/// Credentials-store namespace holding attribute-encryption key material.
///
/// Mirrors Rails' Active Record Encryption layout so the docs and mental model
/// transfer directly.
pub const CREDENTIALS_NAMESPACE: &str = "active_record_encryption";

type HmacSha256 = Hmac<Sha256>;

/// Errors produced by attribute encryption.
#[derive(Debug, Error)]
pub enum EncryptionError {
    /// No global key ring has been installed (boot did not resolve keys).
    #[error(
        "attribute encryption key ring is not installed; configure `{ns}.primary_key` via `autumn credentials edit`",
        ns = CREDENTIALS_NAMESPACE
    )]
    NoKeyRing,

    /// A master key string was not valid 64-character hex.
    #[error("invalid {what} in `{ns}`: expected 64 hex characters, got {len}", ns = CREDENTIALS_NAMESPACE)]
    InvalidKeyFormat {
        /// Which credential (e.g. `primary_key`).
        what: String,
        /// Actual length encountered.
        len: usize,
    },

    /// Deterministic mode was requested but no `deterministic_key` is configured.
    #[error(
        "deterministic encryption requires `{ns}.deterministic_key`; add it via `autumn credentials edit`",
        ns = CREDENTIALS_NAMESPACE
    )]
    NoDeterministicKey,

    /// No `key_derivation_salt` was configured.
    ///
    /// The salt is not secret, but it must be set explicitly (rather than a
    /// shared built-in default) so key derivation is unique per deployment.
    #[error(
        "attribute encryption requires `{ns}.key_derivation_salt`; add it via `autumn credentials edit` (e.g. `openssl rand -hex 16`)",
        ns = CREDENTIALS_NAMESPACE
    )]
    MissingSalt,

    /// The stored envelope is malformed (bad magic, truncated, bad base64).
    #[error("malformed encryption envelope: {0}")]
    MalformedEnvelope(&'static str),

    /// The envelope used an algorithm/version this build does not support.
    #[error("unsupported encryption envelope (version={version:#04x}, alg={alg:#04x})")]
    UnsupportedEnvelope {
        /// Envelope version byte.
        version: u8,
        /// Algorithm id byte.
        alg: u8,
    },

    /// No installed data key matches the envelope's `key_id` (key fully retired/removed).
    #[error("no data key with id {0:#010x} is available; was it removed from the rotation list?")]
    UnknownKeyId(u32),

    /// AEAD authentication failed (wrong key or corrupted ciphertext).
    #[error("decryption failed: wrong key or corrupted ciphertext")]
    DecryptionFailed,

    /// An equality lookup (e.g. a derived `find_by_<col>`) targeted a
    /// randomized-encrypted column. Randomized encryption uses a fresh nonce per
    /// write, so equal plaintexts never produce equal ciphertext and the lookup
    /// can never match — use `#[encrypted(deterministic)]` if you need lookups.
    #[error(
        "equality lookup on randomized-encrypted column `{table}.{column}` is impossible; \
         mark it `#[encrypted(deterministic)]` to support `find_by`/`exists_by` lookups"
    )]
    RandomizedEqualityLookup {
        /// Table the column belongs to.
        table: String,
        /// The randomized-encrypted column name.
        column: String,
    },
}

/// Encryption mode for a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Fresh nonce per write; no equality lookups. The default.
    Randomized,
    /// Stable ciphertext for equal plaintext; supports equality lookups.
    Deterministic,
}

/// A single 32-byte AES-256 data key with its stable id.
#[derive(Clone)]
pub struct DataKey {
    id: u32,
    key: [u8; 32],
}

impl std::fmt::Debug for DataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataKey")
            .field("id", &format_args!("{:#010x}", self.id))
            .field("key", &"[REDACTED]")
            .finish()
    }
}

impl DataKey {
    /// Stable id of this data key (the value stored in the envelope header).
    #[must_use]
    pub const fn id(&self) -> u32 {
        self.id
    }
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

fn decode_master_hex(hex_str: &str, what: &str) -> Result<[u8; 32], EncryptionError> {
    let hex_str = hex_str.trim();
    if hex_str.len() != 64 {
        return Err(EncryptionError::InvalidKeyFormat {
            what: what.to_owned(),
            len: hex_str.len(),
        });
    }
    let decoded = hex::decode(hex_str).map_err(|_| EncryptionError::InvalidKeyFormat {
        what: what.to_owned(),
        len: hex_str.len(),
    })?;
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&decoded);
    Ok(bytes)
}

fn derive_data_key(master: &[u8; 32], salt: &[u8], domain: &[u8]) -> DataKey {
    let mut msg = Vec::with_capacity(domain.len() + salt.len());
    msg.extend_from_slice(domain);
    msg.extend_from_slice(salt);
    let key = hmac_sha256(master, &msg);
    let id_digest = {
        let mut h = Sha256::new();
        h.update(b"autumn:id:v1:");
        h.update(key);
        h.finalize()
    };
    let id = u32::from_be_bytes([id_digest[0], id_digest[1], id_digest[2], id_digest[3]]);
    DataKey { id, key }
}

/// A resolved set of data keys: the current key, any retired keys (for reads
/// during rotation), and an optional deterministic key.
#[derive(Debug, Clone)]
pub struct KeyRing {
    primary: DataKey,
    retired: Vec<DataKey>,
    deterministic: Option<DataKey>,
    /// Deterministic keys derived from the retired key list, so rows written
    /// before a deterministic-key rotation remain readable by `key_id`.
    retired_deterministic: Vec<DataKey>,
}

impl KeyRing {
    /// Build a key ring from hex master key material.
    ///
    /// * `primary_hex` — the current key; all new randomized writes use it.
    /// * `retired_hex` — previously-current keys, kept only so existing rows
    ///   remain readable. Rotating retires the old primary here without
    ///   rewriting any rows.
    /// * `deterministic_hex` — optional; required only if any column uses
    ///   deterministic mode (or `versioned_ciphertext`).
    /// * `salt` — the `key_derivation_salt`; mixed into key derivation.
    ///
    /// Each entry in `retired_hex` is derived in **both** the randomized and the
    /// deterministic key domains, so a key rotated out of either role (former
    /// `primary_key` or former `deterministic_key`) keeps its rows readable.
    ///
    /// # Errors
    ///
    /// Returns [`EncryptionError::InvalidKeyFormat`] if any key is not 64-hex.
    pub fn from_master_hex(
        primary_hex: &str,
        retired_hex: &[String],
        deterministic_hex: Option<&str>,
        salt: &[u8],
    ) -> Result<Self, EncryptionError> {
        let primary = derive_data_key(
            &decode_master_hex(primary_hex, "primary_key")?,
            salt,
            b"autumn:data:v1:",
        );
        let mut retired = Vec::with_capacity(retired_hex.len());
        let mut retired_deterministic = Vec::with_capacity(retired_hex.len());
        for (i, k) in retired_hex.iter().enumerate() {
            let bytes = decode_master_hex(k, &format!("retired_keys[{i}]"))?;
            retired.push(derive_data_key(&bytes, salt, b"autumn:data:v1:"));
            retired_deterministic.push(derive_data_key(&bytes, salt, b"autumn:det:v1:"));
        }
        let deterministic = match deterministic_hex {
            Some(k) => Some(derive_data_key(
                &decode_master_hex(k, "deterministic_key")?,
                salt,
                b"autumn:det:v1:",
            )),
            None => None,
        };
        Ok(Self {
            primary,
            retired,
            deterministic,
            retired_deterministic,
        })
    }

    /// The current (primary) data key id used for new randomized writes.
    #[must_use]
    pub const fn primary_key_id(&self) -> u32 {
        self.primary.id
    }

    fn find_key(&self, mode: u8, key_id: u32) -> Option<&DataKey> {
        if mode == MODE_DETERMINISTIC {
            return self
                .deterministic
                .as_ref()
                .filter(|k| k.id == key_id)
                .or_else(|| self.retired_deterministic.iter().find(|k| k.id == key_id));
        }
        if self.primary.id == key_id {
            return Some(&self.primary);
        }
        self.retired.iter().find(|k| k.id == key_id)
    }

    /// Encrypt `plaintext` into a base64 envelope string for on-disk storage.
    ///
    /// # Errors
    ///
    /// Returns [`EncryptionError::NoDeterministicKey`] if deterministic mode is
    /// requested without a configured deterministic key.
    ///
    /// # Panics
    ///
    /// Panics if the operating system's random number generator is unavailable
    /// (randomized mode only).
    pub fn encrypt(&self, mode: Mode, plaintext: &[u8]) -> Result<String, EncryptionError> {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Nonce};
        use base64::Engine as _;

        let (mode_byte, data_key, nonce_bytes) = match mode {
            Mode::Randomized => {
                let mut nonce = [0u8; NONCE_LEN];
                getrandom::getrandom(&mut nonce).expect("OS RNG failed");
                (MODE_RANDOMIZED, &self.primary, nonce)
            }
            Mode::Deterministic => {
                let det = self
                    .deterministic
                    .as_ref()
                    .ok_or(EncryptionError::NoDeterministicKey)?;
                // Synthetic IV: nonce is a keyed function of the plaintext, so
                // equal plaintexts encrypt identically under a fixed key.
                let tag = hmac_sha256(&det.key, plaintext);
                let mut nonce = [0u8; NONCE_LEN];
                nonce.copy_from_slice(&tag[..NONCE_LEN]);
                (MODE_DETERMINISTIC, det, nonce)
            }
        };

        let cipher = Aes256Gcm::new_from_slice(&data_key.key).expect("32-byte key");
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
            .expect("AES-GCM encryption cannot fail for valid inputs");

        let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        out.push(MAGIC);
        out.push(VERSION);
        out.push(ALG_AES_256_GCM);
        out.push(mode_byte);
        out.extend_from_slice(&data_key.id.to_be_bytes());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);

        Ok(base64::engine::general_purpose::STANDARD.encode(out))
    }

    /// Decrypt a base64 envelope produced by [`KeyRing::encrypt`].
    ///
    /// Transparently selects the right data key by the envelope's `key_id`, so
    /// rows written before a rotation decrypt with the retired key.
    ///
    /// # Errors
    ///
    /// See [`EncryptionError`] variants for malformed input, unknown key id, and
    /// authentication failure.
    ///
    /// # Panics
    ///
    /// Does not panic for any envelope input; the internal cipher construction
    /// is infallible because data keys are always 32 bytes.
    pub fn decrypt(&self, envelope: &str) -> Result<Vec<u8>, EncryptionError> {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Nonce};
        use base64::Engine as _;

        let raw = base64::engine::general_purpose::STANDARD
            .decode(envelope.trim())
            .map_err(|_| EncryptionError::MalformedEnvelope("not valid base64"))?;
        if raw.len() < HEADER_LEN {
            return Err(EncryptionError::MalformedEnvelope("truncated header"));
        }
        if raw[0] != MAGIC {
            return Err(EncryptionError::MalformedEnvelope("bad magic byte"));
        }
        let version = raw[1];
        let alg = raw[2];
        if version != VERSION || alg != ALG_AES_256_GCM {
            return Err(EncryptionError::UnsupportedEnvelope { version, alg });
        }
        let mode = raw[3];
        let key_id = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
        let nonce_bytes = &raw[8..8 + NONCE_LEN];
        let ciphertext = &raw[HEADER_LEN..];

        let data_key = self
            .find_key(mode, key_id)
            .ok_or(EncryptionError::UnknownKeyId(key_id))?;
        let cipher = Aes256Gcm::new_from_slice(&data_key.key).expect("32-byte key");
        cipher
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|_| EncryptionError::DecryptionFailed)
    }
}

// ---------------------------------------------------------------------------
// Global key ring + process-wide accessors
// ---------------------------------------------------------------------------

static KEY_RING: OnceLock<KeyRing> = OnceLock::new();
static DEBUG_PLAINTEXT: AtomicBool = AtomicBool::new(false);

/// Install the process-global key ring. Idempotent: the first install wins.
///
/// Returns `true` if this call installed the ring, `false` if one was already
/// present.
pub fn install_key_ring(ring: KeyRing) -> bool {
    KEY_RING.set(ring).is_ok()
}

/// Borrow the installed global key ring, if any.
#[must_use]
pub fn key_ring() -> Option<&'static KeyRing> {
    KEY_RING.get()
}

/// Encrypt a string column value using the global key ring.
///
/// # Errors
///
/// Returns [`EncryptionError::NoKeyRing`] if encryption is used without a
/// configured key ring (a misconfiguration that boot validation prevents).
pub fn encrypt_text(mode: Mode, plaintext: &str) -> Result<String, EncryptionError> {
    key_ring()
        .ok_or(EncryptionError::NoKeyRing)?
        .encrypt(mode, plaintext.as_bytes())
}

/// Decrypt a string column value using the global key ring.
///
/// # Errors
///
/// Returns [`EncryptionError::NoKeyRing`] without a configured key ring, or a
/// decryption error for a malformed/unauthentic envelope.
pub fn decrypt_text(envelope: &str) -> Result<String, EncryptionError> {
    let bytes = key_ring()
        .ok_or(EncryptionError::NoKeyRing)?
        .decrypt(envelope)?;
    String::from_utf8(bytes).map_err(|_| EncryptionError::DecryptionFailed)
}

/// Return the deterministic ciphertext envelope for `plaintext`, for use in
/// equality lookups against a deterministic-encrypted column:
///
/// ```ignore
/// users::table
///     .filter(users::email.eq(deterministic_ciphertext("a@b.com")?))
///     .first::<User>(&mut conn)
/// ```
///
/// # Errors
///
/// As [`encrypt_text`], plus [`EncryptionError::NoDeterministicKey`].
pub fn deterministic_ciphertext(plaintext: &str) -> Result<String, EncryptionError> {
    encrypt_text(Mode::Deterministic, plaintext)
}

/// Enable rendering of decrypted plaintext in `Debug` output. **Development
/// only** — never enable in production. Off by default.
pub fn set_debug_plaintext(enabled: bool) {
    DEBUG_PLAINTEXT.store(enabled, Ordering::Relaxed);
}

/// Whether the [`set_debug_plaintext`] development escape hatch is active.
#[must_use]
pub fn debug_plaintext_enabled() -> bool {
    DEBUG_PLAINTEXT.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Credentials-store integration + boot validation
// ---------------------------------------------------------------------------

/// Typed view of the `active_record_encryption` credentials namespace.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct EncryptionCredentials {
    /// Current key used for all new randomized writes (64-hex).
    pub primary_key: String,
    /// Optional deterministic key (64-hex); required for deterministic columns.
    #[serde(default)]
    pub deterministic_key: Option<String>,
    /// Salt mixed into key derivation. Required (no shared built-in default) so
    /// derivation is unique per deployment; not secret.
    #[serde(default)]
    pub key_derivation_salt: Option<String>,
    /// Retired keys kept for reads during rotation (64-hex each).
    #[serde(default)]
    pub retired_keys: Vec<String>,
}

impl EncryptionCredentials {
    /// Derive a [`KeyRing`] from this credential material.
    ///
    /// # Errors
    ///
    /// Propagates [`EncryptionError::InvalidKeyFormat`] for malformed keys, and
    /// returns [`EncryptionError::MissingSalt`] if `key_derivation_salt` is not
    /// configured (no shared built-in default — the salt must be deployment-unique).
    pub fn to_key_ring(&self) -> Result<KeyRing, EncryptionError> {
        let salt = self
            .key_derivation_salt
            .as_deref()
            .ok_or(EncryptionError::MissingSalt)?;
        KeyRing::from_master_hex(
            &self.primary_key,
            &self.retired_keys,
            self.deterministic_key.as_deref(),
            salt.as_bytes(),
        )
    }
}

/// Read the encryption key ring from a credentials store, if configured.
///
/// Returns `Ok(None)` when the namespace is absent (apps not using encryption).
///
/// # Errors
///
/// Returns an error if the namespace is present but malformed.
pub fn key_ring_from_credentials(
    store: &crate::credentials::CredentialsStore,
) -> Result<Option<KeyRing>, EncryptionError> {
    match store.get::<EncryptionCredentials>(CREDENTIALS_NAMESPACE) {
        Some(creds) => Ok(Some(creds.to_key_ring()?)),
        None => Ok(None),
    }
}

/// Compile-time registration of an encrypted column, emitted by the `#[model]`
/// macro. Drives boot validation, log-scrub auto-registration, and admin redaction.
#[derive(Debug)]
pub struct EncryptedColumnDescriptor {
    /// Model type name (e.g. `User`).
    pub model: &'static str,
    /// Database table name.
    pub table: &'static str,
    /// Column name.
    pub column: &'static str,
    /// Whether the column uses deterministic mode.
    pub deterministic: bool,
    /// `admin_visible` opt-in: render decrypted plaintext in admin views (the
    /// admin surface itself is authorization-gated; #496). Default: redacted.
    pub admin_visible: bool,
    /// `versioned_ciphertext` opt-in: store encrypted before/after ciphertext in
    /// record version history instead of the default "changed (encrypted)" marker.
    pub versioned_ciphertext: bool,
}

inventory::collect!(EncryptedColumnDescriptor);

/// All encrypted columns registered across the binary.
#[must_use]
pub fn registered_encrypted_columns() -> Vec<&'static EncryptedColumnDescriptor> {
    inventory::iter::<EncryptedColumnDescriptor>
        .into_iter()
        .collect()
}

/// Distinct column names of every registered encrypted column. Fed into the log
/// parameter scrubber so encrypted values never reach traces (composes with #697).
#[must_use]
pub fn registered_encrypted_column_names() -> Vec<String> {
    let mut names: Vec<String> = registered_encrypted_columns()
        .iter()
        .map(|d| d.column.to_owned())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Whether `column` of `table` is a registered encrypted column.
#[must_use]
pub fn is_encrypted_column(table: &str, column: &str) -> bool {
    registered_encrypted_columns()
        .iter()
        .any(|d| d.table == table && d.column == column)
}

/// Encode a derived-query equality parameter for a column that may be encrypted.
///
/// Generated repository derived queries (`find_by_email`, `exists_by_*`,
/// `count_by_*`, `delete_by_*`) compare a user-supplied value against the stored
/// column. When the column is encrypted at rest the stored value is ciphertext,
/// so the plaintext parameter has to be transformed to match:
///
/// * **deterministic** column → its stable deterministic ciphertext, so the
///   equality predicate matches the rows stored under the current key;
/// * **randomized** column → [`EncryptionError::RandomizedEqualityLookup`]:
///   equality lookups are impossible by design (a fresh nonce per write means
///   ciphertext never repeats);
/// * **non-encrypted** column → the plaintext unchanged (a pure passthrough).
///
/// This is a runtime registry lookup because the repository macro cannot see a
/// column's encryption mode at compile time (it is declared on the model).
///
/// # Errors
/// Returns [`EncryptionError::RandomizedEqualityLookup`] for a randomized column,
/// or any error from deterministic encryption (e.g. a missing deterministic key).
pub fn encode_derived_query_param(
    table: &str,
    column: &str,
    plaintext: impl AsRef<str>,
) -> Result<String, EncryptionError> {
    let plaintext = plaintext.as_ref();
    for d in registered_encrypted_columns() {
        if d.table == table && d.column == column {
            if d.deterministic {
                return encrypt_text(Mode::Deterministic, plaintext);
            }
            return Err(EncryptionError::RandomizedEqualityLookup {
                table: table.to_owned(),
                column: column.to_owned(),
            });
        }
    }
    Ok(plaintext.to_owned())
}

/// Whether any registered encrypted column has this name (table-agnostic).
///
/// Used by surfaces that lack table context — e.g. the admin plugin's cell
/// renderer — to redact encrypted columns by default. Errs toward privacy: a
/// same-named column on another table is also redacted.
#[must_use]
pub fn is_encrypted_column_name(column: &str) -> bool {
    registered_encrypted_columns()
        .iter()
        .any(|d| d.column == column)
}

/// Whether an encrypted column named `column` should be redacted in admin views.
///
/// Returns `false` only when the column is encrypted and *every* registration of
/// that name opted into `admin_visible` (conservative: any non-visible
/// registration of the same name forces redaction).
#[must_use]
pub fn admin_redacts_column_name(column: &str) -> bool {
    for d in registered_encrypted_columns() {
        if d.column == column && !d.admin_visible {
            return true;
        }
    }
    // Either not an encrypted column, or every registration opted into
    // admin_visible — in both cases this helper does not force redaction.
    false
}

/// Encrypted column names for a single table.
#[must_use]
pub fn encrypted_columns_for_table(table: &str) -> Vec<&'static str> {
    registered_encrypted_columns()
        .iter()
        .filter(|d| d.table == table)
        .map(|d| d.column)
        .collect()
}

/// Append this table's encrypted columns to `columns`, de-duplicating.
///
/// Used by generated `VersionedRecord::version_sensitive_columns` so encrypted
/// columns are treated as sensitive in record version history (#700): the diff
/// stores a "changed (encrypted)" marker and never the plaintext that the
/// in-memory model would otherwise serialize.
///
/// Columns that opted into `versioned_ciphertext` are **excluded** here — their
/// before/after values are stored as ciphertext instead (see
/// [`encrypt_versioned_columns_in_value`]).
pub fn merge_encrypted_columns_for_table(table: &str, columns: &mut Vec<&'static str>) {
    for d in registered_encrypted_columns() {
        if d.table == table && !d.versioned_ciphertext && !columns.contains(&d.column) {
            columns.push(d.column);
        }
    }
}

/// Append this table's **randomized**-mode encrypted columns to `columns`,
/// de-duplicating.
///
/// A column encrypted in randomized mode produces different ciphertext for the
/// same plaintext on every write, so two independent encodings of the same row
/// never compare equal. Deterministic columns are deliberately excluded: their
/// ciphertext is stable, so they compare fine and stay covered.
///
/// Used by the ledger's live-row cross-check (#1699), which compares a stored
/// revision's snapshot against a freshly encoded live row and must not read a
/// fresh nonce as a divergence.
pub fn randomized_encrypted_columns_for_table(table: &str, columns: &mut Vec<&'static str>) {
    for d in registered_encrypted_columns() {
        if d.table == table && !d.deterministic && !columns.contains(&d.column) {
            columns.push(d.column);
        }
    }
}

/// Rewrite a model's JSON column-values snapshot (as produced for record version
/// history) so that columns opted into `versioned_ciphertext` carry ciphertext
/// rather than plaintext.
///
/// The values are encrypted **deterministically** so that an unchanged plaintext
/// produces identical ciphertext across snapshots — keeping the version diff
/// accurate. If a deterministic key is not configured (or encryption otherwise
/// fails), the value is replaced with a `"<encrypted>"` marker so plaintext can
/// never leak into the version-history table.
pub fn encrypt_versioned_columns_in_value(table: &str, value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for d in registered_encrypted_columns() {
        if d.table != table || !d.versioned_ciphertext {
            continue;
        }
        if let Some(field) = obj.get_mut(d.column) {
            // Non-null, non-string values keep their structure (never plaintext).
            if field.is_null() {
                continue;
            }
            let marker = || serde_json::Value::String("<encrypted>".to_owned());
            let replacement = field.as_str().map_or_else(marker, |plaintext| {
                encrypt_text(Mode::Deterministic, plaintext)
                    .map_or_else(|_| marker(), serde_json::Value::String)
            });
            *field = replacement;
        }
    }
}

/// Rewrite a model's JSON snapshot so encrypted columns carry ciphertext.
///
/// Every encrypted column is encrypted in its declared mode (deterministic
/// columns deterministically, otherwise randomized).
///
/// Used for durable commit-hook payloads (`autumn_repository_commit_hooks`),
/// which would otherwise persist decrypted secrets in a separate table. The
/// values stay recoverable via [`decrypt_text`]. If encryption fails (e.g. a
/// deterministic column without a deterministic key), the value is replaced with
/// a `"<encrypted>"` marker so plaintext can never leak.
pub fn encrypt_persisted_columns_in_value(table: &str, value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for d in registered_encrypted_columns() {
        if d.table != table {
            continue;
        }
        if let Some(field) = obj.get_mut(d.column) {
            if field.is_null() {
                continue;
            }
            let mode = if d.deterministic {
                Mode::Deterministic
            } else {
                Mode::Randomized
            };
            let marker = || serde_json::Value::String("<encrypted>".to_owned());
            let replacement = field.as_str().map_or_else(marker, |plaintext| {
                encrypt_text(mode, plaintext).map_or_else(|_| marker(), serde_json::Value::String)
            });
            *field = replacement;
        }
    }
}

/// Inverse of [`encrypt_persisted_columns_in_value`].
///
/// Decrypts a model's JSON snapshot read back from the durable
/// `autumn_repository_commit_hooks` table so that replayed `after_*_commit` hooks
/// receive plaintext model values — exactly as on the normal repository path.
///
/// Only string fields that successfully decrypt are rewritten; a value that is
/// not a recoverable envelope (e.g. an already-plaintext legacy record, or the
/// `"<encrypted>"` fallback marker) is left untouched so reconstruction still
/// succeeds. Non-string and null fields are ignored.
pub fn decrypt_persisted_columns_in_value(table: &str, value: &mut serde_json::Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    for d in registered_encrypted_columns() {
        if d.table != table {
            continue;
        }
        if let Some(field) = obj.get_mut(d.column) {
            let Some(envelope) = field.as_str() else {
                continue;
            };
            if let Ok(plaintext) = decrypt_text(envelope) {
                *field = serde_json::Value::String(plaintext);
            }
        }
    }
}

/// Boot validation for attribute encryption.
///
/// If any encrypted columns are registered, the key material must resolve.
/// Mirrors the fast-fail diagnostic shape of the credentials and signing-secret
/// checks (#597), naming the missing credential path. On success, installs the
/// global key ring.
///
/// # Errors
///
/// Returns a human-readable diagnostic (already formatted with a hint) when
/// encryption is required but keys are missing or malformed. Callers print it
/// and exit non-zero.
pub fn init_attribute_encryption(
    store: &crate::credentials::CredentialsStore,
) -> Result<(), String> {
    let columns = registered_encrypted_columns();
    // No `#[encrypted]` columns anywhere means encryption is not in use: this is a
    // pure no-op. Return before touching the credentials store, so an app that
    // never opted into attribute encryption can never fail boot over a partial or
    // malformed `active_record_encryption` namespace it does not rely on.
    if columns.is_empty() {
        return Ok(());
    }
    // The deterministic key is needed both for deterministic-mode columns and for
    // `versioned_ciphertext` columns (whose history snapshots are encrypted
    // deterministically so the version diff stays accurate).
    let needs_deterministic = columns
        .iter()
        .any(|c| c.deterministic || c.versioned_ciphertext);

    match key_ring_from_credentials(store) {
        Ok(Some(ring)) => {
            if needs_deterministic && ring.deterministic.is_none() {
                return Err(format!(
                    "An encrypted column requires deterministic encryption (deterministic mode \
                     or `versioned_ciphertext`) but `{CREDENTIALS_NAMESPACE}.deterministic_key` is missing.\n  \
                     hint: add it with `autumn credentials edit` (generate one with `openssl rand -hex 32`)."
                ));
            }
            install_key_ring(ring);
            Ok(())
        }
        Ok(None) => {
            // `columns` is non-empty here (the empty case returned early above).
            let first = columns[0];
            Err(format!(
                "Encrypted column `{}.{}` requires a master key, but `{CREDENTIALS_NAMESPACE}.primary_key` \
                 is not configured.\n  hint: run `autumn credentials edit` and add:\n    \
                 [{CREDENTIALS_NAMESPACE}]\n    primary_key = \"<64 hex chars from `openssl rand -hex 32`>\"",
                first.table, first.column
            ))
        }
        Err(e) => Err(format!(
            "Invalid attribute-encryption key material in `{CREDENTIALS_NAMESPACE}`: {e}\n  \
             hint: keys must be 64 hex characters; regenerate with `openssl rand -hex 32`."
        )),
    }
}

#[cfg(feature = "db")]
mod diesel_types;
#[cfg(feature = "db")]
pub use diesel_types::{DeterministicText, RandomizedText};

#[cfg(test)]
mod tests {
    use super::*;

    // A registered `versioned_ciphertext` + `admin_visible` column, used by the
    // composition tests below.
    inventory::submit! {
        EncryptedColumnDescriptor {
            model: "VcModel",
            table: "vc_table",
            column: "vc_col",
            deterministic: false,
            admin_visible: false,
            versioned_ciphertext: true,
        }
    }
    inventory::submit! {
        EncryptedColumnDescriptor {
            model: "VcModel",
            table: "vc_table",
            column: "visible_col",
            deterministic: false,
            admin_visible: true,
            versioned_ciphertext: false,
        }
    }
    // Columns for the derived-query param-encoding test: a deterministic column
    // (equality lookups supported) and a randomized one (lookups impossible).
    inventory::submit! {
        EncryptedColumnDescriptor {
            model: "DqModel",
            table: "dq_table",
            column: "det_col",
            deterministic: true,
            admin_visible: false,
            versioned_ciphertext: false,
        }
    }
    inventory::submit! {
        EncryptedColumnDescriptor {
            model: "DqModel",
            table: "dq_table",
            column: "rnd_col",
            deterministic: false,
            admin_visible: false,
            versioned_ciphertext: false,
        }
    }

    const KEY_A: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const KEY_B: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const DET: &str = "3333333333333333333333333333333333333333333333333333333333333333";

    // Salts are generated at runtime (not hard-coded) so the test fixtures don't
    // ship a constant cryptographic value; a process-stable salt is shared so
    // cross-ring determinism/rotation assertions hold.
    fn salt() -> &'static [u8] {
        static S: OnceLock<[u8; 16]> = OnceLock::new();
        S.get_or_init(|| {
            let mut b = [0u8; 16];
            getrandom::getrandom(&mut b).expect("OS RNG");
            b
        })
    }

    fn salt2() -> &'static [u8] {
        static S: OnceLock<[u8; 16]> = OnceLock::new();
        S.get_or_init(|| {
            let mut b = [1u8; 16];
            getrandom::getrandom(&mut b).expect("OS RNG");
            b
        })
    }

    fn ring() -> KeyRing {
        KeyRing::from_master_hex(KEY_A, &[], Some(DET), salt()).unwrap()
    }

    #[test]
    fn randomized_roundtrip() {
        let r = ring();
        let env = r.encrypt(Mode::Randomized, b"hello").unwrap();
        assert_eq!(r.decrypt(&env).unwrap(), b"hello");
    }

    #[test]
    fn deterministic_key_rotation_reads_old_rows() {
        // Row written under DET as the deterministic key.
        let old = KeyRing::from_master_hex(KEY_A, &[], Some(DET), salt()).unwrap();
        let written = old.encrypt(Mode::Deterministic, b"pii").unwrap();

        // Rotate the deterministic key to KEY_B; the former DET is retired.
        let rotated =
            KeyRing::from_master_hex(KEY_A, &[DET.to_string()], Some(KEY_B), salt()).unwrap();
        // Old deterministic row still decrypts via the retired deterministic key.
        assert_eq!(rotated.decrypt(&written).unwrap(), b"pii");
        // New deterministic writes use the new key.
        let fresh = rotated.encrypt(Mode::Deterministic, b"pii").unwrap();
        assert_ne!(fresh, written, "new det key produces different ciphertext");
        assert_eq!(rotated.decrypt(&fresh).unwrap(), b"pii");
    }

    #[test]
    fn randomized_is_nondeterministic() {
        let r = ring();
        let a = r.encrypt(Mode::Randomized, b"same").unwrap();
        let b = r.encrypt(Mode::Randomized, b"same").unwrap();
        assert_ne!(a, b, "fresh nonce per write must vary ciphertext");
        assert_eq!(r.decrypt(&a).unwrap(), r.decrypt(&b).unwrap());
    }

    #[test]
    fn deterministic_is_stable_and_equal_across_rings() {
        let r1 = ring();
        let r2 = ring();
        let a = r1.encrypt(Mode::Deterministic, b"a@b.com").unwrap();
        let b = r2.encrypt(Mode::Deterministic, b"a@b.com").unwrap();
        assert_eq!(
            a, b,
            "deterministic ciphertext must be stable for equality lookups"
        );
        let c = r1.encrypt(Mode::Deterministic, b"other@b.com").unwrap();
        assert_ne!(a, c);
        assert_eq!(r1.decrypt(&a).unwrap(), b"a@b.com");
    }

    #[test]
    fn deterministic_without_key_errors() {
        let r = KeyRing::from_master_hex(KEY_A, &[], None, salt()).unwrap();
        assert!(matches!(
            r.encrypt(Mode::Deterministic, b"x"),
            Err(EncryptionError::NoDeterministicKey)
        ));
    }

    #[test]
    fn envelope_header_is_documented_format() {
        use base64::Engine as _;
        let r = ring();
        let env = r.encrypt(Mode::Randomized, b"data").unwrap();
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&env)
            .unwrap();
        assert_eq!(raw[0], MAGIC);
        assert_eq!(raw[1], VERSION);
        assert_eq!(raw[2], ALG_AES_256_GCM);
        assert_eq!(raw[3], MODE_RANDOMIZED);
        let key_id = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
        assert_eq!(key_id, r.primary_key_id());
        assert!(raw.len() > HEADER_LEN);
    }

    #[test]
    fn key_id_is_stable_across_rebuilds() {
        let r1 = KeyRing::from_master_hex(KEY_A, &[], None, salt()).unwrap();
        let r2 = KeyRing::from_master_hex(KEY_A, &[], None, salt()).unwrap();
        assert_eq!(r1.primary_key_id(), r2.primary_key_id());
    }

    #[test]
    fn rotation_reads_old_writes_with_retired_key() {
        // Write under A as primary.
        let old = KeyRing::from_master_hex(KEY_A, &[], None, salt()).unwrap();
        let written = old.encrypt(Mode::Randomized, b"legacy-row").unwrap();

        // Rotate: B is now primary, A retired.
        let rotated = KeyRing::from_master_hex(KEY_B, &[KEY_A.to_string()], None, salt()).unwrap();
        // Old row still decrypts (no rewrite needed).
        assert_eq!(rotated.decrypt(&written).unwrap(), b"legacy-row");
        // New writes use the new primary key id.
        let fresh = rotated.encrypt(Mode::Randomized, b"new-row").unwrap();
        assert_ne!(rotated.primary_key_id(), old.primary_key_id());
        assert_eq!(rotated.decrypt(&fresh).unwrap(), b"new-row");
    }

    #[test]
    fn fully_retired_key_id_is_named_in_error() {
        let old = KeyRing::from_master_hex(KEY_A, &[], None, salt()).unwrap();
        let written = old.encrypt(Mode::Randomized, b"x").unwrap();
        // New ring without A at all.
        let other = KeyRing::from_master_hex(KEY_B, &[], None, salt()).unwrap();
        assert!(matches!(
            other.decrypt(&written),
            Err(EncryptionError::UnknownKeyId(_))
        ));
    }

    #[test]
    fn wrong_salt_fails_decryption() {
        let a = KeyRing::from_master_hex(KEY_A, &[], None, salt()).unwrap();
        let b = KeyRing::from_master_hex(KEY_A, &[], None, salt2()).unwrap();
        let env = a.encrypt(Mode::Randomized, b"x").unwrap();
        // Different salt derives a different data key -> different id -> unknown.
        assert!(b.decrypt(&env).is_err());
    }

    #[test]
    fn invalid_key_hex_is_rejected() {
        assert!(matches!(
            KeyRing::from_master_hex("tooshort", &[], None, salt()),
            Err(EncryptionError::InvalidKeyFormat { .. })
        ));
    }

    #[test]
    fn credentials_namespace_builds_ring() {
        let creds = EncryptionCredentials {
            primary_key: KEY_A.to_string(),
            deterministic_key: Some(DET.to_string()),
            key_derivation_salt: Some(hex::encode(salt())),
            retired_keys: vec![],
        };
        let ring = creds.to_key_ring().unwrap();
        let env = ring.encrypt(Mode::Randomized, b"z").unwrap();
        assert_eq!(ring.decrypt(&env).unwrap(), b"z");
    }

    #[test]
    fn missing_salt_is_rejected() {
        let creds = EncryptionCredentials {
            primary_key: KEY_A.to_string(),
            deterministic_key: None,
            key_derivation_salt: None,
            retired_keys: vec![],
        };
        assert!(matches!(
            creds.to_key_ring(),
            Err(EncryptionError::MissingSalt)
        ));
    }

    #[test]
    fn versioned_ciphertext_column_excluded_from_sensitive_marker() {
        // `versioned_ciphertext` columns store ciphertext, so they must NOT be in
        // the sensitive list (which would suppress their values entirely).
        let mut cols: Vec<&'static str> = Vec::new();
        merge_encrypted_columns_for_table("vc_table", &mut cols);
        assert!(
            !cols.contains(&"vc_col"),
            "versioned_ciphertext excluded: {cols:?}"
        );
    }

    #[test]
    fn versioned_ciphertext_payload_is_deterministic_ciphertext_not_plaintext() {
        install_key_ring(KeyRing::from_master_hex(KEY_A, &[], Some(DET), salt()).unwrap());

        let mut v = serde_json::json!({ "id": 1, "vc_col": "topsecret", "plain": "ok" });
        encrypt_versioned_columns_in_value("vc_table", &mut v);

        let stored = v["vc_col"].as_str().unwrap();
        assert_ne!(stored, "topsecret", "version payload must be ciphertext");
        assert!(!stored.contains("topsecret"), "no plaintext leak: {stored}");
        assert_eq!(v["plain"], "ok", "non-encrypted column untouched");

        // Deterministic: equal plaintext -> equal ciphertext, so the diff stays
        // accurate (an unchanged value is not reported as changed).
        let mut v2 = serde_json::json!({ "vc_col": "topsecret" });
        encrypt_versioned_columns_in_value("vc_table", &mut v2);
        assert_eq!(v2["vc_col"], v["vc_col"]);

        // And it is genuinely decryptable back to the plaintext.
        let ring = key_ring().unwrap();
        assert_eq!(
            String::from_utf8(ring.decrypt(stored).unwrap()).unwrap(),
            "topsecret"
        );
    }

    #[test]
    fn derived_query_param_encoding_matches_column_mode() {
        install_key_ring(KeyRing::from_master_hex(KEY_A, &[], Some(DET), salt()).unwrap());

        // Deterministic column: param is encoded to the same stable ciphertext the
        // wrapper writes, so the equality predicate matches stored rows.
        let encoded =
            encode_derived_query_param("dq_table", "det_col", "alice@example.com").unwrap();
        assert_ne!(encoded, "alice@example.com", "must be ciphertext");
        assert_eq!(
            encoded,
            deterministic_ciphertext("alice@example.com").unwrap(),
            "must equal the deterministic ciphertext used on write"
        );

        // Randomized column: equality lookups are impossible by design -> error,
        // and the message names the offending column and the fix.
        let err = encode_derived_query_param("dq_table", "rnd_col", "x").unwrap_err();
        assert!(matches!(
            err,
            EncryptionError::RandomizedEqualityLookup { .. }
        ));
        let msg = err.to_string();
        assert!(msg.contains("dq_table.rnd_col"), "names the column: {msg}");
        assert!(msg.contains("deterministic"), "suggests the fix: {msg}");

        // Non-encrypted column: pure passthrough.
        assert_eq!(
            encode_derived_query_param("dq_table", "not_encrypted", "plain").unwrap(),
            "plain"
        );
    }

    #[test]
    fn decrypt_persisted_columns_handles_each_branch() {
        install_key_ring(KeyRing::from_master_hex(KEY_A, &[], Some(DET), salt()).unwrap());

        // A persisted snapshot: a recoverable envelope, a null, and a column that
        // is not registered as encrypted.
        let envelope = encrypt_text(Mode::Deterministic, "secret@x.com").unwrap();
        let mut v = serde_json::json!({
            "det_col": envelope,
            "rnd_col": serde_json::Value::Null,
            "not_registered": "plain",
        });
        decrypt_persisted_columns_in_value("dq_table", &mut v);
        assert_eq!(
            v["det_col"], "secret@x.com",
            "recoverable envelope decrypts"
        );
        assert!(v["rnd_col"].is_null(), "null is left untouched");
        assert_eq!(
            v["not_registered"], "plain",
            "unregistered column untouched"
        );

        // A non-recoverable value (e.g. the `<encrypted>` fallback marker or a
        // legacy plaintext row) is left exactly as-is so reconstruction proceeds.
        let mut marker = serde_json::json!({ "det_col": "<encrypted>" });
        decrypt_persisted_columns_in_value("dq_table", &mut marker);
        assert_eq!(marker["det_col"], "<encrypted>");

        // A non-object value passes through without panicking.
        let mut arr = serde_json::json!([1, 2, 3]);
        decrypt_persisted_columns_in_value("dq_table", &mut arr);
        assert!(arr.is_array());
    }

    #[test]
    fn admin_visibility_helper_respects_opt_in() {
        assert!(
            admin_redacts_column_name("vc_col"),
            "default column is redacted"
        );
        assert!(
            !admin_redacts_column_name("visible_col"),
            "admin_visible column is not redacted"
        );
        assert!(
            !admin_redacts_column_name("not_encrypted_at_all"),
            "non-encrypted column is not forced-redacted by this helper"
        );
    }
}
