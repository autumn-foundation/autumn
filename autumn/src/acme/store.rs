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

    /// The per-directory subdirectory holding this store's account + certs.
    fn cert_dir(&self) -> PathBuf {
        self.dir.join(&self.directory_label)
    }

    fn account_path(&self) -> PathBuf {
        self.cert_dir().join("account.json")
    }

    fn chain_path(&self, id: &CertId) -> PathBuf {
        self.cert_dir().join(format!("{}.chain.pem", id.as_str()))
    }

    fn key_path(&self, id: &CertId) -> PathBuf {
        self.cert_dir().join(format!("{}.key.pem", id.as_str()))
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
            write_owner_only(&chain_path, &chain).await?;
            write_owner_only(&key_path, &key).await
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
async fn ensure_dir(dir: &Path) -> io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // Best-effort tighten of the cache dir to owner-only; ignore on
        // filesystems that reject it (the per-file 0600 below is the real
        // guarantee).
        let _ = tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await;
    }
    Ok(())
}

/// Atomically write `data` to `path` with owner-only (`0600`) permissions on
/// Unix.
///
/// Writes to a sibling `{path}.tmp` (created `0600` from the start via
/// `OpenOptions::mode`, so it is never briefly group/other-readable — the same
/// fail-closed discipline the unix-socket bind uses for its `0600` socket) and
/// then `rename`s it over `path`. `rename` is atomic within a directory, so a
/// crash mid-write can never leave a torn cert/key for the loader/reload path:
/// `path` is either the old contents or the complete new contents, never a
/// partial write.
async fn write_owner_only(path: &Path, data: &[u8]) -> io::Result<()> {
    use tokio::io::AsyncWriteExt as _;

    let tmp = tmp_path(path);
    let mut options = tokio::fs::OpenOptions::new();
    // Truncate in case a previous crash left a stale temp file behind.
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        // `tokio::fs::OpenOptions::mode` is inherent under Unix.
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).await?;
    file.write_all(data).await?;
    file.flush().await?;
    // Belt-and-suspenders: enforce 0600 on the temp file even if it pre-existed
    // with a wider mode (OpenOptions::mode only applies at creation), so the
    // renamed-into-place file is always owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    }
    // Atomic publish. On error, best-effort clean up the temp file so it does
    // not accumulate.
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

/// The sibling temp path (`{path}.tmp`) used for the atomic write-then-rename.
/// Kept in the same directory as `path` so `rename` stays within one filesystem.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
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
