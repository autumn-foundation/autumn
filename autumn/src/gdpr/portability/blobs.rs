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
        let Some((bytes, sha256, content_type)) = read_one_version(store, &key).await? else {
            tracing::warn!(blob_key = %key, "capsule export: blob is missing, skipped");
            continue;
        };
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

/// The bytes, their hex SHA-256 and their MIME type, all of one version of
/// `key`, or `None` when `key` is not in `store`.
///
/// `get` and `head` are separate calls, and another writer can replace the
/// blob between them. So read `head`, `get`, and `head` again. The read holds
/// one version when both heads are the same, and when an etag that is a
/// SHA-256 (as on `LocalBlobStore`) is the SHA-256 of the bytes. An S3 etag
/// is an MD5 or a multipart tag: it shows a change between the two heads,
/// not a mismatch with the bytes. When the read sees two versions, read
/// again once, then give up. A store without etags shows only a change of
/// the MIME type.
pub(super) async fn read_one_version(
    store: &dyn BlobStore,
    key: &str,
) -> Result<Option<(bytes::Bytes, String, String)>, DataCapsuleError> {
    for _ in 0..2 {
        let before = head(store, key).await?;
        let bytes = match store.get(key).await {
            Ok(bytes) => bytes,
            // Gone since the first head: read again.
            Err(BlobStoreError::NotFound(_)) if before.is_some() => continue,
            Err(BlobStoreError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(DataCapsuleError::Blob(format!("get {key:?}: {e}"))),
        };
        let after = head(store, key).await?;
        let sha256 = hex::encode(Sha256::digest(&bytes));
        let same = match (&before, &after) {
            (Some(a), Some(b)) => a.etag == b.etag && a.content_type == b.content_type,
            (None, None) => true,
            _ => false,
        };
        let etag = after.as_ref().and_then(|m| m.etag.as_deref());
        let matches =
            !etag.is_some_and(|etag| is_sha256(etag) && !etag.eq_ignore_ascii_case(&sha256));
        if !(same && matches) {
            continue;
        }
        let content_type =
            after.map_or_else(|| "application/octet-stream".to_owned(), |m| m.content_type);
        return Ok(Some((bytes, sha256, content_type)));
    }
    Err(DataCapsuleError::Conflict(format!(
        "blob {key:?} changed while it was read"
    )))
}

/// The metadata of `key`. Export writes `application/octet-stream` for a
/// blob without metadata, so `None` here gets that default too.
async fn head(
    store: &dyn BlobStore,
    key: &str,
) -> Result<Option<crate::storage::BlobMeta>, DataCapsuleError> {
    match store.head(key).await {
        Ok(meta) => Ok(meta),
        Err(BlobStoreError::NotFound(_)) => Ok(None),
        Err(e) => Err(DataCapsuleError::Blob(format!("head {key:?}: {e}"))),
    }
}

/// `true` for 64 hex digits.
fn is_sha256(etag: &str) -> bool {
    etag.len() == 64 && etag.bytes().all(|b| b.is_ascii_hexdigit())
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
/// [`DataCapsuleError::NotConfigured`]. Call it, then [`rebind_blobs`], before
/// [`import_capsule`](super::import_capsule): then no imported record points at
/// a blob that is not there, or at another store.
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
    restore_and_track(capsule, store).await?;
    Ok(capsule.manifest.blobs.len())
}

/// Point each `storage::Blob` object in the blob columns of `capsule` at
/// `store`: its `provider_id` and `etag` become those of `store`. Call it
/// after [`restore_blobs`] and before [`import_capsule`](super::import_capsule).
///
/// A plain key string, and a key that the capsule has no bytes for, stay as
/// they are.
///
/// # Errors
///
/// [`DataCapsuleError::Conflict`] when a restored blob is gone, or
/// [`DataCapsuleError::Blob`] when `store` fails.
pub async fn rebind_blobs(
    capsule: &mut DataCapsule,
    store: &dyn BlobStore,
) -> Result<(), DataCapsuleError> {
    let restored: BTreeSet<&str> = capsule
        .manifest
        .blobs
        .iter()
        .map(|b| b.key.as_str())
        .collect();
    let mut etags = std::collections::BTreeMap::new();
    for model in &capsule.manifest.models {
        for row in capsule.records.get_mut(&model.table).into_iter().flatten() {
            for column in &model.blob_columns {
                let Some(serde_json::Value::Object(blob)) = row.get_mut(column) else {
                    continue;
                };
                let Some(key) = blob.get("key").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                if !blob.contains_key("provider_id") || !restored.contains(key) {
                    continue;
                }
                let key = key.to_owned();
                if !etags.contains_key(&key) {
                    let etag = match store.head(&key).await {
                        Ok(Some(meta)) => meta.etag,
                        Ok(None) | Err(BlobStoreError::NotFound(_)) => {
                            return Err(DataCapsuleError::Conflict(format!(
                                "blob {key:?} is gone after the restore"
                            )));
                        }
                        Err(e) => {
                            return Err(DataCapsuleError::Blob(format!("head {key:?}: {e}")));
                        }
                    };
                    etags.insert(key.clone(), etag);
                }
                blob.insert("provider_id".to_owned(), store.provider_id().into());
                blob.insert("etag".to_owned(), etags[&key].clone().into());
            }
        }
    }
    Ok(())
}

/// Restore the blobs of `capsule` and give the entries that this call wrote.
/// [`roll_back`] removes them again, for example when the record import
/// fails after this.
pub(super) async fn restore_and_track<'c>(
    capsule: &'c DataCapsule,
    store: &dyn BlobStore,
) -> Result<Vec<&'c BlobEntry>, DataCapsuleError> {
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
                verify_created(store, entry).await
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
    Ok(written)
}

/// Read back a blob that this import created. Another writer can replace it
/// right after the create, and a store can keep the bytes but lose the MIME
/// type.
async fn verify_created(store: &dyn BlobStore, entry: &BlobEntry) -> Result<(), DataCapsuleError> {
    let content_type = match read_one_version(store, &entry.key).await? {
        Some((_, sha256, content_type)) if sha256 == entry.sha256 => content_type,
        Some(_) | None => {
            return Err(DataCapsuleError::Conflict(format!(
                "blob {:?} changed during the import",
                entry.key
            )));
        }
    };
    if content_type == entry.content_type {
        Ok(())
    } else {
        Err(DataCapsuleError::Blob(format!(
            "blob {:?} was stored with MIME type {content_type:?}, not {:?}",
            entry.key, entry.content_type
        )))
    }
}

/// Delete the blobs that this import wrote.
///
/// Another writer can replace a blob after this import made it. So delete a
/// key only when it still holds the bytes of this import. The store has no
/// conditional delete, so a replacement between the read and the delete is
/// still possible, but the window is short.
pub(super) async fn roll_back(store: &dyn BlobStore, written: &[&BlobEntry]) {
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
    // The bytes and the MIME type must come from one version: a writer that
    // replaces the blob between two reads must not pass as this blob.
    let Some((_, sha256, content_type)) = read_one_version(store, &entry.key).await? else {
        return Ok(false);
    };
    if sha256 != entry.sha256 {
        return Err(DataCapsuleError::Conflict(format!(
            "blob {:?} exists with different bytes",
            entry.key
        )));
    }
    if content_type != entry.content_type {
        return Err(DataCapsuleError::Conflict(format!(
            "blob {:?} exists with MIME type {content_type:?}, not {:?}",
            entry.key, entry.content_type
        )));
    }
    Ok(true)
}
