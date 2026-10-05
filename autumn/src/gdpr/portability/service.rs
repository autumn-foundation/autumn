//! One export and import path for the actuator and the CLI.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use super::archive::{CapsuleSigner, VerifyReport, is_safe_segment, verify_dir};
use super::model::{CapsuleModel, DataCapsuleError};
use super::store::CapsuleStore;
use super::{DataCapsule, ImportSummary, export_subject, import_capsule};

/// The directory of the actuator capsule endpoints.
///
/// Install it as an [`AppState`](crate::AppState) extension to turn on
/// `{prefix}/capsules/*`. The endpoints read and write only in this directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleDirectory(PathBuf);

impl CapsuleDirectory {
    /// Use `dir` for capsules.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self(dir.into())
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// The result of [`CapsuleService::export_to`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ExportReport {
    /// The subject id.
    pub subject: String,
    /// The number of records.
    pub records: u64,
    /// The number of blobs.
    pub blobs: u64,
}

/// Export, verify, and import capsules with one set of models, store, and key.
///
/// [`CapsuleService::from_state`] builds it from the app: the
/// [`GdprRegistry`](crate::gdpr::GdprRegistry) capsule models, the Postgres
/// pool, `[security.signing_secret]`, and the blob store (feature `storage`).
#[derive(Clone)]
pub struct CapsuleService {
    models: Arc<[CapsuleModel]>,
    store: Arc<dyn CapsuleStore>,
    signer: CapsuleSigner,
    dir: Option<PathBuf>,
    #[cfg(feature = "storage")]
    blobs: Option<crate::storage::SharedBlobStore>,
}

impl std::fmt::Debug for CapsuleService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapsuleService")
            .field("models", &self.models)
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl CapsuleService {
    /// Make a service.
    #[must_use]
    pub fn new(
        models: Vec<CapsuleModel>,
        store: Arc<dyn CapsuleStore>,
        signer: CapsuleSigner,
    ) -> Self {
        Self {
            models: models.into(),
            store,
            signer,
            dir: None,
            #[cfg(feature = "storage")]
            blobs: None,
        }
    }

    /// Set the capsule directory of the actuator endpoints.
    #[must_use]
    pub fn with_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dir = Some(dir.into());
        self
    }

    /// Copy blobs to and from `store`.
    #[cfg(feature = "storage")]
    #[must_use]
    pub fn with_blob_store(mut self, store: crate::storage::SharedBlobStore) -> Self {
        self.blobs = Some(store);
        self
    }

    /// The capsule models.
    #[must_use]
    pub fn models(&self) -> &[CapsuleModel] {
        &self.models
    }

    /// The capsule directory, if set.
    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Build the service from the app state.
    ///
    /// An installed `CapsuleService` extension has priority. Else the service
    /// uses the capsule models of the [`GdprRegistry`](crate::gdpr::GdprRegistry)
    /// extension, the Postgres pool, and `[security.signing_secret]`. A
    /// [`CapsuleDirectory`] extension sets the directory.
    ///
    /// # Errors
    ///
    /// [`DataCapsuleError::NotConfigured`] when no models or no database is
    /// available, or [`DataCapsuleError::MissingSigningSecret`].
    pub fn from_state(state: &crate::AppState) -> Result<Self, DataCapsuleError> {
        let dir = state
            .extension::<CapsuleDirectory>()
            .map(|d| d.path().to_path_buf());
        if let Some(service) = state.extension::<Self>() {
            let service = (*service).clone();
            return Ok(match dir {
                Some(dir) => service.with_dir(dir),
                None => service,
            });
        }
        let models = state
            .extension::<crate::gdpr::GdprRegistry>()
            .map(|r| r.capsule_models().to_vec())
            .unwrap_or_default();
        if models.is_empty() {
            return Err(DataCapsuleError::NotConfigured(
                "no capsule models: register them with GdprRegistry::capsule".to_owned(),
            ));
        }
        let signer = CapsuleSigner::from_config(&state.config().security.signing_secret)?;
        let mut service = Self::new(models, default_store(state)?, signer);
        service.dir = dir;
        #[cfg(feature = "storage")]
        if let Some(blobs) = state.extension::<crate::storage::BlobStoreState>() {
            service.blobs = Some(blobs.store().clone());
        }
        Ok(service)
    }

    /// The signer of the app: the one of an installed `CapsuleService`
    /// extension, else one from `[security.signing_secret]`.
    ///
    /// # Errors
    ///
    /// [`DataCapsuleError::MissingSigningSecret`] when no service is installed
    /// and no secret is set.
    pub fn signer_for_state(state: &crate::AppState) -> Result<CapsuleSigner, DataCapsuleError> {
        state.extension::<Self>().map_or_else(
            || CapsuleSigner::from_config(&state.config().security.signing_secret),
            |service| Ok(service.signer.clone()),
        )
    }

    /// The path of the capsule `name` in the capsule directory.
    ///
    /// # Errors
    ///
    /// [`DataCapsuleError::NotConfigured`] when no directory is set, or
    /// [`DataCapsuleError::InvalidName`] when `name` is not one plain segment.
    pub fn capsule_path(&self, name: &str) -> Result<PathBuf, DataCapsuleError> {
        let dir = self.dir.as_deref().ok_or_else(|| {
            DataCapsuleError::NotConfigured(
                "no capsule directory: install a CapsuleDirectory extension".to_owned(),
            )
        })?;
        if !is_safe_segment(name) {
            return Err(DataCapsuleError::InvalidName(name.to_owned()));
        }
        Ok(dir.join(name))
    }

    /// A new capsule name for `subject`: `capsule-<subject>-<time>`.
    ///
    /// Characters that are not safe in a file name become `_`.
    #[must_use]
    pub fn capsule_name(subject: &str) -> String {
        let subject: String = subject
            .chars()
            .take(48)
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let time = crate::time::ambient_now().format("%Y%m%dT%H%M%S%6f");
        format!("capsule-{subject}-{time}")
    }

    /// Export `subject` and write the signed capsule to `dir`.
    ///
    /// # Errors
    ///
    /// The errors of [`export_subject`], `collect_blobs`, and
    /// [`DataCapsule::write_dir`].
    pub async fn export_to(
        &self,
        subject: &str,
        dir: &Path,
    ) -> Result<ExportReport, DataCapsuleError> {
        #[cfg_attr(not(feature = "storage"), allow(unused_mut))]
        let mut capsule = export_subject(&self.models, self.store.as_ref(), subject).await?;
        #[cfg(feature = "storage")]
        if let Some(blobs) = &self.blobs {
            super::collect_blobs(&mut capsule, blobs.as_ref()).await?;
        }
        let report = ExportReport {
            subject: subject.to_owned(),
            records: capsule.records.values().map(|r| r.len() as u64).sum(),
            blobs: capsule.manifest.blobs.len() as u64,
        };
        let (dir, signer) = (dir.to_path_buf(), self.signer.clone());
        blocking(move || capsule.write_dir(&dir, &signer)).await?;
        Ok(report)
    }

    /// Verify the capsule in `dir`.
    ///
    /// # Errors
    ///
    /// The errors of [`verify_dir`].
    pub async fn verify(&self, dir: &Path) -> Result<VerifyReport, DataCapsuleError> {
        let (dir, signer) = (dir.to_path_buf(), self.signer.clone());
        blocking(move || verify_dir(&dir, &signer)).await
    }

    /// Verify the capsule in `dir`, then import its blobs and records.
    ///
    /// # Errors
    ///
    /// The errors of [`DataCapsule::read_dir`], `restore_blobs`, and
    /// [`import_capsule`]. [`DataCapsuleError::NotConfigured`] when the capsule
    /// has blobs but the service has no blob store.
    pub async fn import_from(&self, dir: &Path) -> Result<ImportSummary, DataCapsuleError> {
        let (dir, signer) = (dir.to_path_buf(), self.signer.clone());
        let capsule = blocking(move || DataCapsule::read_dir(&dir, &signer)).await?;
        self.restore(&capsule).await?;
        import_capsule(&capsule, &self.models, self.store.as_ref()).await
    }

    #[cfg(feature = "storage")]
    async fn restore(&self, capsule: &DataCapsule) -> Result<(), DataCapsuleError> {
        match &self.blobs {
            Some(blobs) => super::restore_blobs(capsule, blobs.as_ref())
                .await
                .map(drop),
            None => no_blob_store(capsule),
        }
    }

    #[cfg(not(feature = "storage"))]
    #[allow(clippy::unused_async)]
    async fn restore(&self, capsule: &DataCapsule) -> Result<(), DataCapsuleError> {
        no_blob_store(capsule)
    }
}

/// Run file work off the async worker threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, DataCapsuleError> + Send + 'static,
) -> Result<T, DataCapsuleError> {
    crate::time::spawn_blocking(work)
        .await
        .map_err(|e| DataCapsuleError::Store(format!("capsule file task failed: {e}")))?
}

fn no_blob_store(capsule: &DataCapsule) -> Result<(), DataCapsuleError> {
    if capsule.manifest.blobs.is_empty() {
        Ok(())
    } else {
        Err(DataCapsuleError::NotConfigured(
            "the capsule has blobs, but no blob store is configured".to_owned(),
        ))
    }
}

#[cfg(all(feature = "db", not(feature = "sqlite")))]
fn default_store(state: &crate::AppState) -> Result<Arc<dyn CapsuleStore>, DataCapsuleError> {
    state
        .pool()
        .map(|pool| Arc::new(super::PgCapsuleStore::new(pool.clone())) as Arc<dyn CapsuleStore>)
        .ok_or_else(|| DataCapsuleError::NotConfigured("no database is configured".to_owned()))
}

#[cfg(not(all(feature = "db", not(feature = "sqlite"))))]
fn default_store(_state: &crate::AppState) -> Result<Arc<dyn CapsuleStore>, DataCapsuleError> {
    Err(DataCapsuleError::NotConfigured(
        "capsules need Postgres, or an installed CapsuleService".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gdpr::GdprRegistry;

    fn state_with_models() -> crate::AppState {
        let mut config = crate::config::AutumnConfig::default();
        config.security.signing_secret.secret = Some("service-test-secret-0123456789abcdef".into());
        crate::AppState::for_test()
            .with_extension(config)
            .with_extension(GdprRegistry::new().capsule(CapsuleModel::new("users", "id")))
    }

    #[test]
    fn from_state_without_a_postgres_pool_is_not_configured() {
        // No pool in a test state, and no Postgres store with `sqlite`.
        let err = CapsuleService::from_state(&state_with_models()).expect_err("no store");
        assert!(matches!(err, DataCapsuleError::NotConfigured(_)), "{err:?}");
    }

    #[test]
    fn signer_for_state_prefers_an_installed_service() {
        let own = CapsuleSigner::new(b"installed-service-secret-0123456789");
        let service = CapsuleService::new(
            vec![CapsuleModel::new("users", "id")],
            Arc::new(super::super::MemoryCapsuleStore::new()),
            own.clone(),
        );
        // No secret in the config: only the installed signer can work.
        let state = crate::AppState::for_test().with_extension(service);
        let signer = CapsuleService::signer_for_state(&state).expect("installed signer");
        assert_eq!(signer.sign(b"m"), own.sign(b"m"));
    }

    #[test]
    fn signer_for_state_falls_back_to_the_config_secret() {
        let err =
            CapsuleService::signer_for_state(&crate::AppState::for_test()).expect_err("no secret");
        assert!(
            matches!(err, DataCapsuleError::MissingSigningSecret),
            "{err:?}"
        );
        let signer = CapsuleService::signer_for_state(&state_with_models()).expect("config");
        assert_eq!(
            signer.sign(b"m"),
            CapsuleSigner::new(b"service-test-secret-0123456789abcdef").sign(b"m")
        );
    }

    #[test]
    fn from_state_without_a_secret_reports_it() {
        let state = crate::AppState::for_test()
            .with_extension(GdprRegistry::new().capsule(CapsuleModel::new("users", "id")));
        let err = CapsuleService::from_state(&state).expect_err("no secret");
        assert!(
            matches!(err, DataCapsuleError::MissingSigningSecret),
            "{err:?}"
        );
    }
}
