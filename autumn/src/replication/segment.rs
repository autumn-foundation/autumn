//! The on-destination object namespace and payload framing for continuous
//! `SQLite` replication (issue #1628).
//!
//! # Layout
//!
//! ```text
//! {prefix}/{profile}/generations/{generation}/snapshot.db.gz   # byte-faithful base
//! {prefix}/{profile}/generations/{generation}/snapshot.json    # commit marker + digest
//! {prefix}/{profile}/generations/{generation}/segments/{index:05}-{seq:010}-{ms:013}.seg
//! ```
//!
//! A **generation** is one base snapshot of the main database file plus every
//! WAL byte range written on top of it. Within a generation, an **index** is the
//! lifetime of one WAL salt sequence: the replicator's own checkpoint folds the
//! current index into the database file and opens the next one at WAL offset `0`.
//!
//! That two-level shape is what keeps replication cheap on a busy database. A
//! checkpoint is unavoidable — the `-wal` cannot grow forever — but re-uploading
//! the whole database every time one fires would mean gigabytes of write
//! amplification per hour. Instead a checkpoint costs one index bump, and a
//! fresh base snapshot is taken only on the configured snapshot interval, which
//! also bounds how much a restore has to replay. Its id is `{created_ms:013}-{salt1:08x}{salt2:08x}`, so a plain
//! lexicographic listing is also chronological and the salt is recoverable from
//! the key alone.
//!
//! `snapshot.json` is written **after** `snapshot.db.gz` and acts as the
//! generation's commit marker: a generation without it was interrupted mid-upload
//! and restore ignores it. Segments need no such marker — an object store PUT is
//! atomic, so a segment either exists whole or not at all.
//!
//! # Segment payload
//!
//! A segment is a JSON header line, a `\n`, then the gzip'd raw WAL byte range:
//!
//! ```text
//! {"version":1,"seq":0,"start_offset":0,...}\n<gzip bytes>
//! ```
//!
//! Self-describing on purpose — a segment can be inspected without an index, and
//! the header's `sha256`/`uncompressed_len` let restore refuse a payload the
//! destination silently mangled (reverse-brainstorm R6).

// autumn-panic-gate: durability-critical module — production code path must be
// panic-free. See CONTRIBUTING.md "Request-path panic gate". Justify exceptions
// with #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::fmt;
use std::io::Write as _;

use serde::{Deserialize, Serialize};

/// Version of the object layout and payload framing. Bumped only by a
/// breaking change; restore refuses a version it does not understand.
pub const LAYOUT_VERSION: u32 = 1;

/// Object name of a generation's base snapshot.
pub const SNAPSHOT_OBJECT: &str = "snapshot.db.gz";

/// Object name of a generation's commit marker / snapshot metadata.
pub const SNAPSHOT_META_OBJECT: &str = "snapshot.json";

/// Hard ceiling on the uncompressed size of one segment (1 GiB).
///
/// A segment is one WAL byte range between commits, so a real one is bounded by
/// the replicator's checkpoint threshold — orders of magnitude below this. The
/// ceiling exists so a crafted or corrupted header cannot make a restore (or the
/// in-process verifier) allocate without bound.
pub const MAX_SEGMENT_BYTES: u64 = 1024 * 1024 * 1024;

/// Why a key or payload could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SegmentError {
    /// The payload has no JSON header line.
    MissingHeader,
    /// The JSON header line did not parse.
    BadHeader {
        /// Parser detail (never carries payload bytes).
        detail: String,
    },
    /// The payload was written by a newer layout version.
    UnsupportedVersion {
        /// The version found in the payload.
        version: u32,
    },
    /// The gzip body did not decompress.
    Decompress {
        /// Decompressor detail.
        detail: String,
    },
    /// The decompressed body does not match the header's digest or length.
    DigestMismatch {
        /// Bytes the header promised.
        expected_len: u64,
        /// Bytes actually recovered.
        actual_len: u64,
    },
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHeader => write!(f, "segment payload has no header line"),
            Self::BadHeader { detail } => write!(f, "segment header did not parse: {detail}"),
            Self::UnsupportedVersion { version } => write!(
                f,
                "segment layout version {version} is not the one this build understands \
                 ({LAYOUT_VERSION}) — upgrade autumn before restoring"
            ),
            Self::Decompress { detail } => write!(f, "segment body did not decompress: {detail}"),
            Self::DigestMismatch {
                expected_len,
                actual_len,
            } => write!(
                f,
                "segment body failed verification (header promised {expected_len} byte(s), \
                 recovered {actual_len})"
            ),
        }
    }
}

impl std::error::Error for SegmentError {}

/// Metadata carried in a segment payload's header line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentHeader {
    /// Layout version ([`LAYOUT_VERSION`]).
    pub version: u32,
    /// Zero-based WAL index inside the generation. Bumped by the replicator's
    /// own checkpoint, which restarts the WAL at offset `0` under a new salt.
    #[serde(default)]
    pub index: u32,
    /// Zero-based position of this segment inside its **index**.
    pub seq: u64,
    /// Byte offset into the `-wal` file this range starts at. Segment `0`
    /// starts at `0`, so it carries the 32-byte WAL header.
    pub start_offset: u64,
    /// Byte offset just past this range (always a commit boundary).
    pub end_offset: u64,
    /// WAL frames in this range.
    pub frame_count: u64,
    /// Commit frames in this range.
    pub commit_count: u64,
    /// Database page size, from the WAL header.
    pub page_size: u32,
    /// Database size in pages as of this range's last commit.
    pub db_size_pages: u32,
    /// WAL salt-1 — must match the generation's.
    pub salt1: u32,
    /// WAL salt-2 — must match the generation's.
    pub salt2: u32,
    /// When the range was shipped (RFC 3339, UTC).
    pub created_at: String,
    /// Milliseconds since the Unix epoch for `created_at`, so ordering needs no
    /// date parsing.
    pub created_ms: i64,
    /// Lowercase hex SHA-256 of the uncompressed WAL bytes.
    pub sha256: String,
    /// Length of the uncompressed WAL bytes.
    pub uncompressed_len: u64,
}

/// Metadata stored alongside a generation's base snapshot (`snapshot.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotMeta {
    /// Layout version ([`LAYOUT_VERSION`]).
    pub version: u32,
    /// The generation this snapshot opens.
    pub generation: String,
    /// When the snapshot was taken (RFC 3339, UTC).
    pub created_at: String,
    /// Milliseconds since the Unix epoch for `created_at`.
    pub created_ms: i64,
    /// Lowercase hex SHA-256 of the **uncompressed** database file.
    pub sha256: String,
    /// Length of the uncompressed database file.
    pub uncompressed_len: u64,
}

/// The key components a generation id encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenerationInfo {
    /// Milliseconds since the Unix epoch when the generation opened.
    pub created_ms: i64,
    /// Random disambiguator.
    pub nonce: u64,
}

/// A segment key parsed back into its ordering components.
///
/// Ordered by `(index, seq)` — replication order — so a plain sort of parsed
/// keys is already replay order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SegmentRef {
    /// WAL index inside the generation.
    pub index: u32,
    /// Position inside that index.
    pub seq: u64,
    /// Milliseconds since the Unix epoch when the segment was shipped.
    pub created_ms: i64,
}

/// Build a generation id: `{created_ms:013}-{nonce:016x}`.
///
/// Zero-padded so lexicographic order matches chronological order for every
/// timestamp up to the year 2286; a negative (pre-epoch) timestamp is clamped to
/// `0` rather than producing a key with a `-` in the middle.
#[must_use]
pub fn generation_id(created_ms: i64, nonce: u64) -> String {
    let ms = created_ms.max(0);
    format!("{ms:013}-{nonce:016x}")
}

/// Parse a generation id produced by [`generation_id`].
#[must_use]
pub fn parse_generation_id(id: &str) -> Option<GenerationInfo> {
    let (ms, salts) = id.split_once('-')?;
    if ms.len() < 13 || salts.len() != 16 {
        return None;
    }
    let created_ms: i64 = ms.parse().ok()?;
    Some(GenerationInfo {
        created_ms,
        nonce: u64::from_str_radix(salts, 16).ok()?,
    })
}

/// The key prefix every object for one app/profile lives under.
///
/// `prefix` is the operator-configured bucket prefix (`None` or empty = bucket
/// root). Leading/trailing slashes are normalized away so `"db/"`, `"/db"` and
/// `"db"` all produce the same namespace.
#[must_use]
pub fn root_prefix(prefix: Option<&str>, profile: &str) -> String {
    let profile = profile.trim_matches('/');
    prefix
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty())
        .map_or_else(|| profile.to_owned(), |p| format!("{p}/{profile}"))
}

/// Key prefix that lists every generation under `root`.
#[must_use]
pub fn generations_prefix(root: &str) -> String {
    format!("{root}/generations/")
}

/// Key prefix of one generation's objects.
#[must_use]
pub fn generation_prefix(root: &str, generation: &str) -> String {
    format!("{root}/generations/{generation}/")
}

/// Key of a generation's base snapshot.
#[must_use]
pub fn snapshot_key(root: &str, generation: &str) -> String {
    format!("{root}/generations/{generation}/{SNAPSHOT_OBJECT}")
}

/// Key of a generation's snapshot metadata / commit marker.
#[must_use]
pub fn snapshot_meta_key(root: &str, generation: &str) -> String {
    format!("{root}/generations/{generation}/{SNAPSHOT_META_OBJECT}")
}

/// Key prefix that lists one generation's segments.
#[must_use]
pub fn segments_prefix(root: &str, generation: &str) -> String {
    format!("{root}/generations/{generation}/segments/")
}

/// Key of one segment: `{index:05}-{seq:010}-{created_ms:013}.seg`.
///
/// Every field is zero-padded so a lexicographic listing is already in
/// replication order, and the shipping time is readable from the key — which is
/// what lets a point-in-time restore choose segments without downloading them.
#[must_use]
pub fn segment_key(root: &str, generation: &str, index: u32, seq: u64, created_ms: i64) -> String {
    let ms = created_ms.max(0);
    format!("{root}/generations/{generation}/segments/{index:05}-{seq:010}-{ms:013}.seg")
}

/// Parse the trailing file name of a segment key.
#[must_use]
pub fn parse_segment_key(key: &str) -> Option<SegmentRef> {
    let name = key.rsplit('/').next()?;
    let stem = name.strip_suffix(".seg")?;
    let mut parts = stem.split('-');
    let index = parts.next()?.parse().ok()?;
    let seq = parts.next()?.parse().ok()?;
    let created_ms = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(SegmentRef {
        index,
        seq,
        created_ms,
    })
}

/// Extract the generation id from any key under [`generations_prefix`].
#[must_use]
pub fn generation_of_key(root: &str, key: &str) -> Option<String> {
    let rest = key.strip_prefix(&generations_prefix(root))?;
    let generation = rest.split('/').next()?;
    if generation.is_empty() {
        None
    } else {
        Some(generation.to_owned())
    }
}

/// Lowercase hex SHA-256 of `data`.
///
/// Re-exported from [`crate::sigv4`] so the whole crate hashes through one
/// implementation.
#[must_use]
pub fn sha256_hex(data: &[u8]) -> String {
    crate::sigv4::sha256_hex(data)
}

/// Frame a WAL byte range into a segment payload: header line, `\n`, gzip body.
///
/// # Errors
///
/// Returns [`SegmentError::Decompress`] if the gzip encoder fails (only possible
/// on allocation failure) or [`SegmentError::BadHeader`] if the header cannot be
/// serialized.
pub fn encode_segment(header: &SegmentHeader, raw: &[u8]) -> Result<Vec<u8>, SegmentError> {
    let mut out = serde_json::to_vec(header).map_err(|e| SegmentError::BadHeader {
        detail: e.to_string(),
    })?;
    out.push(b'\n');
    let mut encoder = flate2::write::GzEncoder::new(out, flate2::Compression::default());
    encoder
        .write_all(raw)
        .map_err(|e| SegmentError::Decompress {
            detail: e.to_string(),
        })?;
    encoder.finish().map_err(|e| SegmentError::Decompress {
        detail: e.to_string(),
    })
}

/// Parse and **verify** a segment payload, returning its header and the raw WAL
/// bytes it carries.
///
/// Verification is not optional: the digest and length recorded at ship time
/// must match the bytes recovered here, so a destination that silently truncated
/// or mangled an object is refused rather than handed to `SQLite`'s recovery,
/// which would stop at the damage without saying so.
///
/// # Errors
///
/// See [`SegmentError`].
pub fn decode_segment(payload: &[u8]) -> Result<(SegmentHeader, Vec<u8>), SegmentError> {
    let newline = payload
        .iter()
        .position(|b| *b == b'\n')
        .ok_or(SegmentError::MissingHeader)?;
    let head = payload.get(..newline).ok_or(SegmentError::MissingHeader)?;
    let body_start = newline.checked_add(1).ok_or(SegmentError::MissingHeader)?;
    let body = payload
        .get(body_start..)
        .ok_or(SegmentError::MissingHeader)?;

    let header: SegmentHeader =
        serde_json::from_slice(head).map_err(|e| SegmentError::BadHeader {
            detail: e.to_string(),
        })?;
    if header.version != LAYOUT_VERSION {
        return Err(SegmentError::UnsupportedVersion {
            version: header.version,
        });
    }
    // A segment is one WAL byte range, bounded in practice by the checkpoint
    // threshold. Refuse a header that claims more than any real segment can hold
    // BEFORE inflating anything: the periodic verifier runs this inside the app
    // process, so an unbounded inflate of a crafted object is an OOM of the whole
    // app, not just a failed restore.
    if header.uncompressed_len > MAX_SEGMENT_BYTES {
        return Err(SegmentError::DigestMismatch {
            expected_len: header.uncompressed_len,
            actual_len: 0,
        });
    }

    let mut raw = Vec::new();
    let decoder = flate2::read::GzDecoder::new(body);
    // One byte past the declared length: enough to notice the payload is longer
    // than it claims, never enough to be a decompression bomb.
    let mut decoder = std::io::Read::take(decoder, header.uncompressed_len.saturating_add(1));
    std::io::copy(&mut decoder, &mut raw).map_err(|e| SegmentError::Decompress {
        detail: e.to_string(),
    })?;

    let actual_len = raw.len() as u64;
    if actual_len != header.uncompressed_len || sha256_hex(&raw) != header.sha256 {
        return Err(SegmentError::DigestMismatch {
            expected_len: header.uncompressed_len,
            actual_len,
        });
    }
    Ok((header, raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(seq: u64, start: u64, end: u64, raw: &[u8]) -> SegmentHeader {
        SegmentHeader {
            version: LAYOUT_VERSION,
            index: 0,
            seq,
            start_offset: start,
            end_offset: end,
            frame_count: 1,
            commit_count: 1,
            page_size: 4096,
            db_size_pages: 3,
            salt1: 1,
            salt2: 2,
            created_at: "2026-09-02T00:00:00Z".to_owned(),
            created_ms: 1_788_000_000_000,
            sha256: sha256_hex(raw),
            uncompressed_len: raw.len() as u64,
        }
    }

    #[test]
    fn generation_ids_sort_chronologically_and_round_trip() {
        let early = generation_id(1_000, 0xAABB_CCDD_0011_2233);
        let late = generation_id(2_000, 1);
        assert!(early < late, "{early} must sort before {late}");
        assert_eq!(
            parse_generation_id(&early),
            Some(GenerationInfo {
                created_ms: 1_000,
                nonce: 0xAABB_CCDD_0011_2233,
            })
        );
        assert_eq!(parse_generation_id("nope"), None);
        assert_eq!(parse_generation_id("0000000001000-abc"), None);
    }

    #[test]
    fn generation_id_clamps_a_pre_epoch_timestamp() {
        let id = generation_id(-5, 2);
        let (ms, _) = id.split_once('-').expect("id has a separator");
        assert!(
            ms.parse::<u64>().is_ok(),
            "the timestamp component must stay unsigned: {id}"
        );
        assert_eq!(parse_generation_id(&id).map(|g| g.created_ms), Some(0));
        assert!(
            generation_id(0, 2) <= generation_id(1, 2),
            "clamping must not break ordering"
        );
    }

    #[test]
    fn root_prefix_normalizes_slashes() {
        assert_eq!(root_prefix(None, "prod"), "prod");
        assert_eq!(root_prefix(Some(""), "prod"), "prod");
        assert_eq!(root_prefix(Some("db"), "prod"), "db/prod");
        assert_eq!(root_prefix(Some("/db/"), "/prod/"), "db/prod");
    }

    #[test]
    fn segment_keys_sort_in_replication_order_and_round_trip() {
        let root = root_prefix(Some("db"), "prod");
        let generation = generation_id(1_000, 2);
        let a = segment_key(&root, &generation, 0, 9, 1_500);
        let b = segment_key(&root, &generation, 0, 10, 1_600);
        let c = segment_key(&root, &generation, 1, 0, 1_700);
        assert!(a < b, "seq 9 must sort before seq 10 ({a} vs {b})");
        assert!(
            b < c,
            "index 0 must sort before index 1 whatever the seq ({b} vs {c})"
        );
        assert_eq!(
            parse_segment_key(&b),
            Some(SegmentRef {
                index: 0,
                seq: 10,
                created_ms: 1_600
            })
        );
        assert_eq!(
            parse_segment_key(&c),
            Some(SegmentRef {
                index: 1,
                seq: 0,
                created_ms: 1_700
            })
        );
        assert_eq!(
            generation_of_key(&root, &b).as_deref(),
            Some(generation.as_str())
        );
        assert_eq!(
            parse_segment_key("db/prod/generations/g/segments/x.seg"),
            None
        );
        assert_eq!(parse_segment_key(&snapshot_key(&root, &generation)), None);
    }

    #[test]
    fn segment_payload_round_trips() {
        let raw = b"the quick brown fox jumps over the lazy dog".repeat(64);
        let encoded = encode_segment(&header(0, 0, 4120, &raw), &raw).expect("encode");
        assert!(
            encoded.len() < raw.len(),
            "a compressible payload should shrink"
        );
        let (decoded_header, decoded) = decode_segment(&encoded).expect("decode");
        assert_eq!(decoded, raw);
        assert_eq!(decoded_header.seq, 0);
        assert_eq!(decoded_header.end_offset, 4120);
    }

    #[test]
    fn decode_refuses_a_tampered_body() {
        let raw = b"payload".repeat(32);
        let mut encoded = encode_segment(&header(1, 0, 32, &raw), &raw).expect("encode");
        let last = encoded.len().saturating_sub(8);
        encoded[last] ^= 0xFF;
        let err = decode_segment(&encoded).expect_err("tampered body must be refused");
        assert!(
            matches!(
                err,
                SegmentError::DigestMismatch { .. } | SegmentError::Decompress { .. }
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn decode_refuses_a_digest_that_does_not_match_the_body() {
        let raw = b"payload".repeat(32);
        let mut head = header(1, 0, 32, &raw);
        head.sha256 = sha256_hex(b"something else");
        let encoded = encode_segment(&head, &raw).expect("encode");
        assert_eq!(
            decode_segment(&encoded).expect_err("digest mismatch must be refused"),
            SegmentError::DigestMismatch {
                expected_len: raw.len() as u64,
                actual_len: raw.len() as u64,
            }
        );
    }

    #[test]
    fn decode_refuses_a_future_layout_version() {
        let raw = b"payload".to_vec();
        let mut head = header(0, 0, 8, &raw);
        head.version = LAYOUT_VERSION.saturating_add(1);
        let encoded = encode_segment(&head, &raw).expect("encode");
        assert_eq!(
            decode_segment(&encoded).expect_err("future version must be refused"),
            SegmentError::UnsupportedVersion {
                version: LAYOUT_VERSION + 1
            }
        );
    }

    #[test]
    fn decode_refuses_a_payload_without_a_header_line() {
        assert_eq!(
            decode_segment(b"no newline here").expect_err("must refuse"),
            SegmentError::MissingHeader
        );
    }
}
