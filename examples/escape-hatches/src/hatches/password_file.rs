//! H12: read the database password from a file on each new connection.

use std::path::{Path, PathBuf};

use autumn_web::RuntimeConnection;
use autumn_web::config::DatabaseConfig;
use autumn_web::db::{DatabasePoolProvider, PoolError};
use diesel_async::pooled_connection::deadpool::Pool;

/// A pool provider that reads the password from a file.
pub struct PasswordFilePool {
    pub file: PathBuf,
}

impl PasswordFilePool {
    /// Read the password from `file`.
    pub fn new(file: impl AsRef<Path>) -> Self {
        Self {
            file: file.as_ref().to_path_buf(),
        }
    }
}

impl DatabasePoolProvider for PasswordFilePool {
    async fn create_pool(
        &self,
        _config: &DatabaseConfig,
    ) -> Result<Option<Pool<RuntimeConnection>>, PoolError> {
        todo!("H12")
    }
}
