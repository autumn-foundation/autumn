//! `SQLite` write-ahead-log (WAL) reading — the byte-level layer the continuous
//! replicator ships from (issue #1628).
//!
//! The replicator does not interpret SQL. It copies **raw WAL byte ranges** to
//! the destination and lets `SQLite`'s own recovery path re-apply them at restore
//! time, which is what makes the loop `O(bytes written)` rather than `O(database
//! size)` and keeps the apply path out of Autumn's hands entirely.
//!
//! That only works if the bytes we copy are exactly the bytes `SQLite` would have
//! recovered, so this module reimplements `SQLite`'s frame validation:
//!
//! * the 32-byte WAL header (magic, format version, page size, salts, and the
//!   header's own checksum over its first 24 bytes);
//! * the rolling frame checksum chain, seeded from the header checksum and
//!   carried through every frame's first 8 header bytes plus its page image;
//! * the **commit boundary** — a frame whose "database size after commit" field
//!   is non-zero. Only complete commits are shippable: a frame the writer was
//!   still appending fails the chain (or is short) and stops the scan, so a torn
//!   tail can never advance the replication point.
//!
//! The salt pair in the header identifies a **generation**. `SQLite` rewrites the
//! salts whenever the WAL restarts (after a checkpoint), which is precisely the
//! moment the previously shipped byte offsets stop meaning anything — so a salt
//! change forces the replicator to take a fresh base snapshot.
//!
//! Format reference: <https://www.sqlite.org/fileformat2.html#walformat>.

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
use std::path::{Path, PathBuf};

/// Size of the header that opens every `-wal` file.
pub const WAL_HEADER_SIZE: usize = 32;

/// Size of the frame header that precedes every WAL frame's page image.
pub const FRAME_HEADER_SIZE: usize = 24;

/// The WAL magic with its byte-order bit masked off. The low bit selects the
/// word order used by the checksum: `0x377f_0683` means big-endian words,
/// `0x377f_0682` little-endian.
const WAL_MAGIC_BASE: u32 = 0x377f_0682;

/// The only WAL format version `SQLite` writes (`WAL_MAX_VERSION`).
const WAL_FORMAT_VERSION: u32 = 3_007_000;

/// Smallest legal `SQLite` page size.
const MIN_PAGE_SIZE: u32 = 512;

/// Largest legal `SQLite` page size. The WAL header stores the page size as a
/// full 4-byte integer, so unlike the database header 65536 needs no encoding
/// trick.
const MAX_PAGE_SIZE: u32 = 65_536;

/// The `-wal` sidecar `SQLite` maintains next to `db`.
///
/// `SQLite` *appends* `-wal` to the full database filename (it does not replace an
/// extension), so `app.db` pairs with `app.db-wal`.
#[must_use]
pub fn wal_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push("-wal");
    PathBuf::from(name)
}

/// The `-shm` shared-memory sidecar `SQLite` maintains next to `db`.
#[must_use]
pub fn shm_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push("-shm");
    PathBuf::from(name)
}

/// Why a `-wal` file could not be read as a WAL.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WalError {
    /// Fewer than [`WAL_HEADER_SIZE`] bytes are present (an empty WAL included).
    TooShort {
        /// How many bytes were available.
        len: usize,
    },
    /// The leading magic is not a `SQLite` WAL magic.
    BadMagic {
        /// The magic that was read.
        magic: u32,
    },
    /// The WAL format version is not one this reader understands.
    UnsupportedVersion {
        /// The version that was read.
        version: u32,
    },
    /// The page size is not a power of two in `[512, 65536]`.
    PageSize {
        /// The page size that was read.
        page_size: u32,
    },
    /// The header's own checksum (bytes 0..24) does not validate.
    HeaderChecksum,
}

impl fmt::Display for WalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort { len } => write!(
                f,
                "the -wal file holds {len} byte(s), fewer than the {WAL_HEADER_SIZE}-byte WAL header"
            ),
            Self::BadMagic { magic } => {
                write!(f, "not a SQLite WAL: leading magic {magic:#010x}")
            }
            Self::UnsupportedVersion { version } => write!(
                f,
                "unsupported WAL format version {version} (expected {WAL_FORMAT_VERSION})"
            ),
            Self::PageSize { page_size } => write!(
                f,
                "illegal WAL page size {page_size} (expected a power of two in \
                 [{MIN_PAGE_SIZE}, {MAX_PAGE_SIZE}])"
            ),
            Self::HeaderChecksum => {
                write!(f, "the WAL header checksum does not validate")
            }
        }
    }
}

impl std::error::Error for WalError {}

/// A parsed 32-byte WAL header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalHeader {
    /// The raw magic, including its byte-order bit.
    pub magic: u32,
    /// Whether checksum words are read big-endian (magic low bit set).
    pub big_endian_checksums: bool,
    /// WAL format version.
    pub format_version: u32,
    /// Database page size in bytes.
    pub page_size: u32,
    /// Checkpoint sequence number.
    pub checkpoint_seq: u32,
    /// Salt-1. Incremented on every WAL restart.
    pub salt1: u32,
    /// Salt-2. Randomized on every WAL restart.
    pub salt2: u32,
    /// The header's checksum, which also seeds the frame checksum chain.
    pub checksum: (u32, u32),
}

impl WalHeader {
    /// Parse and fully validate a WAL header, including its own checksum.
    ///
    /// # Errors
    ///
    /// Returns [`WalError`] when the buffer is short, the magic/version/page
    /// size is not a `SQLite` WAL's, or the header checksum does not validate.
    pub fn parse(bytes: &[u8]) -> Result<Self, WalError> {
        let head = bytes
            .get(..WAL_HEADER_SIZE)
            .ok_or(WalError::TooShort { len: bytes.len() })?;

        let magic = be_u32(head, 0).ok_or(WalError::TooShort { len: bytes.len() })?;
        if magic & 0xFFFF_FFFE != WAL_MAGIC_BASE {
            return Err(WalError::BadMagic { magic });
        }
        let big_endian_checksums = magic & 1 == 1;

        let format_version = be_u32(head, 4).ok_or(WalError::TooShort { len: bytes.len() })?;
        if format_version != WAL_FORMAT_VERSION {
            return Err(WalError::UnsupportedVersion {
                version: format_version,
            });
        }

        let page_size = be_u32(head, 8).ok_or(WalError::TooShort { len: bytes.len() })?;
        if !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size) || !page_size.is_power_of_two() {
            return Err(WalError::PageSize { page_size });
        }

        let checkpoint_seq = be_u32(head, 12).ok_or(WalError::TooShort { len: bytes.len() })?;
        let salt1 = be_u32(head, 16).ok_or(WalError::TooShort { len: bytes.len() })?;
        let salt2 = be_u32(head, 20).ok_or(WalError::TooShort { len: bytes.len() })?;
        let stored1 = be_u32(head, 24).ok_or(WalError::TooShort { len: bytes.len() })?;
        let stored2 = be_u32(head, 28).ok_or(WalError::TooShort { len: bytes.len() })?;

        let covered = head
            .get(..24)
            .ok_or(WalError::TooShort { len: bytes.len() })?;
        let computed = checksum_bytes(big_endian_checksums, covered, (0, 0));
        if computed != (stored1, stored2) {
            return Err(WalError::HeaderChecksum);
        }

        Ok(Self {
            magic,
            big_endian_checksums,
            format_version,
            page_size,
            checkpoint_seq,
            salt1,
            salt2,
            checksum: (stored1, stored2),
        })
    }

    /// The salt pair that identifies this WAL generation.
    #[must_use]
    pub const fn salt(&self) -> (u32, u32) {
        (self.salt1, self.salt2)
    }

    /// Total on-disk size of one frame: the 24-byte frame header plus a page.
    #[must_use]
    pub const fn frame_size(&self) -> u64 {
        (self.page_size as u64).saturating_add(FRAME_HEADER_SIZE as u64)
    }
}

/// Where a scan resumes from: a byte offset into the `-wal` file plus the
/// rolling checksum state at that offset.
///
/// Carrying the checksum forward is what lets each replication tick read only
/// the bytes appended since the last one, instead of re-hashing the whole WAL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanCursor {
    /// Byte offset into the `-wal` file.
    pub offset: u64,
    /// Rolling frame checksum as of `offset`.
    pub checksum: (u32, u32),
    /// How many frames precede `offset`.
    pub frame_index: u64,
}

impl ScanCursor {
    /// The cursor at the first frame of a generation: just past the header,
    /// seeded with the header's checksum.
    #[must_use]
    pub const fn start(header: &WalHeader) -> Self {
        Self {
            offset: WAL_HEADER_SIZE as u64,
            checksum: header.checksum,
            frame_index: 0,
        }
    }
}

/// What a scan found between a [`ScanCursor`] and the end of the valid chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOutcome {
    /// Valid frames read past the cursor.
    pub frames: u64,
    /// Commit frames among them.
    pub commits: u64,
    /// Byte offset just past the **last commit frame** — the furthest point that
    /// may be shipped. Equals the cursor's offset when no commit was found.
    pub last_commit_end: u64,
    /// Database size in pages recorded by that last commit frame (`0` if none).
    pub last_commit_db_pages: u32,
    /// The cursor to resume from at `last_commit_end`.
    pub last_commit_cursor: ScanCursor,
    /// Byte offset just past the last *valid* frame, commit or not. Always
    /// `>= last_commit_end`; the difference is an in-flight transaction.
    pub valid_end: u64,
}

/// Walk the frame chain from `cursor`, validating `SQLite`'s checksums.
///
/// `bytes` must be the `-wal` contents **starting at `cursor.offset`**. The scan
/// stops at the first frame that is short, carries a foreign salt, or breaks the
/// checksum chain — exactly where `SQLite`'s own recovery would stop.
///
/// # Errors
///
/// Returns [`WalError::PageSize`] when the header's page size cannot be
/// represented on this platform.
pub fn scan_from(
    header: &WalHeader,
    cursor: &ScanCursor,
    bytes: &[u8],
) -> Result<ScanOutcome, WalError> {
    let frame_size = header.frame_size();
    let frame_len = usize::try_from(frame_size).map_err(|_| WalError::PageSize {
        page_size: header.page_size,
    })?;

    let mut running = cursor.checksum;
    let mut offset = cursor.offset;
    let mut frame_index = cursor.frame_index;
    let mut frames: u64 = 0;
    let mut commits: u64 = 0;
    let mut last_commit_end = cursor.offset;
    let mut last_commit_db_pages: u32 = 0;
    let mut last_commit_cursor = *cursor;
    let mut rest = bytes;

    while let Some(frame) = rest.get(..frame_len) {
        let (Some(pgno), Some(truncate), Some(salt1), Some(salt2), Some(c1), Some(c2)) = (
            be_u32(frame, 0),
            be_u32(frame, 4),
            be_u32(frame, 8),
            be_u32(frame, 12),
            be_u32(frame, 16),
            be_u32(frame, 20),
        ) else {
            break;
        };
        // A zero page number is never legal, and a frame carrying a different
        // salt belongs to a previous generation whose bytes were reused.
        if pgno == 0 || (salt1, salt2) != header.salt() {
            break;
        }
        let (Some(head8), Some(page)) = (frame.get(..8), frame.get(FRAME_HEADER_SIZE..)) else {
            break;
        };
        let stepped = checksum_bytes(header.big_endian_checksums, head8, running);
        let computed = checksum_bytes(header.big_endian_checksums, page, stepped);
        if computed != (c1, c2) {
            break;
        }

        running = computed;
        frames = frames.saturating_add(1);
        frame_index = frame_index.saturating_add(1);
        offset = offset.saturating_add(frame_size);
        rest = rest.get(frame_len..).unwrap_or(&[]);

        // A non-zero "database size after commit" marks the last frame of a
        // transaction. Everything up to here is durable and shippable.
        if truncate != 0 {
            commits = commits.saturating_add(1);
            last_commit_end = offset;
            last_commit_db_pages = truncate;
            last_commit_cursor = ScanCursor {
                offset,
                checksum: running,
                frame_index,
            };
        }
    }

    Ok(ScanOutcome {
        frames,
        commits,
        last_commit_end,
        last_commit_db_pages,
        last_commit_cursor,
        valid_end: offset,
    })
}

/// `SQLite`'s WAL checksum (`walChecksumBytes`): a pair of 32-bit accumulators
/// stepped over 8-byte blocks. `data.len()` must be a multiple of 8; a trailing
/// partial block is ignored, matching the callers' fixed-size inputs.
fn checksum_bytes(big_endian: bool, data: &[u8], seed: (u32, u32)) -> (u32, u32) {
    let (mut s1, mut s2) = seed;
    for block in data.as_chunks::<8>().0 {
        let Some((first, rest)) = block.split_first_chunk::<4>() else {
            continue;
        };
        let Some((second, _)) = rest.split_first_chunk::<4>() else {
            continue;
        };
        let (w0, w1) = if big_endian {
            (u32::from_be_bytes(*first), u32::from_be_bytes(*second))
        } else {
            (u32::from_le_bytes(*first), u32::from_le_bytes(*second))
        };
        s1 = s1.wrapping_add(w0).wrapping_add(s2);
        s2 = s2.wrapping_add(w1).wrapping_add(s1);
    }
    (s1, s2)
}

/// Read a big-endian `u32` at `at`, or `None` when the buffer is too short.
fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let slice = bytes.get(at..end)?;
    let arr = <[u8; 4]>::try_from(slice).ok()?;
    Some(u32::from_be_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_path_appends_rather_than_replacing_the_extension() {
        assert_eq!(
            wal_path(Path::new("/srv/app.db")),
            Path::new("/srv/app.db-wal")
        );
        assert_eq!(
            shm_path(Path::new("/srv/app.db")),
            Path::new("/srv/app.db-shm")
        );
    }

    #[test]
    fn parse_rejects_a_short_buffer() {
        assert_eq!(
            WalHeader::parse(&[]).unwrap_err(),
            WalError::TooShort { len: 0 }
        );
        assert_eq!(
            WalHeader::parse(&[0u8; 31]).unwrap_err(),
            WalError::TooShort { len: 31 }
        );
    }

    /// Build a WAL header with a valid self-checksum so the negative cases below
    /// isolate exactly one defect each.
    fn header_bytes(magic: u32, version: u32, page_size: u32) -> [u8; WAL_HEADER_SIZE] {
        let mut buf = [0u8; WAL_HEADER_SIZE];
        buf[0..4].copy_from_slice(&magic.to_be_bytes());
        buf[4..8].copy_from_slice(&version.to_be_bytes());
        buf[8..12].copy_from_slice(&page_size.to_be_bytes());
        buf[12..16].copy_from_slice(&7u32.to_be_bytes());
        buf[16..20].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        buf[20..24].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        let (c1, c2) = checksum_bytes(magic & 1 == 1, &buf[..24], (0, 0));
        buf[24..28].copy_from_slice(&c1.to_be_bytes());
        buf[28..32].copy_from_slice(&c2.to_be_bytes());
        buf
    }

    #[test]
    fn parse_accepts_both_checksum_byte_orders() {
        for magic in [0x377f_0682u32, 0x377f_0683] {
            let header = WalHeader::parse(&header_bytes(magic, WAL_FORMAT_VERSION, 4096))
                .expect("valid header");
            assert_eq!(header.big_endian_checksums, magic & 1 == 1);
            assert_eq!(header.page_size, 4096);
            assert_eq!(header.salt(), (0xDEAD_BEEF, 0x1234_5678));
            assert_eq!(header.frame_size(), 4096 + 24);
        }
    }

    #[test]
    fn parse_rejects_bad_magic_version_and_page_size() {
        assert!(matches!(
            WalHeader::parse(&header_bytes(0x0000_0001, WAL_FORMAT_VERSION, 4096)),
            Err(WalError::BadMagic { .. })
        ));
        assert!(matches!(
            WalHeader::parse(&header_bytes(0x377f_0682, 1, 4096)),
            Err(WalError::UnsupportedVersion { version: 1 })
        ));
        for bad in [0u32, 256, 4095, 131_072] {
            assert!(
                matches!(
                    WalHeader::parse(&header_bytes(0x377f_0682, WAL_FORMAT_VERSION, bad)),
                    Err(WalError::PageSize { .. })
                ),
                "page size {bad} must be rejected"
            );
        }
    }

    #[test]
    fn parse_rejects_a_tampered_header_checksum() {
        let mut buf = header_bytes(0x377f_0682, WAL_FORMAT_VERSION, 4096);
        buf[16] ^= 0xFF; // flip a salt byte without refreshing the checksum
        assert_eq!(
            WalHeader::parse(&buf).unwrap_err(),
            WalError::HeaderChecksum
        );
    }

    #[test]
    fn checksum_matches_sqlites_accumulator_definition() {
        // Hand-computed from `s1 += w0 + s2; s2 += w1 + s1` over one block.
        let data = [0u8, 0, 0, 1, 0, 0, 0, 2];
        assert_eq!(checksum_bytes(true, &data, (0, 0)), (1, 3));
        // Seeded, and wrapping rather than panicking on overflow.
        assert_eq!(
            checksum_bytes(true, &data, (u32::MAX, u32::MAX)),
            (u32::MAX, u32::MAX.wrapping_add(2).wrapping_add(u32::MAX))
        );
    }

    #[test]
    fn scan_of_an_empty_tail_reports_no_frames() {
        let header =
            WalHeader::parse(&header_bytes(0x377f_0682, WAL_FORMAT_VERSION, 4096)).expect("header");
        let cursor = ScanCursor::start(&header);
        let outcome = scan_from(&header, &cursor, &[]).expect("scan");
        assert_eq!(outcome.frames, 0);
        assert_eq!(outcome.commits, 0);
        assert_eq!(outcome.last_commit_end, WAL_HEADER_SIZE as u64);
        assert_eq!(outcome.valid_end, WAL_HEADER_SIZE as u64);
        assert_eq!(outcome.last_commit_cursor, cursor);
    }
}
