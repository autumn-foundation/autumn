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
use std::path::{Path, PathBuf};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::DataCapsule;
use super::model::{
    CapsuleError, CapsuleManifest, FORMAT, FORMAT_VERSION, Record, check_model_names, record_file,
};
use crate::security::config::{ResolvedSigningKeys, SigningSecretConfig};

const MANIFEST_FILE: &str = "manifest.json";
const SIGNATURE_FILE: &str = "signature.json";
const ALGORITHM: &str = "HMAC-SHA256";
/// Domain separation: a capsule signature is never a valid cookie or CSRF tag.
const SIGNATURE_DOMAIN: &[u8] = b"autumn-data-capsule/v1\n";
/// The deepest file path in a capsule (`viewer/<table>/index.html`).
const MAX_PATH_DEPTH: usize = 3;

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
    /// [`CapsuleError::MissingSigningSecret`] when no secret is set. A random
    /// key would make capsules that no other process can verify.
    pub fn from_config(config: &SigningSecretConfig) -> Result<Self, CapsuleError> {
        if config.secret.as_deref().is_none_or(str::is_empty) {
            return Err(CapsuleError::MissingSigningSecret);
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
    pub files_checked: usize,
    /// The number of records in the capsule.
    pub records: u64,
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn to_json<T: Serialize + ?Sized>(file: &str, value: &T) -> Result<Vec<u8>, CapsuleError> {
    serde_json::to_vec_pretty(value).map_err(|e| CapsuleError::Json {
        file: file.to_owned(),
        message: e.to_string(),
    })
}

fn from_json<T: serde::de::DeserializeOwned>(file: &str, bytes: &[u8]) -> Result<T, CapsuleError> {
    serde_json::from_slice(bytes).map_err(|e| CapsuleError::Json {
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

fn join(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(root.to_path_buf(), |p, s| p.join(s))
}

fn read_file(root: &Path, rel: &str) -> Result<Vec<u8>, CapsuleError> {
    let path = join(root, rel);
    std::fs::read(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CapsuleError::Integrity(format!("file is missing: {rel}"))
        } else {
            CapsuleError::io(path, e)
        }
    })
}

impl DataCapsule {
    /// Write the capsule to `dir` and sign it.
    ///
    /// `dir` must not exist, or must be an empty directory.
    ///
    /// # Errors
    ///
    /// [`CapsuleError::NotEmpty`] when `dir` has content, an I/O error, or an
    /// unsafe name in the manifest.
    pub fn write_dir(&self, dir: &Path, signer: &CapsuleSigner) -> Result<(), CapsuleError> {
        let created = prepare_empty_dir(dir)?;
        let result = self.write_files(dir, signer);
        if result.is_err() {
            // The directory was empty or new, so all content is from this
            // call. Remove it: a partial capsule is not useful.
            let _ = std::fs::remove_dir_all(dir);
            if !created {
                let _ = std::fs::create_dir(dir);
            }
        }
        result
    }

    fn write_files(&self, dir: &Path, signer: &CapsuleSigner) -> Result<(), CapsuleError> {
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
        for blob in &manifest.blobs {
            let bytes = self
                .blobs
                .get(&blob.sha256)
                .ok_or_else(|| CapsuleError::Blob(format!("no bytes for blob {:?}", blob.key)))?;
            files.push((blob.file(), bytes.to_vec()));
        }
        files.extend(super::viewer::render(&manifest, &self.records));

        manifest.files = files
            .iter()
            .map(|(path, bytes)| (path.clone(), sha256_hex(bytes)))
            .collect();
        for (rel, bytes) in &files {
            if !is_safe_rel_path(rel) {
                return Err(CapsuleError::InvalidName(rel.clone()));
            }
            let path = join(dir, rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| CapsuleError::io(parent, e))?;
            }
            std::fs::write(&path, bytes).map_err(|e| CapsuleError::io(path, e))?;
        }

        let manifest_bytes = to_json(MANIFEST_FILE, &manifest)?;
        let signature = SignatureFile {
            algorithm: ALGORITHM.to_owned(),
            signature: signer.sign(&manifest_bytes),
        };
        let manifest_path = dir.join(MANIFEST_FILE);
        std::fs::write(&manifest_path, &manifest_bytes)
            .map_err(|e| CapsuleError::io(manifest_path, e))?;
        let signature_path = dir.join(SIGNATURE_FILE);
        std::fs::write(&signature_path, to_json(SIGNATURE_FILE, &signature)?)
            .map_err(|e| CapsuleError::io(signature_path, e))
    }

    /// Verify the capsule in `dir`, then read it.
    ///
    /// # Errors
    ///
    /// The errors of [`verify_dir`], or a record file that is not valid.
    pub fn read_dir(dir: &Path, signer: &CapsuleSigner) -> Result<Self, CapsuleError> {
        let (manifest, mut contents) = load_verified(dir, signer)?;
        let mut records = BTreeMap::new();
        for model in &manifest.models {
            let bytes = contents.remove(&model.file).unwrap_or_default();
            let rows: Vec<Record> = from_json(&model.file, &bytes)?;
            if rows.len() as u64 != model.record_count {
                return Err(CapsuleError::Integrity(format!(
                    "{} has {} records, the manifest says {}",
                    model.file,
                    rows.len(),
                    model.record_count
                )));
            }
            records.insert(model.table.clone(), rows);
        }
        let blobs = manifest
            .blobs
            .iter()
            .map(|b| {
                let bytes = contents.remove(&b.file()).unwrap_or_default();
                (b.sha256.clone(), Bytes::from(bytes))
            })
            .collect();
        Ok(Self {
            manifest,
            records,
            blobs,
        })
    }
}

/// Make sure `dir` is an empty directory. Give `true` when this call made it.
fn prepare_empty_dir(dir: &Path) -> Result<bool, CapsuleError> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(CapsuleError::NotEmpty(dir.to_path_buf()));
            }
            Ok(false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(dir)
            .map(|()| true)
            .map_err(|e| CapsuleError::io(dir, e)),
        Err(e) => Err(CapsuleError::io(dir, e)),
    }
}

/// Verify the signature and every file hash of the capsule in `dir`.
///
/// The check fails when a file is changed, missing, or not in the manifest.
///
/// # Errors
///
/// [`CapsuleError::Integrity`] for a check that fails,
/// [`CapsuleError::UnsupportedFormat`] for an unknown format, or an I/O error.
pub fn verify_dir(dir: &Path, signer: &CapsuleSigner) -> Result<VerifyReport, CapsuleError> {
    let (manifest, _) = load_verified(dir, signer)?;
    Ok(VerifyReport {
        subject: manifest.subject.clone(),
        files_checked: manifest.files.len(),
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
) -> Result<(CapsuleManifest, BTreeMap<String, Vec<u8>>), CapsuleError> {
    let manifest_bytes = read_file(dir, MANIFEST_FILE)?;
    let signature: SignatureFile = from_json(SIGNATURE_FILE, &read_file(dir, SIGNATURE_FILE)?)?;
    if signature.algorithm != ALGORITHM {
        return Err(CapsuleError::UnsupportedFormat(format!(
            "signature algorithm {:?}",
            signature.algorithm
        )));
    }
    if !signer.verify(&manifest_bytes, &signature.signature) {
        return Err(CapsuleError::Integrity(
            "the manifest signature does not agree with a known key".to_owned(),
        ));
    }
    let manifest: CapsuleManifest = from_json(MANIFEST_FILE, &manifest_bytes)?;
    if manifest.format != FORMAT || manifest.format_version != FORMAT_VERSION {
        return Err(CapsuleError::UnsupportedFormat(format!(
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
    let mut contents = BTreeMap::new();
    for (rel, expected) in &manifest.files {
        if !is_safe_rel_path(rel) {
            return Err(CapsuleError::InvalidName(rel.clone()));
        }
        let bytes = read_file(dir, rel)?;
        if sha256_hex(&bytes) != *expected {
            return Err(CapsuleError::Integrity(format!("file is changed: {rel}")));
        }
        if wanted.contains(rel) {
            contents.insert(rel.clone(), bytes);
        }
    }
    check_no_extra_files(dir, &manifest)?;
    Ok((manifest, contents))
}

/// Each model and blob file must be in `files`, with the correct name.
fn check_manifest_refs(manifest: &CapsuleManifest) -> Result<(), CapsuleError> {
    for model in &manifest.models {
        check_model_names(
            &model.table,
            &model.primary_key,
            &model.subject_column,
            &model.relationships,
            &model.blob_columns,
        )?;
        if model.file != record_file(&model.table) || !manifest.files.contains_key(&model.file) {
            return Err(CapsuleError::Integrity(format!(
                "record file of {} is not in the manifest",
                model.table
            )));
        }
    }
    for blob in &manifest.blobs {
        // The file name is the hash, so the hash check covers the bytes.
        if manifest.files.get(&blob.file()) != Some(&blob.sha256) {
            return Err(CapsuleError::Integrity(format!(
                "blob {:?} does not agree with its file",
                blob.key
            )));
        }
    }
    Ok(())
}

fn check_no_extra_files(dir: &Path, manifest: &CapsuleManifest) -> Result<(), CapsuleError> {
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((path, prefix)) = stack.pop() {
        let entries = std::fs::read_dir(&path).map_err(|e| CapsuleError::io(&path, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| CapsuleError::io(&path, e))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let kind = entry
                .file_type()
                .map_err(|e| CapsuleError::io(entry.path(), e))?;
            if kind.is_dir() {
                stack.push((entry.path(), rel));
            } else if !kind.is_file() {
                return Err(CapsuleError::Integrity(format!(
                    "entry is not a regular file: {rel}"
                )));
            } else if rel != MANIFEST_FILE
                && rel != SIGNATURE_FILE
                && !manifest.files.contains_key(&rel)
            {
                return Err(CapsuleError::Integrity(format!(
                    "file is not in the manifest: {rel}"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Err(CapsuleError::MissingSigningSecret)
        ));
    }
}
