//! The capsule directory: write, sign, verify, and read.
//!
//! Layout:
//!
//! ```text
//! manifest.json          models, fields, relationships, blobs, file hashes
//! signature.json         HMAC-SHA256 of manifest.json
//! records/<table>.json   the records of each model
//! blobs/<sha256>         the blob bytes
//! viewer/                the offline HTML viewer
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::DataCapsule;
use super::model::{
    CapsuleManifest, DATA_CAPSULE_FORMAT, DATA_CAPSULE_FORMAT_VERSION, DataCapsuleError, Record,
    check_model_names, record_file,
};
use super::root::Root;
use crate::security::config::{ResolvedSigningKeys, SigningSecretConfig};

const MANIFEST_FILE: &str = "manifest.json";
const SIGNATURE_FILE: &str = "signature.json";
/// The largest manifest that verify reads. It is read before its signature
/// is checked, so a capsule from anyone must not make it allocate more.
const MAX_MANIFEST_BYTES: u64 = 64 << 20;
/// The largest signature file: a few hundred bytes in practice.
const MAX_SIGNATURE_BYTES: u64 = 64 << 10;
const ALGORITHM: &str = "HMAC-SHA256";
/// Domain separation: a capsule signature is never a valid cookie or CSRF tag.
const SIGNATURE_DOMAIN: &[u8] = b"autumn-data-capsule/v1\n";
/// The deepest file path in a capsule (`viewer/<table>/index.html`).
pub(super) const MAX_PATH_DEPTH: usize = 3;

/// Signs and verifies capsules with the app signing secret.
///
/// Verification also accepts `previous_secrets`, so a key rotation does not
/// make old capsules unusable.
#[derive(Clone)]
pub struct CapsuleSigner {
    keys: ResolvedSigningKeys,
}

impl std::fmt::Debug for CapsuleSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapsuleSigner").finish_non_exhaustive()
    }
}

impl CapsuleSigner {
    /// Make a signer from one secret.
    #[must_use]
    pub fn new(secret: impl AsRef<[u8]>) -> Self {
        Self {
            keys: ResolvedSigningKeys::new(secret.as_ref().to_vec(), Vec::new()),
        }
    }

    /// Make a signer from `[security.signing_secret]`.
    ///
    /// # Errors
    ///
    /// [`DataCapsuleError::MissingSigningSecret`] when no secret is set. A random
    /// key would make capsules that no other process can verify.
    pub fn from_config(config: &SigningSecretConfig) -> Result<Self, DataCapsuleError> {
        if config.secret.as_deref().is_none_or(str::is_empty) {
            return Err(DataCapsuleError::MissingSigningSecret);
        }
        Ok(Self {
            keys: crate::security::config::resolve_signing_keys(config),
        })
    }

    /// The hex signature of `message`.
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> String {
        self.keys.sign(&domain(message))
    }

    /// `true` when `signature` agrees with `message` under a known key.
    #[must_use]
    pub fn verify(&self, message: &[u8], signature: &str) -> bool {
        self.keys.verify(&domain(message), signature)
    }
}

fn domain(message: &[u8]) -> Vec<u8> {
    [SIGNATURE_DOMAIN, message].concat()
}

#[derive(Debug, Serialize, Deserialize)]
struct SignatureFile {
    algorithm: String,
    signature: String,
}

/// The result of a successful [`verify_dir`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct VerifyReport {
    /// The subject id of the capsule.
    pub subject: String,
    /// The number of files whose hash agrees.
    pub files_checked: u64,
    /// The number of records in the capsule.
    pub records: u64,
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn to_json<T: Serialize + ?Sized>(file: &str, value: &T) -> Result<Vec<u8>, DataCapsuleError> {
    serde_json::to_vec_pretty(value).map_err(|e| DataCapsuleError::Json {
        file: file.to_owned(),
        message: e.to_string(),
    })
}

fn from_json<T: serde::de::DeserializeOwned>(
    file: &str,
    bytes: &[u8],
) -> Result<T, DataCapsuleError> {
    serde_json::from_slice(bytes).map_err(|e| DataCapsuleError::Json {
        file: file.to_owned(),
        message: e.to_string(),
    })
}

/// `true` for one plain path segment: ASCII letters, digits, `_`, `-`, `.`,
/// and not first a `.`.
pub(super) fn is_safe_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 255
        && !segment.starts_with('.')
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// `true` for a relative path of plain segments.
fn is_safe_rel_path(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    segments.len() <= MAX_PATH_DEPTH && segments.iter().all(|s| is_safe_segment(s))
}

impl DataCapsule {
    /// Write the capsule to `dir` and sign it.
    ///
    /// `dir` must not exist, or must be an empty directory.
    ///
    /// # Errors
    ///
    /// [`DataCapsuleError::NotEmpty`] when `dir` has content, an I/O error, or an
    /// unsafe name in the manifest.
    pub fn write_dir(&self, dir: &Path, signer: &CapsuleSigner) -> Result<(), DataCapsuleError> {
        let created = prepare_empty_dir(dir)?;
        // Write through a handle: the path can change after the check.
        let root = match Root::open(dir) {
            Ok(root) => root,
            Err(error) => {
                if created {
                    let _ = std::fs::remove_dir(dir);
                }
                return Err(error);
            }
        };
        let mut written = Vec::new();
        let result = root
            .prepare()
            .map_err(|e| match e {
                DataCapsuleError::NotEmpty(_) => DataCapsuleError::NotEmpty(dir.to_path_buf()),
                other => other,
            })
            .and_then(|()| self.write_files(&root, signer, &mut written));
        if result.is_err() {
            // A partial capsule is not useful. Remove only what this call
            // wrote: another export can write into the same directory.
            root.remove_written(&written);
            if created {
                // `remove_dir` removes only an empty directory.
                let _ = std::fs::remove_dir(dir);
            }
        }
        result
    }

    fn write_files(
        &self,
        root: &Root,
        signer: &CapsuleSigner,
        written: &mut Vec<String>,
    ) -> Result<(), DataCapsuleError> {
        let mut manifest = self.manifest.clone();
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for model in &mut manifest.models {
            check_model_names(
                &model.table,
                &model.primary_key,
                &model.subject_column,
                &model.relationships,
                &model.blob_columns,
            )?;
            let records = self.records(&model.table);
            model.record_count = records.len() as u64;
            model.file = record_file(&model.table);
            files.push((model.file.clone(), to_json(&model.file, records)?));
        }
        // Two keys with the same bytes share one file: write it one time.
        let mut seen = std::collections::BTreeSet::new();
        for blob in &manifest.blobs {
            let bytes = self.blobs.get(&blob.sha256).ok_or_else(|| {
                DataCapsuleError::Blob(format!("no bytes for blob {:?}", blob.key))
            })?;
            if seen.insert(blob.sha256.as_str()) {
                files.push((blob.file(), bytes.to_vec()));
            }
        }
        files.extend(super::viewer::render(&manifest, &self.records));

        manifest.files = files
            .iter()
            .map(|(path, bytes)| (path.clone(), sha256_hex(bytes)))
            .collect();
        for (rel, bytes) in &files {
            if !is_safe_rel_path(rel) {
                return Err(DataCapsuleError::InvalidName(rel.clone()));
            }
            root.write(rel, bytes, written)?;
        }

        let manifest_bytes = to_json(MANIFEST_FILE, &manifest)?;
        // Verify would refuse a larger manifest: never write one.
        if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(DataCapsuleError::InvalidInput(format!(
                "the manifest has {} bytes, more than the {MAX_MANIFEST_BYTES} that verify reads",
                manifest_bytes.len()
            )));
        }
        let signature = SignatureFile {
            algorithm: ALGORITHM.to_owned(),
            signature: signer.sign(&manifest_bytes),
        };
        root.write(MANIFEST_FILE, &manifest_bytes, written)?;
        root.write(
            SIGNATURE_FILE,
            &to_json(SIGNATURE_FILE, &signature)?,
            written,
        )?;
        Ok(())
    }

    /// Verify the capsule in `dir`, then read it.
    ///
    /// # Errors
    ///
    /// The errors of [`verify_dir`], or a record file that is not valid.
    pub fn read_dir(dir: &Path, signer: &CapsuleSigner) -> Result<Self, DataCapsuleError> {
        let (manifest, mut contents) = load_verified(dir, signer)?;
        let mut records = BTreeMap::new();
        for model in &manifest.models {
            let bytes = contents.remove(&model.file).ok_or_else(|| {
                DataCapsuleError::Integrity(format!("no record file for {}", model.table))
            })?;
            let rows: Vec<Record> = from_json(&model.file, &bytes)?;
            if rows.len() as u64 != model.record_count {
                return Err(DataCapsuleError::Integrity(format!(
                    "{} has {} records, the manifest says {}",
                    model.file,
                    rows.len(),
                    model.record_count
                )));
            }
            records.insert(model.table.clone(), rows);
        }
        // Two keys can share one file, so take the bytes by hash, one time.
        let mut blobs = BTreeMap::new();
        for blob in &manifest.blobs {
            if !blobs.contains_key(&blob.sha256) {
                let bytes = contents.remove(&blob.file()).ok_or_else(|| {
                    DataCapsuleError::Integrity(format!("no file for blob {:?}", blob.key))
                })?;
                blobs.insert(blob.sha256.clone(), Bytes::from(bytes));
            }
            let bytes = &blobs[&blob.sha256];
            if bytes.len() as u64 != blob.byte_size || sha256_hex(bytes) != blob.sha256 {
                return Err(DataCapsuleError::Integrity(format!(
                    "blob {:?} does not agree with its file",
                    blob.key
                )));
            }
        }
        Ok(Self {
            manifest,
            records,
            blobs,
        })
    }
}

/// Make sure `dir` is an empty, owner-only directory. Give `true` when this
/// call made it.
///
/// A link is refused: the capsule must not go to a place that the path does
/// not name.
fn prepare_empty_dir(dir: &Path) -> Result<bool, DataCapsuleError> {
    let created = match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(DataCapsuleError::InvalidName(format!(
                "{} is a link",
                dir.display()
            )));
        }
        Ok(_) => {
            let mut entries = std::fs::read_dir(dir).map_err(|e| DataCapsuleError::io(dir, e))?;
            if entries.next().is_some() {
                return Err(DataCapsuleError::NotEmpty(dir.to_path_buf()));
            }
            false
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(DataCapsuleError::io(dir, e)),
    };
    crate::fs_atomic::ensure_owner_only_dir(dir).map_err(|e| DataCapsuleError::io(dir, e))?;
    Ok(created)
}

/// Verify the signature and every file hash of the capsule in `dir`.
///
/// The check fails when a file is changed, missing, or not in the manifest.
///
/// # Errors
///
/// [`DataCapsuleError::Integrity`] for a check that fails,
/// [`DataCapsuleError::UnsupportedFormat`] for an unknown format, or an I/O error.
pub fn verify_dir(dir: &Path, signer: &CapsuleSigner) -> Result<VerifyReport, DataCapsuleError> {
    let (manifest, _) = load_verified(dir, signer)?;
    Ok(VerifyReport {
        subject: manifest.subject.clone(),
        files_checked: manifest.files.len() as u64,
        records: manifest.models.iter().map(|m| m.record_count).sum(),
    })
}

/// Verify `dir`. Give the manifest and the bytes of the record and blob files.
///
/// The function reads each file one time, so the bytes it gives are the bytes
/// it verified.
fn load_verified(
    dir: &Path,
    signer: &CapsuleSigner,
) -> Result<(CapsuleManifest, BTreeMap<String, Vec<u8>>), DataCapsuleError> {
    // Open the directory one time, without following a link. List the files
    // before any read: a link or a device file fails here, so no read
    // follows a link to `/dev/zero` or to a file outside.
    let root = Root::open(dir)?;
    let present = root.list_regular_files()?;
    for required in [MANIFEST_FILE, SIGNATURE_FILE] {
        if !present.contains(required) {
            return Err(DataCapsuleError::Integrity(format!(
                "file is missing: {required}"
            )));
        }
    }
    let manifest_bytes = root.read_at_most(MANIFEST_FILE, MAX_MANIFEST_BYTES)?;
    let signature: SignatureFile = from_json(
        SIGNATURE_FILE,
        &root.read_at_most(SIGNATURE_FILE, MAX_SIGNATURE_BYTES)?,
    )?;
    if signature.algorithm != ALGORITHM {
        return Err(DataCapsuleError::UnsupportedFormat(format!(
            "signature algorithm {:?}",
            signature.algorithm
        )));
    }
    if !signer.verify(&manifest_bytes, &signature.signature) {
        return Err(DataCapsuleError::Integrity(
            "the manifest signature does not agree with a known key".to_owned(),
        ));
    }
    let manifest: CapsuleManifest = from_json(MANIFEST_FILE, &manifest_bytes)?;
    if manifest.format != DATA_CAPSULE_FORMAT
        || manifest.format_version != DATA_CAPSULE_FORMAT_VERSION
    {
        return Err(DataCapsuleError::UnsupportedFormat(format!(
            "{} version {}",
            manifest.format, manifest.format_version
        )));
    }
    check_manifest_refs(&manifest)?;

    let wanted: BTreeSet<String> = manifest
        .models
        .iter()
        .map(|m| m.file.clone())
        .chain(manifest.blobs.iter().map(super::model::BlobEntry::file))
        .collect();
    for rel in manifest.files.keys() {
        if !is_safe_rel_path(rel) {
            return Err(DataCapsuleError::InvalidName(rel.clone()));
        }
        if !present.contains(rel) {
            return Err(DataCapsuleError::Integrity(format!(
                "file is missing: {rel}"
            )));
        }
    }
    if let Some(extra) = present.iter().find(|rel| {
        *rel != MANIFEST_FILE && *rel != SIGNATURE_FILE && !manifest.files.contains_key(*rel)
    }) {
        return Err(DataCapsuleError::Integrity(format!(
            "file is not in the manifest: {extra}"
        )));
    }
    let mut contents = BTreeMap::new();
    for (rel, expected) in &manifest.files {
        let bytes = root.read(rel)?;
        if sha256_hex(&bytes) != *expected {
            return Err(DataCapsuleError::Integrity(format!(
                "file is changed: {rel}"
            )));
        }
        if wanted.contains(rel) {
            contents.insert(rel.clone(), bytes);
        }
    }
    Ok((manifest, contents))
}

/// Each model and blob file must be in `files`, with the correct name.
fn check_manifest_refs(manifest: &CapsuleManifest) -> Result<(), DataCapsuleError> {
    for model in &manifest.models {
        check_model_names(
            &model.table,
            &model.primary_key,
            &model.subject_column,
            &model.relationships,
            &model.blob_columns,
        )?;
        if model.file != record_file(&model.table) || !manifest.files.contains_key(&model.file) {
            return Err(DataCapsuleError::Integrity(format!(
                "record file of {} is not in the manifest",
                model.table
            )));
        }
    }
    for blob in &manifest.blobs {
        // The file name is the hash, so the hash check covers the bytes.
        if manifest.files.get(&blob.file()) != Some(&blob.sha256) {
            return Err(DataCapsuleError::Integrity(format!(
                "blob {:?} does not agree with its file",
                blob.key
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_never_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        // Another export wrote the file first.
        std::fs::write(dir.path().join("manifest.json"), b"theirs").unwrap();
        let mut written = Vec::new();
        assert!(root.write("manifest.json", b"ours", &mut written).is_err());
        assert!(written.is_empty(), "{written:?}");
        assert_eq!(
            std::fs::read(dir.path().join("manifest.json")).unwrap(),
            b"theirs"
        );
    }

    #[test]
    fn listing_refuses_a_tree_deeper_than_a_capsule() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("f.json"), b"[]").unwrap();
        let err = Root::open(dir.path())
            .unwrap()
            .list_regular_files()
            .expect_err("too deep");
        assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
    }

    #[cfg(unix)]
    mod no_follow {
        use super::*;

        fn capsule_dir() -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("records")).unwrap();
            std::fs::write(dir.path().join("records/a.json"), b"[]").unwrap();
            dir
        }

        #[test]
        fn open_refuses_a_linked_capsule_dir() {
            let dir = capsule_dir();
            let link = dir.path().with_extension("link");
            std::os::unix::fs::symlink(dir.path(), &link).unwrap();
            let err = Root::open(&link).expect_err("a link must fail");
            assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
            let _ = std::fs::remove_file(link);
        }

        #[test]
        fn read_goes_through_the_open_handle() {
            let dir = capsule_dir();
            let root = Root::open(dir.path()).unwrap();
            // Swap the directory after open: reads still see the original.
            let moved = dir.path().with_extension("moved");
            std::fs::rename(dir.path(), &moved).unwrap();
            let decoy = tempfile::tempdir().unwrap();
            std::fs::create_dir(decoy.path().join("records")).unwrap();
            std::fs::write(decoy.path().join("records/a.json"), b"[1]").unwrap();
            std::os::unix::fs::symlink(decoy.path(), dir.path()).unwrap();
            assert_eq!(root.read("records/a.json").unwrap(), b"[]");
            let listed = root.list_regular_files().unwrap();
            assert_eq!(listed.into_iter().collect::<Vec<_>>(), ["records/a.json"]);
            std::fs::remove_file(dir.path()).unwrap();
            std::fs::rename(&moved, dir.path()).unwrap();
        }

        #[test]
        fn write_goes_through_the_open_handle() {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().unwrap();
            let root = Root::open(dir.path()).unwrap();
            // Swap the directory after open: writes still go to the original.
            let moved = dir.path().with_extension("moved");
            std::fs::rename(dir.path(), &moved).unwrap();
            let decoy = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(decoy.path(), dir.path()).unwrap();
            root.write("records/a.json", b"[]", &mut Vec::new())
                .unwrap();
            assert_eq!(std::fs::read(moved.join("records/a.json")).unwrap(), b"[]");
            assert!(std::fs::read_dir(decoy.path()).unwrap().next().is_none());
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&moved.join("records")), 0o700);
            assert_eq!(mode(&moved.join("records/a.json")), 0o600);
            std::fs::remove_file(dir.path()).unwrap();
            std::fs::rename(&moved, dir.path()).unwrap();
        }

        #[test]
        fn a_file_is_tracked_before_a_step_that_can_fail() {
            let dir = tempfile::tempdir().unwrap();
            let root = Root::open(dir.path()).unwrap();
            let mut written = Vec::new();
            // The fill after the create fails (for example, the disk is full).
            drop(
                root.create("viewer/users/index.html", &mut written)
                    .unwrap(),
            );
            assert_eq!(
                written,
                ["viewer/", "viewer/users/", "viewer/users/index.html"]
            );

            root.remove_written(&written);
            assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        }

        #[test]
        fn cleanup_removes_only_the_files_of_this_call() {
            let dir = tempfile::tempdir().unwrap();
            let root = Root::open(dir.path()).unwrap();
            let mut written = Vec::new();
            root.write("records/a.json", b"[]", &mut written).unwrap();
            root.write("viewer/users/index.html", b"<p>", &mut written)
                .unwrap();
            // Another export wrote into the same directory.
            std::fs::write(dir.path().join("records/b.json"), b"[1]").unwrap();

            root.remove_written(&written);
            assert!(!dir.path().join("records/a.json").exists());
            assert_eq!(
                std::fs::read(dir.path().join("records/b.json")).unwrap(),
                b"[1]"
            );
            // Empty directories go; a directory with a file of another call stays.
            assert!(!dir.path().join("viewer").exists());
            assert!(dir.path().join("records").is_dir());
        }

        #[test]
        fn write_refuses_a_linked_dir_or_an_existing_file() {
            let dir = tempfile::tempdir().unwrap();
            let decoy = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(decoy.path(), dir.path().join("records")).unwrap();
            let root = Root::open(dir.path()).unwrap();
            let err = root
                .write("records/a.json", b"[]", &mut Vec::new())
                .expect_err("linked dir");
            assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
            assert!(std::fs::read_dir(decoy.path()).unwrap().next().is_none());

            // A file is never replaced, and a link at the file name is not followed.
            std::fs::write(decoy.path().join("target"), b"keep").unwrap();
            std::os::unix::fs::symlink(decoy.path().join("target"), dir.path().join("m.json"))
                .unwrap();
            let mut written = Vec::new();
            assert!(root.write("m.json", b"{}", &mut written).is_err());
            // A file of another writer is not ours to remove.
            assert!(written.is_empty(), "{written:?}");
            assert_eq!(std::fs::read(decoy.path().join("target")).unwrap(), b"keep");
        }

        #[test]
        fn read_refuses_a_linked_file_or_dir() {
            let dir = capsule_dir();
            std::os::unix::fs::symlink(
                dir.path().join("records/a.json"),
                dir.path().join("records/b.json"),
            )
            .unwrap();
            std::os::unix::fs::symlink(dir.path().join("records"), dir.path().join("viewer"))
                .unwrap();
            let root = Root::open(dir.path()).unwrap();
            for rel in ["records/b.json", "viewer/a.json"] {
                let err = root.read(rel).expect_err("a link must fail");
                assert!(
                    matches!(err, DataCapsuleError::Integrity(_)),
                    "{rel}: {err:?}"
                );
            }
            let err = root.list_regular_files().expect_err("links in the tree");
            assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
        }

        #[test]
        fn read_of_a_missing_file_is_an_integrity_error() {
            let dir = capsule_dir();
            let root = Root::open(dir.path()).unwrap();
            let err = root.read("records/none.json").expect_err("missing");
            assert!(matches!(err, DataCapsuleError::Integrity(_)), "{err:?}");
        }
    }

    #[test]
    fn safe_rel_path_accepts_capsule_paths() {
        for p in [
            "records/users.json",
            "blobs/ab12",
            "viewer/posts/index.html",
        ] {
            assert!(is_safe_rel_path(p), "{p}");
        }
    }

    #[test]
    fn safe_rel_path_rejects_escapes() {
        for p in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            ".hidden",
            "a\\b",
            "a/b/c/d",
            "a b",
        ] {
            assert!(!is_safe_rel_path(p), "{p}");
        }
    }

    #[test]
    fn signature_is_domain_separated() {
        let keys = ResolvedSigningKeys::new(b"k".to_vec(), Vec::new());
        let signer = CapsuleSigner::new(b"k");
        assert_ne!(signer.sign(b"m"), keys.sign(b"m"));
    }

    #[test]
    fn debug_does_not_print_the_key() {
        let text = format!("{:?}", CapsuleSigner::new(b"very-secret"));
        assert!(!text.contains("118"), "{text}");
        assert!(!text.contains("very-secret"), "{text}");
    }

    #[test]
    fn empty_secret_is_missing() {
        let config = SigningSecretConfig {
            secret: Some(String::new()),
            previous_secrets: Vec::new(),
        };
        assert!(matches!(
            CapsuleSigner::from_config(&config),
            Err(DataCapsuleError::MissingSigningSecret)
        ));
    }
}
