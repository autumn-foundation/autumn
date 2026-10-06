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
/// writes nothing. A key with the same bytes and MIME type is not written again.
///
/// The function writes with [`BlobStore::put_if_absent`]. If another writer
/// takes a key after the check, that is also a conflict. Then the function
/// deletes the blobs that it wrote and keeps the blob of the other writer. A
/// store without a conditional create gives
/// [`DataCapsuleError::NotConfigured`]. Call it before [`import_capsule`](super::import_capsule):
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
        if !check_existing(store, entry).await? {
            to_write.push((entry, bytes));
        }
    }
    let mut written = Vec::new();
    for (entry, bytes) in to_write {
        let result = match store
            .put_if_absent(&entry.key, &entry.content_type, bytes.clone())
            .await
        {
            Ok(Some(_)) => {
                written.push(entry);
                // A store can keep the bytes but lose the MIME type.
                match stored_content_type(store, entry).await {
                    Ok(content_type) if content_type == entry.content_type => Ok(()),
                    Ok(content_type) => Err(DataCapsuleError::Blob(format!(
                        "blob {:?} was stored with MIME type {content_type:?}, not {:?}",
                        entry.key, entry.content_type
                    ))),
                    Err(e) => Err(e),
                }
            }
            // Another writer took the key after the check. If the key is
            // free again, the blob is not there: that is a conflict too.
            Ok(None) => match check_existing(store, entry).await {
                Ok(true) => Ok(()),
                Ok(false) => Err(DataCapsuleError::Conflict(format!(
                    "blob {:?} changed during the import",
                    entry.key
                ))),
                Err(e) => Err(e),
            },
            Err(BlobStoreError::Unsupported(_)) => Err(DataCapsuleError::NotConfigured(
                "the blob store cannot create a blob without the risk of replacing one \
                 (BlobStore::put_if_absent)"
                    .to_owned(),
            )),
            Err(e) => Err(DataCapsuleError::Blob(format!("put {:?}: {e}", entry.key))),
        };
        if let Err(error) = result {
            roll_back(store, &written).await;
            return Err(error);
        }
    }
    Ok(capsule.manifest.blobs.len())
}

/// Delete the blobs that this import wrote.
///
/// Another writer can replace a blob after this import made it. So delete a
/// key only when it still holds the bytes of this import. The store has no
/// conditional delete, so a replacement between the read and the delete is
/// still possible, but the window is short.
async fn roll_back(store: &dyn BlobStore, written: &[&BlobEntry]) {
    for entry in written {
        let ours = store
            .get(&entry.key)
            .await
            .is_ok_and(|bytes| hex::encode(Sha256::digest(&bytes)) == entry.sha256);
        if !ours {
            tracing::warn!(blob_key = %entry.key, "capsule import: blob changed, not rolled back");
            continue;
        }
        if let Err(e) = store.delete(&entry.key).await {
            tracing::warn!(blob_key = %entry.key, error = %e, "capsule import: rollback failed");
        }
    }
}

/// Compare the blob at `entry.key` with `entry`.
///
/// Returns `false` when the key is free, and `true` when it holds the same
/// bytes and MIME type.
async fn check_existing(
    store: &dyn BlobStore,
    entry: &BlobEntry,
) -> Result<bool, DataCapsuleError> {
    let existing = match store.get(&entry.key).await {
        Ok(existing) => existing,
        Err(BlobStoreError::NotFound(_)) => return Ok(false),
        Err(e) => return Err(DataCapsuleError::Blob(format!("get {:?}: {e}", entry.key))),
    };
    if hex::encode(Sha256::digest(&existing)) != entry.sha256 {
        return Err(DataCapsuleError::Conflict(format!(
            "blob {:?} exists with different bytes",
            entry.key
        )));
    }
    let content_type = stored_content_type(store, entry).await?;
    if content_type != entry.content_type {
        return Err(DataCapsuleError::Conflict(format!(
            "blob {:?} exists with MIME type {content_type:?}, not {:?}",
            entry.key, entry.content_type
        )));
    }
    Ok(true)
}

/// The MIME type that `store` keeps for `entry.key`. Export writes
/// `application/octet-stream` when a blob has no metadata, so use the same
/// default.
async fn stored_content_type(
    store: &dyn BlobStore,
    entry: &BlobEntry,
) -> Result<String, DataCapsuleError> {
    match store.head(&entry.key).await {
        Ok(Some(meta)) => Ok(meta.content_type),
        Ok(None) | Err(BlobStoreError::NotFound(_)) => Ok("application/octet-stream".to_owned()),
        Err(e) => Err(DataCapsuleError::Blob(format!("head {:?}: {e}", entry.key))),
    }
}
