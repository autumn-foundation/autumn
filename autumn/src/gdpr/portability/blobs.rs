//! Blob bytes in a capsule (feature `storage`).

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

use super::DataCapsule;
use super::model::{BlobEntry, DataCapsuleError};
use super::viewer::blob_key;
use crate::storage::{BlobStore, BlobStoreError};

/// Copy the blobs that the records reference from `store` into `capsule`.
///
/// The blob columns come from the capsule manifest. Export skips a key that is
/// not in `store` and logs a warning. One missing file must not stop an export.
///
/// # Errors
///
/// [`DataCapsuleError::Blob`] when `store` fails for a reason other than "not
/// found".
pub async fn collect_blobs(
    capsule: &mut DataCapsule,
    store: &dyn BlobStore,
) -> Result<(), DataCapsuleError> {
    let mut keys = BTreeSet::new();
    for model in &capsule.manifest.models {
        for row in capsule.records(&model.table) {
            for column in &model.blob_columns {
                if let Some(key) = row.get(column).and_then(blob_key) {
                    keys.insert(key.to_owned());
                }
            }
        }
    }
    for key in keys {
        if capsule.manifest.blob(&key).is_some() {
            continue;
        }
        let bytes = match store.get(&key).await {
            Ok(bytes) => bytes,
            Err(BlobStoreError::NotFound(_)) => {
                tracing::warn!(blob_key = %key, "capsule export: blob is missing, skipped");
                continue;
            }
            Err(e) => return Err(DataCapsuleError::Blob(format!("get {key:?}: {e}"))),
        };
        let content_type = match store.head(&key).await {
            Ok(Some(meta)) => meta.content_type,
            Ok(None) | Err(BlobStoreError::NotFound(_)) => "application/octet-stream".to_owned(),
            Err(e) => return Err(DataCapsuleError::Blob(format!("head {key:?}: {e}"))),
        };
        let sha256 = hex::encode(Sha256::digest(&bytes));
        capsule.manifest.blobs.push(BlobEntry {
            key,
            sha256: sha256.clone(),
            content_type,
            byte_size: bytes.len() as u64,
        });
        capsule.blobs.insert(sha256, bytes);
    }
    Ok(())
}

/// Write the blobs of `capsule` to `store`, each under its original key.
///
/// The function first checks all keys. A key that holds different bytes, or the
/// same bytes with a different MIME type, is a conflict, and then the function
/// writes nothing. A key with the same bytes and MIME type is not written again. Call it before [`import_capsule`](super::import_capsule):
/// then no imported record points at a blob that is not there.
///
/// # Errors
///
/// [`DataCapsuleError::Conflict`] for a key with different bytes or MIME type, or
/// [`DataCapsuleError::Blob`] when the capsule has no bytes for an entry or
/// `store` fails.
pub async fn restore_blobs(
    capsule: &DataCapsule,
    store: &dyn BlobStore,
) -> Result<usize, DataCapsuleError> {
    let mut to_write = Vec::new();
    for entry in &capsule.manifest.blobs {
        let bytes = capsule
            .blobs
            .get(&entry.sha256)
            .ok_or_else(|| DataCapsuleError::Blob(format!("no bytes for blob {:?}", entry.key)))?;
        match store.get(&entry.key).await {
            Ok(existing) if hex::encode(Sha256::digest(&existing)) == entry.sha256 => {
                // Export writes `application/octet-stream` when a blob has no
                // metadata, so compare with the same default.
                let content_type = match store.head(&entry.key).await {
                    Ok(Some(meta)) => meta.content_type,
                    Ok(None) | Err(BlobStoreError::NotFound(_)) => {
                        "application/octet-stream".to_owned()
                    }
                    Err(e) => {
                        return Err(DataCapsuleError::Blob(format!("head {:?}: {e}", entry.key)));
                    }
                };
                if content_type != entry.content_type {
                    return Err(DataCapsuleError::Conflict(format!(
                        "blob {:?} exists with MIME type {content_type:?}, not {:?}",
                        entry.key, entry.content_type
                    )));
                }
            }
            Ok(_) => {
                return Err(DataCapsuleError::Conflict(format!(
                    "blob {:?} exists with different bytes",
                    entry.key
                )));
            }
            Err(BlobStoreError::NotFound(_)) => to_write.push((entry, bytes)),
            Err(e) => return Err(DataCapsuleError::Blob(format!("get {:?}: {e}", entry.key))),
        }
    }
    for (entry, bytes) in to_write {
        store
            .put(&entry.key, &entry.content_type, bytes.clone())
            .await
            .map_err(|e| DataCapsuleError::Blob(format!("put {:?}: {e}", entry.key)))?;
    }
    Ok(capsule.manifest.blobs.len())
}
