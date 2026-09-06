//! Persistence for the ACME account and issued certificates (issue #1608).
//!
//! [`AcmeStore`] is an async trait so later work can back it with a database or
//! a per-tenant store (#1620/#1635). The first implementation, [`FsAcmeStore`],
//! keeps everything under a **per-directory subdirectory** of the cache
//! directory (`{cache_dir}/{directory-label}/`, `0700`) as `0600` files: the
//! ACME account credentials (`account.json`) and, per certificate,
//! `<cert-id>.chain.pem` + `<cert-id>.key.pem`. Namespacing by directory label
//! keeps a staging leaf from being reused after a promotion to production (a
//! browser-untrusted staging cert still has ~90d validity, so an un-namespaced
//! leaf would silently be served for weeks).

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;

/// A boxed, pinned future returned by [`AcmeStore`] operations, so the trait
/// stays object-safe (`Arc<dyn AcmeStore>`).
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Stable identifier for a certificate, derived from its (sorted) domain set.
///
/// Two configurations requesting the same domains — in any order — map to the
/// same `CertId`, so a stored certificate is reused across restarts and the
/// renewal leader-election key is stable.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CertId(pub String);

impl CertId {
    /// Derive the id from a domain set (order-independent).
    #[must_use]
    pub fn from_domains(domains: &[String]) -> Self {
        use sha2::{Digest as _, Sha256};
        let mut sorted: Vec<&str> = domains.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        sorted.dedup();
        let mut hasher = Sha256::new();
        for domain in sorted {
            hasher.update(domain.as_bytes());
            hasher.update(b"\0");
        }
        let digest = hasher.finalize();
        let mut out = String::with_capacity(32);
        for byte in &digest[..16] {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        Self(out)
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A stored certificate: the PEM chain (leaf first) and its private key PEM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCert {
    /// PEM certificate chain, leaf first.
    pub chain_pem: String,
    /// PEM private key matching the leaf.
    pub key_pem: String,
}

/// Persistence contract for ACME material.
///
/// Implementations must persist the account credentials and each issued
/// certificate durably enough to survive a restart, so a reboot does not
/// re-register or re-order (which would burn ACME rate limits).
pub trait AcmeStore: Send + Sync {
    /// Load the persisted ACME account credentials, if any.
    fn load_account(&self) -> StoreFuture<'_, io::Result<Option<Vec<u8>>>>;

    /// Persist the ACME account credentials.
    fn save_account<'a>(&'a self, data: &'a [u8]) -> StoreFuture<'a, io::Result<()>>;

    /// Load the stored certificate for `id`, if any.
    fn load_cert<'a>(&'a self, id: &'a CertId) -> StoreFuture<'a, io::Result<Option<StoredCert>>>;

    /// Persist the certificate for `id`.
    fn save_cert<'a>(
        &'a self,
        id: &'a CertId,
        cert: &'a StoredCert,
    ) -> StoreFuture<'a, io::Result<()>>;
}

/// Filesystem-backed [`AcmeStore`] rooted at a cache directory.
///
/// Both the account file and the certificate files live under a per-directory
/// subdirectory (`{dir}/{directory_label}/`) so distinct ACME directories
/// (staging vs production vs a custom CA) never share an account **or a
/// certificate** — promoting `directory = "production"` must not reuse a
/// leftover, browser-untrusted staging leaf. All writes are `0600` on Unix
/// (owner-only) inside a `0700` subdirectory, mirroring the owner-only
/// discipline the unix socket path uses — the account key and certificate
/// private key are secrets.
#[derive(Debug, Clone)]
pub struct FsAcmeStore {
    dir: PathBuf,
    directory_label: String,
}

impl FsAcmeStore {
    /// Create a store rooted at `dir`, namespaced by `directory_label` (see
    /// [`crate::acme::directory_label`]).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>, directory_label: impl Into<String>) -> Self {
        Self {
            dir: dir.into(),
            directory_label: directory_label.into(),
        }
    }

    /// The per-directory subdirectory holding this store's account + certs
    /// (`{dir}/{directory_label}/`).
    fn cert_dir(&self) -> PathBuf {
        self.dir.join(&self.directory_label)
    }

    fn account_path(&self) -> PathBuf {
        self.cert_dir().join("account.json")
    }

    /// The on-disk path of the certificate chain PEM for `id`.
    fn chain_path(&self, id: &CertId) -> PathBuf {
        self.cert_dir().join(format!("{}.chain.pem", id.as_str()))
    }

    /// The on-disk path of the private key PEM for `id`.
    fn key_path(&self, id: &CertId) -> PathBuf {
        self.cert_dir().join(format!("{}.key.pem", id.as_str()))
    }

    /// The stored chain+key pair for `domains`, if both files are present.
    ///
    /// Mirrors [`AcmeStore::load_cert`]'s treatment of a partial pair as
    /// absent, but works purely off the filesystem (no read/parse), so a
    /// caller that only needs the paths — like `autumn doctor`'s offline
    /// certificate scan — can locate them without re-deriving this store's
    /// on-disk layout independently (issue #1864).
    #[must_use]
    pub fn find_cert_for_domains(&self, domains: &[String]) -> Option<(PathBuf, PathBuf)> {
        let id = CertId::from_domains(domains);
        let chain = self.chain_path(&id);
        let key = self.key_path(&id);
        if chain.is_file() && key.is_file() {
            Some((chain, key))
        } else {
            None
        }
    }

    /// Enumerate every complete (chain+key) certificate pair currently stored
    /// under this store's directory, alongside its [`CertId`] and on-disk
    /// paths. A partial pair (one file present, its sibling missing — e.g. a
    /// torn write interrupted mid-publish) is skipped rather than reported,
    /// mirroring [`AcmeStore::load_cert`]'s treatment of a partial pair as
    /// absent.
    ///
    /// Returns an empty list (not an error) when the store's directory does
    /// not exist yet — a benign first-run state — so callers do not need to
    /// special-case "never provisioned" from "provisioned, nothing stored".
    ///
    /// # Errors
    ///
    /// Returns an error if the store's directory exists but cannot be read
    /// (e.g. a permissions problem).
    pub fn list_certs(&self) -> io::Result<Vec<(CertId, PathBuf, PathBuf)>> {
        let dir = self.cert_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };

        let mut ids = std::collections::BTreeSet::new();
        for entry in entries {
            let name = entry?.file_name();
            if let Some(id) = name.to_string_lossy().strip_suffix(".chain.pem") {
                ids.insert(id.to_owned());
            }
        }

        Ok(ids
            .into_iter()
            .filter_map(|id| {
                let id = CertId(id);
                let chain = self.chain_path(&id);
                let key = self.key_path(&id);
                key.is_file().then_some((id, chain, key))
            })
            .collect())
    }
}

impl AcmeStore for FsAcmeStore {
    fn load_account(&self) -> StoreFuture<'_, io::Result<Option<Vec<u8>>>> {
        let path = self.account_path();
        Box::pin(async move { read_optional(&path).await })
    }

    fn save_account<'a>(&'a self, data: &'a [u8]) -> StoreFuture<'a, io::Result<()>> {
        let dir = self.cert_dir();
        let path = self.account_path();
        let data = data.to_vec();
        Box::pin(async move {
            ensure_dir(&dir).await?;
            write_owner_only(&path, &data).await
        })
    }

    fn load_cert<'a>(&'a self, id: &'a CertId) -> StoreFuture<'a, io::Result<Option<StoredCert>>> {
        let chain_path = self.chain_path(id);
        let key_path = self.key_path(id);
        Box::pin(async move {
            let (Some(chain), Some(key)) = (
                read_optional(&chain_path).await?,
                read_optional(&key_path).await?,
            ) else {
                return Ok(None);
            };
            Ok(Some(StoredCert {
                chain_pem: String::from_utf8(chain)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
                key_pem: String::from_utf8(key)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            }))
        })
    }

    fn save_cert<'a>(
        &'a self,
        id: &'a CertId,
        cert: &'a StoredCert,
    ) -> StoreFuture<'a, io::Result<()>> {
        let dir = self.cert_dir();
        let chain_path = self.chain_path(id);
        let key_path = self.key_path(id);
        let chain = cert.chain_pem.clone().into_bytes();
        let key = cert.key_pem.clone().into_bytes();
        Box::pin(async move {
            ensure_dir(&dir).await?;
            // Publish the pair as atomically as two files allow: STAGE both temp
            // files fully (write + flush) BEFORE renaming EITHER into place, so a
            // crash can only tear the pair during the two back-to-back rename
            // syscalls rather than across a full write+flush of the key. Any
            // residual torn state (a new chain with the old/mismatched key, or
            // vice-versa) is still caught at load time by the renewal decision's
            // pair validation, which treats a non-loadable pair as absent. If
            // staging the key fails, clean up the already-staged chain temp so it
            // does not linger.
            let chain_tmp = stage_owner_only(&chain_path, &chain).await?;
            let key_tmp = match stage_owner_only(&key_path, &key).await {
                Ok(tmp) => tmp,
                Err(e) => {
                    let _ = tokio::fs::remove_file(&chain_tmp).await;
                    return Err(e);
                }
            };
            publish_staged(&chain_tmp, &chain_path).await?;
            publish_staged(&key_tmp, &key_path).await
        })
    }
}

/// Read a file, returning `Ok(None)` when it does not exist.
async fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Create `dir` (and parents), owner-only on Unix.
///
/// Delegates to the shared [`crate::fs_atomic`] helper (issue #1864) on
/// tokio's blocking thread pool, since the helper's core is synchronous
/// `std::fs`.
async fn ensure_dir(dir: &Path) -> io::Result<()> {
    let dir = dir.to_path_buf();
    blocking(move || crate::fs_atomic::ensure_owner_only_dir(&dir)).await
}

/// Atomically write `data` to `path` with owner-only (`0600`) permissions on
/// Unix, via the shared [`crate::fs_atomic`] helper (issue #1864).
///
/// Stages the data to an unpredictable sibling temp file and then `rename`s
/// it over `path`. `rename` is atomic within a directory, so a crash
/// mid-write can never leave a torn single file for the loader/reload path:
/// `path` is either the old contents or the complete new contents, never a
/// partial write.
async fn write_owner_only(path: &Path, data: &[u8]) -> io::Result<()> {
    let path = path.to_path_buf();
    let data = data.to_vec();
    blocking(move || crate::fs_atomic::write_owner_only(&path, &data)).await
}

/// Stage `data` to an unpredictable owner-only (`0600` on Unix) sibling of
/// `path`, WITHOUT renaming it into place, via the shared [`crate::fs_atomic`]
/// helper (issue #1864).
///
/// Splitting staging from the final [`publish_staged`] rename lets a
/// multi-file publish (the cert chain + its key) stage BOTH files before
/// renaming EITHER, shrinking the window in which a crash could tear the pair
/// down to the two back-to-back rename syscalls.
async fn stage_owner_only(path: &Path, data: &[u8]) -> io::Result<PathBuf> {
    let path = path.to_path_buf();
    let data = data.to_vec();
    blocking(move || crate::fs_atomic::stage_owner_only(&path, &data)).await
}

/// Atomically publish a staged temp file by renaming it over `path`, via the
/// shared [`crate::fs_atomic`] helper (issue #1864). On error, best-effort
/// clean up the temp file so it does not accumulate.
async fn publish_staged(tmp: &Path, path: &Path) -> io::Result<()> {
    let tmp = tmp.to_path_buf();
    let path = path.to_path_buf();
    blocking(move || crate::fs_atomic::publish_staged(&tmp, &path)).await
}

/// Run a blocking `std::fs` operation on tokio's blocking thread pool,
/// converting a task panic into an `io::Error` (not expected in practice —
/// these closures only perform fallible filesystem I/O, they don't panic).
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join_error) => Err(io::Error::other(join_error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_id_is_order_independent_and_deduped() {
        let a = CertId::from_domains(&["b.example.com".into(), "a.example.com".into()]);
        let b = CertId::from_domains(&["a.example.com".into(), "b.example.com".into()]);
        let c = CertId::from_domains(&[
            "a.example.com".into(),
            "b.example.com".into(),
            "a.example.com".into(),
        ]);
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a.as_str().len(), 32);
    }

    #[test]
    fn cert_id_distinct_domain_sets_differ() {
        let a = CertId::from_domains(&["a.example.com".into()]);
        let b = CertId::from_domains(&["b.example.com".into()]);
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn account_save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsAcmeStore::new(dir.path().join("acme"), "staging");
        assert!(store.load_account().await.unwrap().is_none());
        store.save_account(b"creds-json").await.unwrap();
        assert_eq!(
            store.load_account().await.unwrap().as_deref(),
            Some(&b"creds-json"[..])
        );
    }

    #[tokio::test]
    async fn account_is_keyed_per_directory() {
        let dir = tempfile::tempdir().unwrap();
        let staging = FsAcmeStore::new(dir.path(), "staging");
        let production = FsAcmeStore::new(dir.path(), "production");
        staging.save_account(b"staging-creds").await.unwrap();
        // A production store in the same dir must not see the staging account.
        assert!(production.load_account().await.unwrap().is_none());
        assert_eq!(
            staging.load_account().await.unwrap().as_deref(),
            Some(&b"staging-creds"[..])
        );
    }

    #[tokio::test]
    async fn cert_save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsAcmeStore::new(dir.path(), "staging");
        let id = CertId::from_domains(&["app.example.com".into()]);
        assert!(store.load_cert(&id).await.unwrap().is_none());
        let cert = StoredCert {
            chain_pem: "CHAIN".into(),
            key_pem: "KEY".into(),
        };
        store.save_cert(&id, &cert).await.unwrap();
        assert_eq!(store.load_cert(&id).await.unwrap(), Some(cert));
    }

    #[tokio::test]
    async fn partial_cert_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsAcmeStore::new(dir.path(), "staging");
        let id = CertId::from_domains(&["app.example.com".into()]);
        // Only the chain written (no key) → treated as no stored cert.
        let chain_path = store.chain_path(&id);
        tokio::fs::create_dir_all(chain_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&chain_path, b"CHAIN").await.unwrap();
        assert!(store.load_cert(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cert_is_namespaced_per_directory() {
        // Regression: a staging-issued leaf must NOT be served after promoting
        // the same cache dir to production. The cert files are keyed only by the
        // domain-set `CertId`, so before per-directory namespacing a production
        // store would load the leftover (browser-untrusted) staging cert.
        let dir = tempfile::tempdir().unwrap();
        let staging = FsAcmeStore::new(dir.path(), "staging");
        let production = FsAcmeStore::new(dir.path(), "production");
        let id = CertId::from_domains(&["app.example.com".into()]);
        let staging_cert = StoredCert {
            chain_pem: "STAGING-CHAIN".into(),
            key_pem: "STAGING-KEY".into(),
        };
        staging.save_cert(&id, &staging_cert).await.unwrap();

        // Same domain set, but the production store must see nothing.
        assert!(production.load_cert(&id).await.unwrap().is_none());
        // The staging store still finds its own cert.
        assert_eq!(staging.load_cert(&id).await.unwrap(), Some(staging_cert));
    }

    #[tokio::test]
    async fn save_cert_publishes_both_files_without_leftover_temps() {
        // The staged-then-rename publish must leave BOTH files in place and no
        // `.tmp` siblings behind (regression for the atomic-pair-publish change).
        let dir = tempfile::tempdir().unwrap();
        let store = FsAcmeStore::new(dir.path(), "staging");
        let id = CertId::from_domains(&["app.example.com".into()]);
        store
            .save_cert(
                &id,
                &StoredCert {
                    chain_pem: "CHAIN".into(),
                    key_pem: "KEY".into(),
                },
            )
            .await
            .unwrap();

        assert!(store.chain_path(&id).exists(), "chain must be published");
        assert!(store.key_path(&id).exists(), "key must be published");

        let mut entries = tokio::fs::read_dir(store.cert_dir()).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name();
            assert!(
                !name.to_string_lossy().ends_with(".tmp"),
                "no staged temp file should linger, found {name:?}"
            );
        }
    }

    #[tokio::test]
    async fn find_cert_for_domains_locates_the_configured_pair_and_ignores_others() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsAcmeStore::new(dir.path(), "production");
        let configured = vec!["app.example.com".to_owned()];
        let other = vec!["old.example.com".to_owned()];

        store
            .save_cert(
                &CertId::from_domains(&other),
                &StoredCert {
                    chain_pem: "OLD-CHAIN".into(),
                    key_pem: "OLD-KEY".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            store.find_cert_for_domains(&configured).is_none(),
            "a cert for different domains must not be reported as the configured one"
        );

        let id = CertId::from_domains(&configured);
        store
            .save_cert(
                &id,
                &StoredCert {
                    chain_pem: "CHAIN".into(),
                    key_pem: "KEY".into(),
                },
            )
            .await
            .unwrap();
        let (chain, key) = store
            .find_cert_for_domains(&configured)
            .expect("configured pair must be found");
        assert_eq!(chain, store.chain_path(&id));
        assert_eq!(key, store.key_path(&id));
    }

    #[tokio::test]
    async fn find_cert_for_domains_treats_a_partial_pair_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsAcmeStore::new(dir.path(), "production");
        let id = CertId::from_domains(&["app.example.com".into()]);
        tokio::fs::create_dir_all(store.cert_dir()).await.unwrap();
        tokio::fs::write(store.chain_path(&id), b"CHAIN")
            .await
            .unwrap();
        assert!(
            store
                .find_cert_for_domains(&["app.example.com".to_owned()])
                .is_none()
        );
    }

    #[tokio::test]
    async fn find_cert_for_domains_is_absent_when_the_store_dir_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsAcmeStore::new(dir.path(), "production");
        assert!(
            store
                .find_cert_for_domains(&["app.example.com".to_owned()])
                .is_none()
        );
    }

    #[tokio::test]
    async fn list_certs_is_empty_when_the_store_dir_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsAcmeStore::new(dir.path(), "production");
        assert_eq!(store.list_certs().unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn list_certs_enumerates_only_complete_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsAcmeStore::new(dir.path(), "production");

        let a = CertId::from_domains(&["a.example.com".into()]);
        let b = CertId::from_domains(&["b.example.com".into()]);
        store
            .save_cert(
                &a,
                &StoredCert {
                    chain_pem: "A-CHAIN".into(),
                    key_pem: "A-KEY".into(),
                },
            )
            .await
            .unwrap();
        store
            .save_cert(
                &b,
                &StoredCert {
                    chain_pem: "B-CHAIN".into(),
                    key_pem: "B-KEY".into(),
                },
            )
            .await
            .unwrap();
        // A partial pair alongside the two complete ones must be skipped.
        let partial = CertId::from_domains(&["partial.example.com".into()]);
        tokio::fs::write(store.chain_path(&partial), b"PARTIAL")
            .await
            .unwrap();

        let mut listed = store.list_certs().unwrap();
        listed.sort_by(|x, y| x.0.as_str().cmp(y.0.as_str()));
        let mut expected = [a.clone(), b.clone()];
        expected.sort_by(|x, y| x.as_str().cmp(y.as_str()));

        assert_eq!(listed.len(), 2, "the partial pair must not be listed");
        for ((id, chain, key), expected_id) in listed.iter().zip(expected.iter()) {
            assert_eq!(id, expected_id);
            assert_eq!(*chain, store.chain_path(id));
            assert_eq!(*key, store.key_path(id));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn written_files_are_owner_only_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let store = FsAcmeStore::new(dir.path().join("acme"), "staging");
        store.save_account(b"secret").await.unwrap();
        let id = CertId::from_domains(&["app.example.com".into()]);
        store
            .save_cert(
                &id,
                &StoredCert {
                    chain_pem: "CHAIN".into(),
                    key_pem: "KEY".into(),
                },
            )
            .await
            .unwrap();

        for path in [
            store.account_path(),
            store.chain_path(&id),
            store.key_path(&id),
        ] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode,
                0o600,
                "{} should be 0600, was {mode:o}",
                path.display()
            );
        }

        // The per-directory subdirectory that holds the secrets is 0700.
        let subdir_mode = std::fs::metadata(store.cert_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(subdir_mode, 0o700, "cert subdir should be 0700");
    }
}
