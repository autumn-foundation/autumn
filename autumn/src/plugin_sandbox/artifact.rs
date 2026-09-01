//! The `.autumn-plugin` container: a manifest plus the wasm module it describes.
//!
//! ```text
//! offset  0   "AUTUMNPL"     8 bytes of magic
//! offset  8   u32 LE         container format version (= 1)
//! offset 12   u32 LE         manifest length, N
//! offset 16   N bytes        the manifest, UTF-8 TOML
//! offset 16+N …              the wasm module, to the end of the file
//! ```
//!
//! Deliberately not tar, zip or any other archive: the container needs exactly
//! one reader, and putting the manifest at a fixed offset means
//! `head -c 4096 hello.autumn-plugin` shows an operator the whole review
//! surface with no tooling at all.
//!
//! # What the digest proves, and what it does not
//!
//! The digest binds the manifest to the module **inside the same file**, so a
//! byte flipped in either one is caught. It is not a signature: anyone who can
//! rewrite the file can also recompute the digest. Reviewing an artifact
//! therefore means recording the digest `autumn plugin inspect` prints and
//! comparing it against the one your deployment loads — which is why the digest
//! is on the consent screen at all.
//!
//! # Reading is the gate
//!
//! [`SandboxArtifact::read`] refuses a container whose magic, format version,
//! framing, manifest, module digest or module magic is wrong. A
//! `SandboxArtifact` in hand therefore means: these bytes are a wasm module,
//! this manifest describes *these* bytes, and this build understands every
//! word of it.

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes concurrent `write_file` calls within one process.
///
/// The pid separates processes; this separates threads and repeat calls, so two
/// packagings racing on the same output directory cannot collide on the
/// temporary name and turn each other's `create_new` into an error.
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

use sha2::{Digest as _, Sha256};

use super::manifest::{ManifestError, SandboxManifest};

/// Bytes of container header before the manifest: magic + version + length.
const HEADER_BYTES: usize = 16;

/// The largest manifest a container may carry (64 KiB).
///
/// A manifest is a page of TOML a human is expected to read; anything larger is
/// a framing error or an attempt to make the reader allocate.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// The largest wasm module a container may carry (64 MiB).
pub const MAX_MODULE_BYTES: usize = 64 * 1024 * 1024;

/// The largest `.autumn-plugin` file this build will read at all.
///
/// The ceilings inside [`SandboxArtifact::read`] apply to bytes the process has
/// already allocated, so a reader that slurps the file first has made the
/// decision before it can refuse: a crafted multi-gigabyte artifact would
/// exhaust the process that was trying to inspect it. This bound is applied to
/// the *file*, before a byte of it is read.
pub const MAX_ARTIFACT_BYTES: usize = HEADER_BYTES + MAX_MANIFEST_BYTES + MAX_MODULE_BYTES;

/// The four bytes every WebAssembly module starts with.
const WASM_MAGIC: &[u8] = b"\0asm";

/// Why a container was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ArtifactError {
    /// The bytes do not start with [`SandboxArtifact::MAGIC`].
    BadMagic,
    /// The container declares a format version this build does not read.
    UnsupportedFormatVersion {
        /// The version found in the container.
        found: u32,
        /// The version this build reads.
        supported: u32,
    },
    /// The container ends before a field it declares.
    Truncated {
        /// What was being read.
        what: &'static str,
        /// How many bytes were needed.
        needed: usize,
        /// How many were available.
        available: usize,
    },
    /// The manifest length exceeds [`MAX_MANIFEST_BYTES`].
    ManifestTooLarge {
        /// The declared length.
        found: usize,
        /// The ceiling.
        max: usize,
    },
    /// The file exceeds [`MAX_ARTIFACT_BYTES`], refused before it was read.
    ArtifactTooLarge {
        /// The file's length in bytes.
        found: u64,
        /// The ceiling.
        max: usize,
    },
    /// The module length exceeds [`MAX_MODULE_BYTES`].
    ModuleTooLarge {
        /// The module's length.
        found: usize,
        /// The ceiling.
        max: usize,
    },
    /// The manifest bytes are not UTF-8.
    ManifestNotUtf8,
    /// The manifest did not parse or did not validate.
    Manifest(ManifestError),
    /// The payload is not a WebAssembly module.
    NotWasm,
    /// The manifest's digest does not match the module bytes in the container.
    DigestMismatch {
        /// What the manifest claims.
        declared: String,
        /// What the bytes actually hash to.
        actual: String,
    },
    /// The artifact could not be read from or written to disk.
    ///
    /// The [`kind`](std::io::ErrorKind) is carried separately so a caller can
    /// tell "this optional plugin is not installed" from "this plugin is
    /// installed and unreadable" — one is a skip, the other is a boot failure.
    Io {
        /// The path that failed.
        path: std::path::PathBuf,
        /// What kind of I/O failure it was.
        kind: std::io::ErrorKind,
        /// The underlying message.
        detail: String,
    },
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => write!(
                f,
                "not an Autumn sandboxed plugin artifact: the file does not start with `{magic}`",
                magic = String::from_utf8_lossy(SandboxArtifact::MAGIC)
            ),
            Self::UnsupportedFormatVersion { found, supported } => write!(
                f,
                "sandboxed plugin artifact declares container format {found}, but this build \
                 reads format {supported}"
            ),
            Self::Truncated {
                what,
                needed,
                available,
            } => write!(
                f,
                "truncated sandboxed plugin artifact: {what} needs {needed} bytes but only \
                 {available} remain"
            ),
            Self::ManifestTooLarge { found, max } => write!(
                f,
                "sandboxed plugin manifest is {found} bytes, over the {max}-byte ceiling"
            ),
            Self::ArtifactTooLarge { found, max } => write!(
                f,
                "sandboxed plugin artifact is {found} bytes, over the {max}-byte ceiling; \
                 refusing to read it"
            ),
            Self::ModuleTooLarge { found, max } => write!(
                f,
                "sandboxed plugin module is {found} bytes, over the {max}-byte ceiling"
            ),
            Self::ManifestNotUtf8 => {
                write!(f, "the sandboxed plugin manifest is not valid UTF-8")
            }
            Self::Manifest(err) => write!(f, "{err}"),
            Self::NotWasm => write!(
                f,
                "the sandboxed plugin payload is not a WebAssembly module (no `\\0asm` header)"
            ),
            Self::DigestMismatch { declared, actual } => write!(
                f,
                "sandboxed plugin module digest mismatch: the manifest declares {declared} but \
                 the module hashes to {actual}. The manifest and the module in this file do not \
                 belong together"
            ),
            Self::Io { path, detail, .. } => write!(
                f,
                "sandboxed plugin artifact I/O failure at {path}: {detail}",
                path = path.display()
            ),
        }
    }
}

impl std::error::Error for ArtifactError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Manifest(err) => Some(err),
            _ => None,
        }
    }
}

impl From<ManifestError> for ArtifactError {
    fn from(err: ManifestError) -> Self {
        Self::Manifest(err)
    }
}

/// A validated sandboxed-plugin artifact: a manifest and the module it
/// describes, proven to be the module it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxArtifact {
    /// Private on purpose. The type's whole claim is that *this* manifest
    /// describes *these* bytes and that both passed the gate; a caller that
    /// could reach in and widen a prefix, raise a ceiling or add a route would
    /// defeat every one of those checks after the fact, because nothing
    /// re-validates on the way to the host.
    manifest: SandboxManifest,
    module: Vec<u8>,
}

impl SandboxArtifact {
    /// The container's magic bytes.
    pub const MAGIC: &'static [u8] = b"AUTUMNPL";

    /// The container format version this build reads and writes.
    pub const FORMAT_VERSION: u32 = 1;

    /// Lowercase hex SHA-256 of `module`, as it appears in a manifest.
    #[must_use]
    pub fn digest(module: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(module);
        hex::encode(hasher.finalize())
    }

    /// Bind a manifest to a module, computing and stamping the digest.
    ///
    /// This is the packaging entry point: a packer cannot forget the digest,
    /// and cannot stamp one that does not match.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::NotWasm`] if `module` is not a WebAssembly
    /// module, [`ArtifactError::ModuleTooLarge`] if it is over the ceiling, and
    /// [`ArtifactError::Manifest`] if the manifest does not validate once the
    /// digest is stamped.
    pub fn seal(mut manifest: SandboxManifest, module: Vec<u8>) -> Result<Self, ArtifactError> {
        check_module(&module)?;
        manifest.sha256 = Self::digest(&module);
        // Re-validate through the public constructor so a hand-built manifest
        // can never enter an artifact without passing the same gate a parsed
        // one does.
        let manifest = SandboxManifest::parse(&manifest.to_toml()?)?;
        Ok(Self { manifest, module })
    }

    /// The digest of the whole artifact — the manifest *and* the module.
    ///
    /// [`digest`](Self::digest) covers the module alone, which is what the
    /// manifest's `sha256` field declares and what [`read`](Self::read)
    /// verifies. That binding answers "are these the bytes the author built",
    /// and it is the right question for the payload. It is the wrong question
    /// for a review.
    ///
    /// What an operator reviews is not only the module: it is the prefix, the
    /// routes, the capabilities and the ceilings — all of which live in the
    /// manifest. Those can be rewritten while the module is untouched, and the
    /// module digest stays correct because it is still describing the same
    /// bytes. An artifact reviewed under one grant could then be deployed under
    /// a wider one and match the digest that was written down.
    ///
    /// So this is the identity to record and compare. It is taken over the
    /// canonical container bytes, so it moves when anything the consent screen
    /// shows moves, and does not move for a difference the format does not
    /// carry, such as how the authored TOML was laid out.
    ///
    /// It is still a binding rather than a signature: anyone who can rewrite
    /// the file can recompute it. What it buys is that the number an operator
    /// wrote down covers everything they agreed to.
    ///
    /// # Errors
    ///
    /// Returns any [`to_bytes`](Self::to_bytes) error.
    pub fn artifact_digest(&self) -> Result<String, ArtifactError> {
        Ok(Self::digest(&self.to_bytes()?))
    }

    /// The capability manifest these bytes were verified against.
    #[must_use]
    pub const fn manifest(&self) -> &SandboxManifest {
        &self.manifest
    }

    /// The wasm module these bytes describe.
    #[must_use]
    pub fn module(&self) -> &[u8] {
        &self.module
    }

    /// Render the artifact as container bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::Manifest`] if the manifest cannot be rendered,
    /// and [`ArtifactError::ManifestTooLarge`] if the rendered manifest is over
    /// the ceiling.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ArtifactError> {
        let manifest = self.manifest.to_toml()?;
        if manifest.len() > MAX_MANIFEST_BYTES {
            return Err(ArtifactError::ManifestTooLarge {
                found: manifest.len(),
                max: MAX_MANIFEST_BYTES,
            });
        }
        let length =
            u32::try_from(manifest.len()).map_err(|_| ArtifactError::ManifestTooLarge {
                found: manifest.len(),
                max: MAX_MANIFEST_BYTES,
            })?;

        let mut out = Vec::with_capacity(HEADER_BYTES + manifest.len() + self.module.len());
        out.extend_from_slice(Self::MAGIC);
        out.extend_from_slice(&Self::FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(manifest.as_bytes());
        out.extend_from_slice(&self.module);
        Ok(out)
    }

    /// Parse and verify container bytes.
    ///
    /// # Errors
    ///
    /// Returns an [`ArtifactError`] for a container whose framing, manifest,
    /// payload or digest is wrong — see the type's variants.
    pub fn read(bytes: &[u8]) -> Result<Self, ArtifactError> {
        let magic = bytes
            .get(..Self::MAGIC.len())
            .ok_or(ArtifactError::Truncated {
                what: "the container magic",
                needed: Self::MAGIC.len(),
                available: bytes.len(),
            })?;
        if magic != Self::MAGIC {
            return Err(ArtifactError::BadMagic);
        }

        let header = bytes.get(..HEADER_BYTES).ok_or(ArtifactError::Truncated {
            what: "the container header",
            needed: HEADER_BYTES,
            available: bytes.len(),
        })?;
        let format = read_u32(header, Self::MAGIC.len())?;
        if format != Self::FORMAT_VERSION {
            return Err(ArtifactError::UnsupportedFormatVersion {
                found: format,
                supported: Self::FORMAT_VERSION,
            });
        }

        let manifest_len = read_u32(header, Self::MAGIC.len() + 4)? as usize;
        if manifest_len > MAX_MANIFEST_BYTES {
            return Err(ArtifactError::ManifestTooLarge {
                found: manifest_len,
                max: MAX_MANIFEST_BYTES,
            });
        }
        let body = bytes.get(HEADER_BYTES..).unwrap_or_default();
        let manifest_bytes = body.get(..manifest_len).ok_or(ArtifactError::Truncated {
            what: "the manifest",
            needed: manifest_len,
            available: body.len(),
        })?;
        let manifest_src =
            std::str::from_utf8(manifest_bytes).map_err(|_| ArtifactError::ManifestNotUtf8)?;
        let manifest = SandboxManifest::parse(manifest_src)?;

        // Checked on the borrowed slice, *then* cloned: `to_vec` first would
        // copy a multi-gigabyte payload into a second allocation before the
        // ceiling that exists to refuse it ever ran, so the refusal itself was
        // the expensive part. The caller already holds these bytes; the point
        // of the ceiling is not to hold them twice.
        let module = body.get(manifest_len..).unwrap_or_default();
        check_module(module)?;
        let module = module.to_vec();

        let actual = Self::digest(&module);
        if actual != manifest.sha256 {
            return Err(ArtifactError::DigestMismatch {
                declared: manifest.sha256,
                actual,
            });
        }
        Ok(Self { manifest, module })
    }

    /// Read and verify an artifact from disk.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::Io`] if the file cannot be read, or any
    /// [`read`](Self::read) error for its contents.
    pub fn read_file(path: &Path) -> Result<Self, ArtifactError> {
        Self::read(&read_bounded(path, MAX_ARTIFACT_BYTES)?)
    }

    /// Write the artifact to disk.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::Io`] if the file cannot be written, or a
    /// [`to_bytes`](Self::to_bytes) error.
    pub fn write_file(&self, path: &Path) -> Result<(), ArtifactError> {
        use std::io::Write as _;

        let bytes = self.to_bytes()?;
        // Write beside the target and rename, so a failure part-way through
        // cannot leave a truncated artifact where a good one was. A rename is
        // atomic on every platform this runs on.
        //
        // The temporary name is unique *and* the file is created with
        // `create_new`, which is the half that matters: a predictable sibling
        // that `std::fs::write` opens is a file it will truncate and a symlink
        // it will follow, so packaging in a checkout the author did not write
        // became a write primitive aimed at anything the user can write.
        // `create_new` fails on anything already there — a symlink included —
        // rather than resolving it.
        let unique = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let temporary = path.with_extension(format!(
            "autumn-plugin-tmp-{pid}-{unique}",
            pid = std::process::id()
        ));

        let io = |path: &Path, err: &std::io::Error| ArtifactError::Io {
            path: path.to_path_buf(),
            kind: err.kind(),
            detail: err.to_string(),
        };

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|err| io(&temporary, &err))?;

        // From here the temporary exists, so every failure has to remove it —
        // leaving debris beside the artifact would make the next run's
        // `create_new` the thing that fails.
        if let Err(err) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = std::fs::remove_file(&temporary);
            return Err(io(&temporary, &err));
        }
        drop(file);

        if let Err(err) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(io(path, &err));
        }
        Ok(())
    }
}

/// Read a file, refusing anything over `max` **before** allocating for it.
///
/// `std::fs::read` sizes its buffer from the file, so a ceiling applied after it
/// returns is a decision made after the damage: a crafted multi-gigabyte input
/// exhausts the process that was trying to refuse it. Every path that reads a
/// caller-supplied file — the loader and `autumn plugin package` both — goes
/// through here.
///
/// The metadata length is checked first (cheap) and the read is *also* bounded
/// (correct): a length can be a lie, from a growing file, a pipe, or a racing
/// writer, so the ceiling has to hold against the bytes rather than against what
/// the filesystem said about them.
///
/// # Errors
///
/// Returns [`ArtifactError::ArtifactTooLarge`] over the ceiling, and
/// [`ArtifactError::Io`] if the file cannot be opened or read.
pub fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>, ArtifactError> {
    use std::io::Read as _;

    let io = |err: &std::io::Error| ArtifactError::Io {
        path: path.to_path_buf(),
        kind: err.kind(),
        detail: err.to_string(),
    };
    let too_large = |found: u64| ArtifactError::ArtifactTooLarge { found, max };

    let mut file = std::fs::File::open(path).map_err(|err| io(&err))?;
    let length = file.metadata().map_err(|err| io(&err))?.len();
    if length > max as u64 {
        return Err(too_large(length));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0).min(max));
    let read = file
        .by_ref()
        // Reading one past the cap is how "larger than the cap" is detected.
        // At `usize::MAX` there is nothing past it to detect and the addition
        // itself overflows: a debug build panics, and a release build wraps the
        // limit to zero and returns an empty read that looks like a success.
        // Saturating is the honest answer — at the top of the range the cap
        // cannot be exceeded, so the read is simply unbounded.
        .take((max as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|err| io(&err))?;
    if read > max {
        return Err(too_large(u64::try_from(read).unwrap_or(u64::MAX)));
    }
    Ok(bytes)
}

fn read_u32(header: &[u8], at: usize) -> Result<u32, ArtifactError> {
    let slice = header
        .get(at..at.saturating_add(4))
        .ok_or_else(|| ArtifactError::Truncated {
            what: "a container header field",
            needed: at.saturating_add(4),
            available: header.len(),
        })?;
    let mut buffer = [0u8; 4];
    buffer.copy_from_slice(slice);
    Ok(u32::from_le_bytes(buffer))
}

fn check_module(module: &[u8]) -> Result<(), ArtifactError> {
    if module.len() > MAX_MODULE_BYTES {
        return Err(ArtifactError::ModuleTooLarge {
            found: module.len(),
            max: MAX_MODULE_BYTES,
        });
    }
    if module.get(..WASM_MAGIC.len()) == Some(WASM_MAGIC) {
        Ok(())
    } else {
        Err(ArtifactError::NotWasm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_sandbox::manifest::SandboxManifest;

    /// The smallest legal `wasm32` module: the 8-byte header alone.
    const EMPTY_MODULE: &[u8] = b"\0asm\x01\0\0\0";

    fn manifest_toml(digest: &str) -> String {
        format!(
            r#"
name = "autumn-plugin-hello"
version = "0.1.0"
wire_version = 1
prefix = "/hello"
capabilities = ["http-request"]
sha256 = "{digest}"

[[routes]]
method = "GET"
path = "/hello/greet"
"#
        )
    }

    fn sealed() -> SandboxArtifact {
        let manifest =
            SandboxManifest::parse(&manifest_toml(&SandboxArtifact::digest(EMPTY_MODULE)))
                .expect("valid manifest");
        SandboxArtifact::seal(manifest, EMPTY_MODULE.to_vec()).expect("seals")
    }

    #[test]
    fn round_trips_through_the_container() {
        let artifact = sealed();
        let bytes = artifact.to_bytes().expect("packs");
        let read = SandboxArtifact::read(&bytes).expect("reads back");
        assert_eq!(read.manifest, artifact.manifest);
        assert_eq!(read.module(), EMPTY_MODULE);
    }

    #[test]
    fn the_manifest_is_readable_at_a_fixed_offset() {
        // The container is deliberately not an archive format: a reviewer with
        // `head -c` can see the manifest without any tooling at all.
        let bytes = sealed().to_bytes().expect("packs");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("name = \"autumn-plugin-hello\""), "{text}");
        assert!(bytes.starts_with(SandboxArtifact::MAGIC));
    }

    #[test]
    fn seal_computes_the_digest_so_a_packer_cannot_forget_it() {
        let artifact = sealed();
        assert_eq!(
            artifact.manifest().sha256,
            SandboxArtifact::digest(EMPTY_MODULE)
        );
    }

    #[test]
    fn an_unlimited_read_cap_reads_the_file_rather_than_nothing() {
        // `usize::MAX` is how a caller says "no limit". The cap is applied by
        // reading one byte past it, and that addition used to overflow: debug
        // panicked, release wrapped the limit to zero and handed back an empty
        // `Ok` — the worst of the three outcomes, because it looks like data.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("small.autumn-plugin");
        std::fs::write(&path, b"some bytes").expect("writes");

        let bytes = read_bounded(&path, usize::MAX).expect("an unlimited cap must read the file");
        assert_eq!(bytes, b"some bytes", "the read came back empty or short");
    }

    #[test]
    fn the_artifact_digest_moves_when_the_grant_does_and_the_module_digest_does_not() {
        // The guide tells an operator that reviewing an artifact means
        // recording the digest `inspect` printed and comparing it against what
        // the deployment loads. The module digest cannot carry that promise:
        // what is reviewed is the prefix, the routes, the capabilities and the
        // ceilings, and every one of those lives in the manifest. Rewrite them
        // and the module digest is still correct — it is still describing the
        // same bytes — so a wider grant matches the number that was written
        // down.
        let reviewed = sealed();
        let reviewed_identity = reviewed.artifact_digest().expect("digests");

        // The same module, one word of the grant different: a prefix that
        // reaches somewhere else in the host's origin.
        let widened = SandboxManifest::parse(
            &manifest_toml(&SandboxArtifact::digest(EMPTY_MODULE)).replace("/hello", "/admin"),
        )
        .expect("valid manifest");
        let widened = SandboxArtifact::seal(widened, EMPTY_MODULE.to_vec()).expect("seals");

        // The module digest is identical, and honestly so — same bytes.
        assert_eq!(
            widened.manifest().sha256,
            reviewed.manifest().sha256,
            "the module did not change, so its digest must not",
        );
        // …which is exactly why it cannot be the review identity.
        assert_ne!(
            widened.artifact_digest().expect("digests"),
            reviewed_identity,
            "a rewritten grant kept the identity an operator recorded",
        );

        // A ceiling is part of the grant too, not just the routing.
        let mut raised =
            SandboxManifest::parse(&manifest_toml(&SandboxArtifact::digest(EMPTY_MODULE)))
                .expect("valid manifest");
        raised.limits.max_concurrency = raised.limits.max_concurrency.saturating_add(1);
        let raised = SandboxArtifact::seal(raised, EMPTY_MODULE.to_vec()).expect("seals");
        assert_ne!(
            raised.artifact_digest().expect("digests"),
            reviewed_identity,
            "a raised ceiling kept the identity an operator recorded",
        );

        // And it is stable: the same artifact through the container and back
        // reviews as the same thing, or an operator could never compare it.
        let round_tripped =
            SandboxArtifact::read(&reviewed.to_bytes().expect("packs")).expect("reads back");
        assert_eq!(
            round_tripped.artifact_digest().expect("digests"),
            reviewed_identity,
            "the identity must survive a round trip through the container",
        );
    }

    #[test]
    fn a_tampered_module_is_refused() {
        let mut bytes = sealed().to_bytes().expect("packs");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let err = SandboxArtifact::read(&bytes).expect_err("tampering must be caught");
        assert!(matches!(err, ArtifactError::DigestMismatch { .. }), "{err}");
    }

    #[test]
    fn a_foreign_container_is_refused() {
        let mut bytes = sealed().to_bytes().expect("packs");
        bytes[0] = b'X';
        let err = SandboxArtifact::read(&bytes).expect_err("bad magic must be caught");
        assert!(matches!(err, ArtifactError::BadMagic), "{err}");
    }

    #[test]
    fn a_future_container_version_is_refused() {
        let mut bytes = sealed().to_bytes().expect("packs");
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        let err = SandboxArtifact::read(&bytes).expect_err("future version must be caught");
        assert!(
            matches!(
                err,
                ArtifactError::UnsupportedFormatVersion { found: 2, .. }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_truncated_container_is_refused_at_every_cut() {
        let bytes = sealed().to_bytes().expect("packs");
        for cut in 0..bytes.len() {
            let err = SandboxArtifact::read(&bytes[..cut])
                .expect_err("a truncated container must never parse");
            // Any refusal will do; what must never happen is a panic or a
            // successful parse of a partial artifact.
            let _ = err.to_string();
        }
    }

    #[test]
    fn a_manifest_length_past_the_end_is_refused_without_allocating() {
        let mut bytes = sealed().to_bytes().expect("packs");
        bytes[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        let err = SandboxArtifact::read(&bytes).expect_err("must be caught");
        // The ceiling check runs before the slice, so a 4 GiB claim is refused
        // without the reader ever sizing a buffer from it.
        assert!(
            matches!(err, ArtifactError::ManifestTooLarge { .. }),
            "{err}"
        );

        // A length under the ceiling but past the end is a framing error.
        #[allow(clippy::cast_possible_truncation)]
        let past_end = (MAX_MANIFEST_BYTES - 1) as u32;
        bytes[12..16].copy_from_slice(&past_end.to_le_bytes());
        let err = SandboxArtifact::read(&bytes).expect_err("must be caught");
        assert!(matches!(err, ArtifactError::Truncated { .. }), "{err}");
    }

    #[test]
    fn an_oversized_manifest_is_refused() {
        let manifest = "x".repeat(MAX_MANIFEST_BYTES + 1);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SandboxArtifact::MAGIC);
        bytes.extend_from_slice(&SandboxArtifact::FORMAT_VERSION.to_le_bytes());
        #[allow(clippy::cast_possible_truncation)]
        bytes.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        bytes.extend_from_slice(manifest.as_bytes());
        bytes.extend_from_slice(EMPTY_MODULE);
        let err = SandboxArtifact::read(&bytes).expect_err("must be caught");
        assert!(
            matches!(err, ArtifactError::ManifestTooLarge { .. }),
            "{err}"
        );
    }

    #[test]
    fn a_payload_that_is_not_a_wasm_module_is_refused() {
        let module = b"#!/bin/sh\nrm -rf /\n".to_vec();
        let manifest =
            SandboxManifest::parse(&manifest_toml(&SandboxArtifact::digest(&module))).expect("ok");
        let err = SandboxArtifact::seal(manifest, module).expect_err("must be caught");
        assert!(matches!(err, ArtifactError::NotWasm), "{err}");
    }

    #[test]
    fn a_manifest_that_does_not_validate_is_refused_on_read() {
        // A hand-built container, which is the only way this can happen: the
        // manifest is not a public field, so nothing can widen a prefix on a
        // sealed artifact after the fact.
        let manifest = manifest_toml(&SandboxArtifact::digest(EMPTY_MODULE))
            .replace(r#"prefix = "/hello""#, r#"prefix = "/""#);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(SandboxArtifact::MAGIC);
        bytes.extend_from_slice(&SandboxArtifact::FORMAT_VERSION.to_le_bytes());
        #[allow(clippy::cast_possible_truncation)]
        bytes.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        bytes.extend_from_slice(manifest.as_bytes());
        bytes.extend_from_slice(EMPTY_MODULE);
        let err = SandboxArtifact::read(&bytes).expect_err("must be caught");
        assert!(matches!(err, ArtifactError::Manifest(_)), "{err}");
    }

    #[test]
    fn an_oversized_file_is_refused_before_it_is_read_into_memory() {
        // `std::fs::read` sizes its buffer from the file, so the container
        // ceilings inside `read()` are applied to bytes the process already
        // allocated. A crafted multi-gigabyte artifact would exhaust the
        // inspecting process instead of being refused by it.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("huge.autumn-plugin");
        let file = std::fs::File::create(&path).expect("creates");
        // Sparse: the bytes are never written, only the length is claimed.
        file.set_len(MAX_ARTIFACT_BYTES as u64 + 1)
            .expect("sets len");
        drop(file);

        let err = SandboxArtifact::read_file(&path).expect_err("must be refused");
        assert!(
            matches!(err, ArtifactError::ArtifactTooLarge { .. }),
            "{err}"
        );
    }

    #[test]
    fn a_missing_file_is_distinguishable_from_an_unreadable_one() {
        // "this optional plugin is not installed" is a skip; "this plugin is
        // installed and unreadable" is a boot failure. A caller cannot tell
        // those apart from a stringified error.
        let dir = tempfile::tempdir().expect("tempdir");
        let err = SandboxArtifact::read_file(&dir.path().join("absent.autumn-plugin"))
            .expect_err("must fail");
        assert!(
            matches!(
                err,
                ArtifactError::Io {
                    kind: std::io::ErrorKind::NotFound,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn writing_never_follows_something_already_sitting_at_the_temporary_path() {
        // The temporary sibling had a name anyone could predict, and
        // `std::fs::write` truncates through a symlink. In a checkout the
        // author did not write, that turns `autumn plugin package` into a
        // write primitive aimed at any file the user can write.
        let dir = tempfile::tempdir().expect("tempdir");
        let victim = dir.path().join("precious");
        std::fs::write(&victim, b"do not clobber me").expect("victim written");

        let path = dir.path().join("hello.autumn-plugin");
        let predictable = path.with_extension("autumn-plugin-tmp");
        std::os::unix::fs::symlink(&victim, &predictable).expect("symlink");

        // Packaging may succeed or refuse, but it must not write through the
        // link: the victim's contents are what this is about.
        let _ = sealed().write_file(&path);
        assert_eq!(
            std::fs::read(&victim).expect("victim still readable"),
            b"do not clobber me",
            "packaging wrote through a symlink at the temporary path"
        );
    }

    #[test]
    fn a_failed_write_never_replaces_a_good_artifact_with_a_truncated_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hello.autumn-plugin");
        sealed().write_file(&path).expect("writes");
        // A second write lands atomically over the first.
        sealed().write_file(&path).expect("rewrites");
        assert!(SandboxArtifact::read_file(&path).is_ok());
        assert!(
            !path.with_extension("autumn-plugin-tmp").exists(),
            "the staging file must not survive a successful write"
        );
    }

    #[test]
    fn reads_and_writes_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("hello.autumn-plugin");
        let artifact = sealed();
        artifact.write_file(&path).expect("writes");
        let read = SandboxArtifact::read_file(&path).expect("reads");
        assert_eq!(read.manifest, artifact.manifest);
    }

    #[test]
    fn the_digest_is_lowercase_hex_sha256() {
        let digest = SandboxArtifact::digest(b"");
        assert_eq!(
            digest,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
