//! Operator-blind confidential fields (issue #1771).
//!
//! [`encryption`](crate::encryption) protects columns at rest under keys **the
//! operator holds**: it stops a stolen disk, not a rogue admin, a subpoena or a
//! leaked backup. This module removes the operator from the trust boundary for
//! the fields an application marks `#[confidential]`.
//!
//! A confidential value is sealed **on the client**, under a [`RootKey`] the
//! server never receives. The server's only representation of the value is
//! [`Sealed`] — an opaque envelope with no accessor that yields plaintext, no
//! `Display`, and a redacted `Debug`. Ciphertext is therefore what flows into
//! every operator-reachable sink by construction: the database, the access log,
//! `autumn db backup` output, a replay capsule, record version history and the
//! admin UI. Equality lookups still work, through a client-computed
//! [`BlindIndex`] token.
//!
//! ```ignore
//! #[autumn_web::model(table = "notes")]
//! pub struct Note {
//!     pub id: i32,
//!     pub owner_id: String,
//!     #[confidential(blind_index)]
//!     pub body: Sealed,
//!     pub body_bidx: BlindIndex,
//! }
//!
//! // Client side (never on the server):
//! let ctx = FieldContext::new("notes", "body", &owner_id);
//! let sealed = key.seal(&ctx, "my diagnosis")?;
//! let token = key.blind_index(&ctx, "my diagnosis");
//! ```
//!
//! # Envelope format
//!
//! A sealed value is base64 ([`STANDARD`](base64::engine::general_purpose::STANDARD))
//! over this binary envelope:
//!
//! ```text
//! byte  0       magic   = 0xCF        (Autumn confidential field)
//! byte  1       version = 0x01
//! byte  2       alg     = 0x01        (AES-256-GCM)
//! bytes 3..15   nonce   : 12 bytes
//! bytes 15..    ciphertext + 16-byte AES-GCM authentication tag
//! ```
//!
//! There is no key id: the key is the client's, and the server has no key ring
//! to select from.
//!
//! # Key derivation
//!
//! Each field gets its own keys, derived from the root key and the
//! [`FieldContext`] (table, column, owner):
//!
//! ```text
//! context    = table || 0x1F || column || 0x1F || owner
//! seal_key   = HMAC-SHA256(root, "autumn:confidential:seal:v1:"  || context)
//! index_key  = HMAC-SHA256(root, "autumn:confidential:index:v1:" || context)
//! token      = hex(HMAC-SHA256(index_key, "autumn:confidential:bidx:v1:" || plaintext)[0..16])
//! ```
//!
//! The same `context` is the AES-GCM associated data, so an envelope moved to
//! another row, column, table or owner fails to authenticate. That is what makes
//! a sealed value un-replayable by whoever can write the database.
//!
//! # What the operator can still see
//!
//! Sealing hides the value, not the record. See [`OPERATOR_VISIBLE`] and
//! [`OPERATOR_BLIND_SINKS`], and `docs/guide/confidential-fields.md` for the
//! full threat model.

use std::fmt;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::KeyInit;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroize as _;

/// Envelope magic byte. Distinct from [`crate::encryption`]'s `0xA7`, so the two
/// formats can never be confused for one another.
const MAGIC: u8 = 0xCF;
const VERSION: u8 = 0x01;
const ALG_AES_256_GCM: u8 = 0x01;
const NONCE_LEN: usize = 12;
const HEADER_LEN: usize = 3 + NONCE_LEN;
/// AES-GCM authentication tag length. An envelope shorter than header + tag
/// cannot hold a valid ciphertext.
const TAG_LEN: usize = 16;
/// Separator between the parts of a field context. `0x1F` (ASCII unit
/// separator) cannot appear in a table or column name, so the context is
/// unambiguous: `("ab", "c")` and `("a", "bc")` derive different keys.
const SEP: u8 = 0x1F;
/// Bytes of HMAC output kept in a blind-index token: 128 bits, which makes an
/// accidental collision negligible while keeping the token short.
const TOKEN_BYTES: usize = 16;

type HmacSha256 = Hmac<Sha256>;

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

/// Errors produced when sealing, unsealing or parsing a confidential value.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfidentialError {
    /// The root key was not valid 64-character hex.
    #[error("invalid root key: expected 64 hex characters, got {len}")]
    InvalidKeyFormat {
        /// Length of the supplied string.
        len: usize,
    },

    /// The envelope is not parseable (bad base64, bad magic, truncated).
    #[error("malformed sealed envelope: {0}")]
    MalformedEnvelope(&'static str),

    /// The envelope uses a version or algorithm this build does not know.
    #[error("unsupported sealed envelope (version={version:#04x}, alg={alg:#04x})")]
    UnsupportedEnvelope {
        /// Envelope version byte.
        version: u8,
        /// Algorithm id byte.
        alg: u8,
    },

    /// AEAD authentication failed: the wrong key, the wrong field context, or
    /// corrupted ciphertext.
    #[error("unseal failed: wrong key, wrong field context, or corrupted ciphertext")]
    UnsealFailed,

    /// The recovered plaintext was not UTF-8.
    #[error("unsealed value is not valid UTF-8")]
    NotUtf8,

    /// A blind-index token was not 32 lowercase hex characters.
    #[error("invalid blind-index token: expected {expected} lowercase hex characters")]
    InvalidToken {
        /// The required token length.
        expected: usize,
    },
}

// ---------------------------------------------------------------------------
// Field context
// ---------------------------------------------------------------------------

/// Names the field a value belongs to: its table, its column and the owner whose
/// key seals it.
///
/// The context does two jobs. It derives the field's keys, so one root key gives
/// every column an independent key. And it is the AES-GCM associated data, so an
/// envelope is bound to the exact place it was written: an operator who copies
/// one user's ciphertext into another user's row produces a value that no longer
/// unseals.
#[derive(Clone, PartialEq, Eq)]
pub struct FieldContext {
    bytes: Vec<u8>,
}

impl FieldContext {
    /// Build the context for `owner`'s value in `table`.`column`.
    #[must_use]
    pub fn new(table: &str, column: &str, owner: &str) -> Self {
        let mut bytes = Vec::with_capacity(table.len() + column.len() + owner.len() + 2);
        bytes.extend_from_slice(table.as_bytes());
        bytes.push(SEP);
        bytes.extend_from_slice(column.as_bytes());
        bytes.push(SEP);
        bytes.extend_from_slice(owner.as_bytes());
        Self { bytes }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Redacted: the owner identifier is a stable per-user value, and `Debug` output
/// reaches logs and error pages.
impl fmt::Debug for FieldContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FieldContext(<redacted>)")
    }
}

// ---------------------------------------------------------------------------
// Root key
// ---------------------------------------------------------------------------

/// A client-held 32-byte root key. **The server must never hold one.**
///
/// The type has no `Serialize`, no `Display`, no `Clone` and no accessor for its
/// bytes, so no expression writes it to a log line, a response or a file. It
/// zeroizes on drop. The only inputs are [`RootKey::generate`],
/// [`RootKey::from_bytes`] and [`RootKey::from_hex`]; nothing reads it out of
/// configuration or the credentials store, which is what keeps a server build
/// from acquiring one by accident.
pub struct RootKey {
    bytes: [u8; 32],
}

impl RootKey {
    /// Draw a fresh root key from the operating system RNG.
    ///
    /// # Panics
    ///
    /// Panics if the operating system's random number generator is unavailable.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes).expect("OS RNG failed");
        Self { bytes }
    }

    /// Adopt 32 bytes of existing key material.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self { bytes }
    }

    /// Parse a 64-character hex key.
    ///
    /// # Errors
    ///
    /// Returns [`ConfidentialError::InvalidKeyFormat`] for anything else.
    pub fn from_hex(hex_str: &str) -> Result<Self, ConfidentialError> {
        let hex_str = hex_str.trim();
        let decoded = hex::decode(hex_str)
            .map_err(|_| ConfidentialError::InvalidKeyFormat { len: hex_str.len() })?;
        let bytes: [u8; 32] = decoded
            .try_into()
            .map_err(|_| ConfidentialError::InvalidKeyFormat { len: hex_str.len() })?;
        Ok(Self { bytes })
    }

    fn derive(&self, domain: &[u8], ctx: &FieldContext) -> [u8; 32] {
        let mut msg = Vec::with_capacity(domain.len() + ctx.as_bytes().len());
        msg.extend_from_slice(domain);
        msg.extend_from_slice(ctx.as_bytes());
        hmac_sha256(&self.bytes, &msg)
    }

    /// Seal `plaintext` for the field `ctx` names.
    ///
    /// # Errors
    ///
    /// Returns [`ConfidentialError::UnsealFailed`] only if the AEAD refuses the
    /// input, which cannot happen for a valid key and nonce.
    ///
    /// # Panics
    ///
    /// Panics if the operating system's random number generator is unavailable.
    pub fn seal(&self, ctx: &FieldContext, plaintext: &str) -> Result<Sealed, ConfidentialError> {
        use aes_gcm::Nonce;
        use aes_gcm::aead::{Aead, Payload};
        use base64::Engine as _;

        let mut seal_key = self.derive(b"autumn:confidential:seal:v1:", ctx);
        let cipher = Aes256Gcm::new_from_slice(&seal_key).expect("32-byte key");
        seal_key.zeroize();

        let mut nonce = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce).expect("OS RNG failed");

        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: ctx.as_bytes(),
                },
            )
            .map_err(|_| ConfidentialError::UnsealFailed)?;

        let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        out.push(MAGIC);
        out.push(VERSION);
        out.push(ALG_AES_256_GCM);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(Sealed(
            base64::engine::general_purpose::STANDARD.encode(out),
        ))
    }

    /// Recover the plaintext of `sealed` for the field `ctx` names.
    ///
    /// # Errors
    ///
    /// Returns [`ConfidentialError::UnsealFailed`] for the wrong key or a
    /// different field context, and [`ConfidentialError::NotUtf8`] if the
    /// recovered bytes are not text.
    pub fn unseal(&self, ctx: &FieldContext, sealed: &Sealed) -> Result<String, ConfidentialError> {
        use aes_gcm::Nonce;
        use aes_gcm::aead::{Aead, Payload};

        let raw = sealed.to_bytes()?;
        // `to_bytes` already validated the header, so these slices are in range.
        let nonce = &raw[3..HEADER_LEN];
        let ciphertext = &raw[HEADER_LEN..];

        let mut seal_key = self.derive(b"autumn:confidential:seal:v1:", ctx);
        let cipher = Aes256Gcm::new_from_slice(&seal_key).expect("32-byte key");
        seal_key.zeroize();

        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: ctx.as_bytes(),
                },
            )
            .map_err(|_| ConfidentialError::UnsealFailed)?;
        String::from_utf8(plaintext).map_err(|_| ConfidentialError::NotUtf8)
    }

    /// Compute the deterministic equality token for `plaintext` in the field
    /// `ctx` names.
    ///
    /// Equal plaintexts give equal tokens under one key and context, which is
    /// what makes `WHERE <column>_bidx = $1` work. Nothing else is derivable:
    /// the token is a keyed MAC of fixed length, so it reveals neither the
    /// plaintext nor its length, and an operator without the key cannot confirm
    /// a guessed plaintext by recomputing it.
    #[must_use]
    pub fn blind_index(&self, ctx: &FieldContext, plaintext: &str) -> BlindIndex {
        let mut index_key = self.derive(b"autumn:confidential:index:v1:", ctx);
        let mut msg = Vec::with_capacity(28 + plaintext.len());
        msg.extend_from_slice(b"autumn:confidential:bidx:v1:");
        msg.extend_from_slice(plaintext.as_bytes());
        let mac = hmac_sha256(&index_key, &msg);
        index_key.zeroize();
        BlindIndex(hex::encode(&mac[..TOKEN_BYTES]))
    }
}

impl Drop for RootKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl fmt::Debug for RootKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RootKey(<redacted>)")
    }
}

// ---------------------------------------------------------------------------
// Sealed
// ---------------------------------------------------------------------------

/// The server-side representation of a confidential value: an opaque envelope.
///
/// This is the type a `#[confidential]` column is declared as, so every struct
/// the model macro generates — insert, patch, changeset, factory, JSON view —
/// carries ciphertext and nothing else. The type has no `Display`, no `Deref`
/// and no accessor that returns plaintext. [`Sealed::as_envelope`] returns the
/// ciphertext, which is what the client needs and what the operator may already
/// read from the database.
#[derive(Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "db", derive(diesel::AsExpression, diesel::FromSqlRow))]
#[cfg_attr(feature = "db", diesel(sql_type = diesel::sql_types::Text))]
pub struct Sealed(String);

impl Sealed {
    /// Adopt an envelope string that arrived from a client or the database.
    ///
    /// # Errors
    ///
    /// Returns [`ConfidentialError::MalformedEnvelope`] or
    /// [`ConfidentialError::UnsupportedEnvelope`] when the header does not
    /// parse, so junk is refused at the boundary rather than stored.
    pub fn from_envelope(envelope: String) -> Result<Self, ConfidentialError> {
        let candidate = Self(envelope);
        candidate.to_bytes()?;
        Ok(candidate)
    }

    /// The base64 envelope, exactly as stored.
    #[must_use]
    pub fn as_envelope(&self) -> &str {
        &self.0
    }

    /// Decode and validate the envelope, returning its raw bytes.
    ///
    /// # Errors
    ///
    /// As [`Sealed::from_envelope`].
    pub fn to_bytes(&self) -> Result<Vec<u8>, ConfidentialError> {
        use base64::Engine as _;

        let raw = base64::engine::general_purpose::STANDARD
            .decode(self.0.trim())
            .map_err(|_| ConfidentialError::MalformedEnvelope("not valid base64"))?;
        if raw.len() < HEADER_LEN + TAG_LEN {
            return Err(ConfidentialError::MalformedEnvelope("truncated envelope"));
        }
        if raw[0] != MAGIC {
            return Err(ConfidentialError::MalformedEnvelope("bad magic byte"));
        }
        if raw[1] != VERSION || raw[2] != ALG_AES_256_GCM {
            return Err(ConfidentialError::UnsupportedEnvelope {
                version: raw[1],
                alg: raw[2],
            });
        }
        Ok(raw)
    }
}

/// An envelope no key opens.
///
/// The server cannot seal a value, so a confidential column has no meaningful
/// default. This exists because the `#[model]` factory and patch structs need
/// one. It is structurally valid, so it round-trips through the database, and
/// [`RootKey::unseal`] always refuses it — the honest outcome for a value
/// nobody sealed.
impl Default for Sealed {
    fn default() -> Self {
        use base64::Engine as _;
        let mut raw = Vec::with_capacity(HEADER_LEN + TAG_LEN);
        raw.push(MAGIC);
        raw.push(VERSION);
        raw.push(ALG_AES_256_GCM);
        raw.resize(HEADER_LEN + TAG_LEN, 0);
        Self(base64::engine::general_purpose::STANDARD.encode(raw))
    }
}

/// Redacted. The envelope is ciphertext, but `Debug` output reaches logs, panic
/// messages and error pages, where a per-user envelope is still a correlatable
/// identifier.
impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sealed(<sealed>)")
    }
}

impl Serialize for Sealed {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sealed {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let envelope = String::deserialize(deserializer)?;
        Self::from_envelope(envelope).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Blind index
// ---------------------------------------------------------------------------

/// A deterministic equality token for a confidential value.
///
/// The client computes it with [`RootKey::blind_index`] and sends it alongside
/// the sealed value. The server stores it in its own column and compares it,
/// which is the only server-side predicate a confidential field supports.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(feature = "db", derive(diesel::AsExpression, diesel::FromSqlRow))]
#[cfg_attr(feature = "db", diesel(sql_type = diesel::sql_types::Text))]
pub struct BlindIndex(String);

impl BlindIndex {
    /// Character length of a token: 16 bytes of HMAC, hex encoded.
    pub const TOKEN_LEN: usize = TOKEN_BYTES * 2;

    /// Adopt a token string that arrived from a client or the database.
    ///
    /// # Errors
    ///
    /// Returns [`ConfidentialError::InvalidToken`] unless the value is exactly
    /// [`BlindIndex::TOKEN_LEN`] lowercase hex characters.
    pub fn from_token(token: String) -> Result<Self, ConfidentialError> {
        let valid = token.len() == Self::TOKEN_LEN
            && token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(token))
        } else {
            Err(ConfidentialError::InvalidToken {
                expected: Self::TOKEN_LEN,
            })
        }
    }

    /// The token, as stored and compared.
    #[must_use]
    pub fn as_token(&self) -> &str {
        &self.0
    }
}

/// An all-zero token, which no plaintext produces.
///
/// Present for the same reason as [`Sealed`]'s: the `#[model]` factory and patch
/// structs need a default. It matches no client-computed token.
impl Default for BlindIndex {
    fn default() -> Self {
        Self("0".repeat(Self::TOKEN_LEN))
    }
}

impl Serialize for BlindIndex {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for BlindIndex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let token = String::deserialize(deserializer)?;
        Self::from_token(token).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Compile-time registration of a confidential column, emitted by `#[model]`.
///
/// Drives log-parameter scrubbing, version-history redaction and admin
/// redaction for surfaces that have no compile-time view of the model.
#[derive(Debug)]
pub struct ConfidentialColumnDescriptor {
    /// Model type name (e.g. `Note`).
    pub model: &'static str,
    /// Database table name.
    pub table: &'static str,
    /// Column holding the sealed envelope.
    pub column: &'static str,
    /// Companion column holding the blind-index token, when the field declared
    /// `#[confidential(blind_index)]`.
    pub blind_index: Option<&'static str>,
}

inventory::collect!(ConfidentialColumnDescriptor);

/// Every confidential column registered across the binary.
#[must_use]
pub fn registered_confidential_columns() -> Vec<&'static ConfidentialColumnDescriptor> {
    inventory::iter::<ConfidentialColumnDescriptor>
        .into_iter()
        .collect()
}

/// Distinct column names of every confidential column, blind-index companions
/// included.
///
/// Fed into the log parameter scrubber. The sealed column is ciphertext and the
/// token is already in the database, so neither is a plaintext leak; both are
/// per-user values that would let anyone reading a log correlate requests, so
/// both are filtered.
#[must_use]
pub fn registered_confidential_column_names() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for d in registered_confidential_columns() {
        names.push(d.column.to_owned());
        if let Some(bidx) = d.blind_index {
            names.push(bidx.to_owned());
        }
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// Whether `column` of `table` is a registered confidential column.
#[must_use]
pub fn is_confidential_column(table: &str, column: &str) -> bool {
    registered_confidential_columns()
        .iter()
        .any(|d| d.table == table && d.column == column)
}

/// Whether any registered confidential column has this name (table-agnostic).
///
/// Used by surfaces that lack table context, such as the admin cell renderer.
/// Errs toward privacy: a same-named column on another table is also redacted.
#[must_use]
pub fn is_confidential_column_name(column: &str) -> bool {
    registered_confidential_columns()
        .iter()
        .any(|d| d.column == column)
}

/// Confidential column names for one table.
#[must_use]
pub fn confidential_columns_for_table(table: &str) -> Vec<&'static str> {
    registered_confidential_columns()
        .iter()
        .filter(|d| d.table == table)
        .map(|d| d.column)
        .collect()
}

/// Append this table's confidential columns to `columns`, de-duplicating.
///
/// Used by generated `VersionedRecord::version_sensitive_columns`, so record
/// version history keeps a "changed" marker instead of copying the envelope
/// into a second table.
pub fn merge_confidential_columns_for_table(table: &str, columns: &mut Vec<&'static str>) {
    for d in registered_confidential_columns() {
        if d.table == table && !columns.contains(&d.column) {
            columns.push(d.column);
        }
    }
}

/// Whether `column` appears in `columns`.
///
/// A `const fn` so the `#[repository]` macro can refuse a server-side predicate
/// over a confidential column at build time, from the column list `#[model]`
/// publishes. Not part of the public API.
#[doc(hidden)]
#[must_use]
pub const fn __column_is_confidential(columns: &[&str], column: &str) -> bool {
    let mut i = 0;
    while i < columns.len() {
        if const_str_eq(columns[i], column) {
            return true;
        }
        i += 1;
    }
    false
}

const fn const_str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

// ---------------------------------------------------------------------------
// Threat model
// ---------------------------------------------------------------------------

/// One operator-reachable sink a confidential value passes through, and why the
/// operator reads only ciphertext there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorSink {
    /// Stable identifier, matching the heading in
    /// `docs/guide/confidential-fields.md`.
    pub id: &'static str,
    /// Why plaintext cannot reach this sink.
    pub why: &'static str,
}

/// The sinks a confidential value reaches, and the reason each holds only
/// ciphertext. This is the "the operator cannot see" set the guide documents and
/// `confidential_threat_model` asserts in CI.
pub const OPERATOR_BLIND_SINKS: &[OperatorSink] = &[
    OperatorSink {
        id: "database",
        why: "the column type is `Sealed`, so the only value bound into an INSERT \
              or UPDATE is the envelope",
    },
    OperatorSink {
        id: "access_log",
        why: "the access log carries no bodies, and confidential column names are \
              folded into the log parameter filter",
    },
    OperatorSink {
        id: "db_backup",
        why: "a backup is a dump of the database, which holds only envelopes",
    },
    OperatorSink {
        id: "replay_capsule",
        why: "a capsule copies the request body and the SQL binds, both of which \
              carry envelopes",
    },
    OperatorSink {
        id: "version_history",
        why: "confidential columns are version-sensitive, so a revision records \
              that the column changed, not what it changed to",
    },
    OperatorSink {
        id: "admin_ui",
        why: "the admin cell renderer redacts registered confidential columns",
    },
];

/// What sealing does **not** hide. Stated so the guarantee is not overclaimed.
pub const OPERATOR_VISIBLE: &[&str] = &[
    "that the row exists, and its id, timestamps and foreign keys",
    "the approximate length of the plaintext, from the length of the envelope",
    "the blind-index token, which is stable per value per owner, so the operator \
     can see when two of one owner's rows hold the same value",
    "every column the application did not mark `#[confidential]`",
];

#[cfg(feature = "db")]
mod diesel_types;
