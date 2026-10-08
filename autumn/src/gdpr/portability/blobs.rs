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
/// [`DataCapsuleError::Conflict`] when a `Blob` object names another store
/// than `store`, or no store (`provider_id`), or when a blob changes while it
/// is read.
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
                // A `Blob` names its store. After a switch of backend, a row
                // can hold a handle of the old store, and the same key in
                // `store` can hold other bytes: never export those.
                // Only a plain key string names no store.
                if let Some(blob) = row.get(column).and_then(serde_json::Value::as_object) {
                    let provider = blob.get("provider_id").and_then(serde_json::Value::as_str);
                    if provider != Some(store.provider_id()) {
                        return Err(DataCapsuleError::Conflict(format!(
                            "{}.{column} holds a blob of store {}, but the blob store is {:?}",
                            model.table,
                            provider.map_or_else(|| "(none)".to_owned(), |p| format!("{p:?}")),
                            store.provider_id()
                        )));
                    }
                }
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
/// the MIME type or the size.
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
            (Some(a), Some(b)) => {
                a.etag == b.etag && a.content_type == b.content_type && a.byte_size == b.byte_size
            }
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
/// takes a key after the check, that is also a conflict. A failed restore
/// deletes no blob, not even one that it wrote: another import can find the
/// same bytes under that key, take the blob as its own, and commit records
/// that point at it. A retry finds those bytes and reuses them. A store
/// without a conditional create gives
/// [`DataCapsuleError::NotConfigured`]. Call it, then [`rebind_blobs`], before
/// [`import_capsule`](super::import_capsule): then no imported record points at
/// a blob that is not there, or at another store.
///
/// # Errors
///
/// [`DataCapsuleError::Conflict`] for a key with different bytes or MIME type,
/// [`DataCapsuleError::InvalidInput`] for an entry that does not describe its
/// bytes, or [`DataCapsuleError::Blob`] when the capsule has no bytes for an
/// entry or `store` fails.
pub async fn restore_blobs(
    capsule: &DataCapsule,
    store: &dyn BlobStore,
) -> Result<usize, DataCapsuleError> {
    // A failed restore keeps what it wrote, so check every entry against its
    // bytes, and that no two share a key, before the first key is read or
    // written.
    super::archive::check_unique_blob_keys(&capsule.manifest.blobs)?;
    let mut entries = Vec::with_capacity(capsule.manifest.blobs.len());
    for entry in &capsule.manifest.blobs {
        let bytes = capsule
            .blobs
            .get(&entry.sha256)
            .ok_or_else(|| DataCapsuleError::Blob(format!("no bytes for blob {:?}", entry.key)))?;
        super::archive::check_blob_entry(entry, bytes)?;
        entries.push((entry, bytes));
    }
    let mut to_write = Vec::new();
    for (entry, bytes) in entries {
        if !check_existing(store, entry).await? {
            to_write.push((entry, bytes));
        }
    }
    for (entry, bytes) in to_write {
        match store
            .put_if_absent(&entry.key, &entry.content_type, bytes.clone())
            .await
        {
            Ok(Some(_)) => verify_created(store, entry).await?,
            // Another writer took the key after the check. If the key is
            // free again, the blob is not there: that is a conflict too.
            Ok(None) => {
                if !check_existing(store, entry).await? {
                    return Err(DataCapsuleError::Conflict(format!(
                        "blob {:?} changed during the import",
                        entry.key
                    )));
                }
            }
            Err(BlobStoreError::Unsupported(_)) => {
                return Err(DataCapsuleError::NotConfigured(
                    "the blob store cannot create a blob without the risk of replacing one \
                     (BlobStore::put_if_absent)"
                        .to_owned(),
                ));
            }
            Err(e) => return Err(DataCapsuleError::Blob(format!("put {:?}: {e}", entry.key))),
        }
    }
    Ok(capsule.manifest.blobs.len())
}

/// Point each `storage::Blob` object in the blob columns of `capsule` at
/// `store`.
///
/// Its `provider_id`, `etag`, `content_type` and `byte_size` become those
/// that `store` keeps for the key. The handle in a source row can be stale
/// (the key was written again before export), and the target app reads these
/// fields, for example for size limits. Call it
/// after [`restore_blobs`] and before [`import_capsule`](super::import_capsule).
///
/// A plain key string, and a key that the capsule has no bytes for, stay as
/// they are. An object with a restored key gets these fields even when it
/// has no `provider_id`.
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
    let mut metas = std::collections::BTreeMap::new();
    for model in &capsule.manifest.models {
        for row in capsule.records.get_mut(&model.table).into_iter().flatten() {
            for column in &model.blob_columns {
                let Some(serde_json::Value::Object(blob)) = row.get_mut(column) else {
                    continue;
                };
                let Some(key) = blob.get("key").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                // An object without `provider_id` (a capsule written through
                // the public API) is a handle too: its bytes are restored, so
                // it must name the target store.
                if !restored.contains(key) {
                    continue;
                }
                let key = key.to_owned();
                if !metas.contains_key(&key) {
                    let meta = match store.head(&key).await {
                        Ok(Some(meta)) => meta,
                        Ok(None) | Err(BlobStoreError::NotFound(_)) => {
                            return Err(DataCapsuleError::Conflict(format!(
                                "blob {key:?} is gone after the restore"
                            )));
                        }
                        Err(e) => {
                            return Err(DataCapsuleError::Blob(format!("head {key:?}: {e}")));
                        }
                    };
                    metas.insert(key.clone(), meta);
                }
                let meta = &metas[&key];
                blob.insert("provider_id".to_owned(), store.provider_id().into());
                blob.insert("etag".to_owned(), meta.etag.clone().into());
                blob.insert("content_type".to_owned(), meta.content_type.clone().into());
                blob.insert("byte_size".to_owned(), meta.byte_size.into());
            }
        }
    }
    Ok(())
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
