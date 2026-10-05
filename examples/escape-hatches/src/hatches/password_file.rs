//! H12: read the database password from a file on each new connection, with
//! `.with_pool_provider(..)`.
//!
//! A secrets sidecar (for example, a Vault agent) writes the database
//! password to a file and rotates it. Autumn reads `database.url` once, at
//! boot. Its pool connections use tokio-postgres, which reads no password
//! file. So, after a rotation, each new pool connection fails until a
//! restart.
//!
//! This provider builds the pool with a custom connect step. The step reads
//! the file each time the pool opens a connection. A rotation takes effect
//! with no restart. Open connections keep working, because Postgres checks
//! the password only at connect.
//!
//! With no file set, the provider uses Autumn's default pool.
//!
//! Limits:
//!
//! - A custom connect step replaces Autumn's own, which also sets up TLS.
//!   This provider does not set up TLS, so it refuses a URL that asks for it.
//!   It never drops TLS silently.
//! - It builds only the primary pool. Autumn builds replica and shard pools
//!   from their URLs, with no password file. So it refuses a replica or
//!   shards.
//! - Migrations do not use the pool. `autumn migrate` and auto-migrate
//!   connect through libpq with `database.url`. Give them a credential of
//!   their own, for example a libpq `PGPASSFILE`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use autumn_web::RuntimeConnection;
use autumn_web::config::DatabaseConfig;
use autumn_web::db::{DatabasePoolProvider, PoolError};
use diesel::ConnectionError;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::pooled_connection::{
    AsyncDieselConnectionManager, ManagerConfig, RecyclingMethod,
};
use diesel_async::{AsyncConnection, AsyncPgConnection};

/// The env var that names the password file.
pub const FILE_ENV: &str = "STOCKROOM_DB_PASSWORD_FILE";

/// A pool provider that reads the password from a file.
#[derive(Debug, Clone, Default)]
pub struct PasswordFilePool {
    file: Option<PathBuf>,
}

impl PasswordFilePool {
    /// Read the password from `file`.
    pub fn new(file: impl AsRef<Path>) -> Self {
        Self {
            file: Some(file.as_ref().to_path_buf()),
        }
    }

    /// Read the password from the file in [`FILE_ENV`]. With no env var, use
    /// Autumn's default pool.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            file: std::env::var_os(FILE_ENV).map(PathBuf::from),
        }
    }
}

impl DatabasePoolProvider for PasswordFilePool {
    async fn create_pool(
        &self,
        config: &DatabaseConfig,
    ) -> Result<Option<Pool<RuntimeConnection>>, PoolError> {
        let Some(file) = self.file.clone() else {
            return autumn_web::db::create_pool(config);
        };
        if config.replica_url.is_some() || !config.shards.is_empty() {
            return Err(refuse(
                "PasswordFilePool builds only the primary pool; remove the replica and shards",
            ));
        }
        let Some(url) = config.effective_primary_url() else {
            return Ok(None);
        };
        check_url(url).map_err(|reason| refuse(&reason))?;

        let base_url = url.to_owned();
        let mut manager_config = ManagerConfig::<AsyncPgConnection>::default();
        manager_config.recycling_method = RecyclingMethod::Fast;
        manager_config.custom_setup = Box::new(move |_| {
            let base_url = base_url.clone();
            let file = file.clone();
            Box::pin(async move {
                let password = tokio::fs::read_to_string(&file).await.map_err(|error| {
                    ConnectionError::BadConnection(format!(
                        "read password file {}: {error}",
                        file.display()
                    ))
                })?;
                let url = with_password(&base_url, password.trim_end_matches(['\r', '\n']))
                    .map_err(ConnectionError::InvalidConnectionUrl)?;
                AsyncPgConnection::establish(&url).await
            })
        });

        let timeout = Duration::from_secs(config.connect_timeout_secs);
        let manager = AsyncDieselConnectionManager::new_with_config(url, manager_config);
        let pool = Pool::builder(manager)
            .max_size(config.effective_primary_pool_size().max(1))
            .wait_timeout(Some(timeout))
            .create_timeout(Some(timeout))
            .runtime(deadpool::Runtime::Tokio1)
            .build()?;
        Ok(Some(pool))
    }
}

fn refuse(reason: &str) -> PoolError {
    PoolError::UnsupportedBackend(reason.to_owned())
}

/// Refuse a URL that the connect step cannot rewrite safely. The check runs
/// at boot, so a bad URL fails there and not at the first connection.
fn check_url(url: &str) -> Result<(), String> {
    if asks_for_tls(url) {
        return Err(
            "PasswordFilePool does not set up TLS; remove `sslmode` or use the default pool"
                .to_owned(),
        );
    }
    let query = url.split_once('?').map_or("", |(_, query)| query);
    if query.split('&').any(|pair| pair.starts_with("password=")) {
        // A query password wins over the user part, so the file would lose.
        return Err("database URL has a `password` query parameter".to_owned());
    }
    with_password(url, "").map(|_| ())
}

/// True if the URL asks for TLS (`sslmode=require`, `verify-ca`, or
/// `verify-full`).
fn asks_for_tls(url: &str) -> bool {
    let Some((_, query)) = url.split_once('?') else {
        return false;
    };
    query.split('&').any(|pair| {
        matches!(
            pair.split_once('='),
            Some(("sslmode", "require" | "verify-ca" | "verify-full"))
        )
    })
}

/// Put `password` into the user part of a `postgres://` URL. The URL must
/// name a user. A password in the URL is replaced.
fn with_password(url: &str, password: &str) -> Result<String, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| "database URL has no scheme".to_owned())?;
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let (user_info, host) = authority
        .rsplit_once('@')
        .ok_or_else(|| "database URL names no user".to_owned())?;
    let user = user_info
        .split_once(':')
        .map_or(user_info, |(user, _)| user);
    Ok(format!(
        "{scheme}://{user}:{}@{host}{tail}",
        percent_encode(password)
    ))
}

/// Percent-encode each byte that is not an RFC 3986 unreserved character.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_password_adds_a_password() {
        assert_eq!(
            with_password("postgres://app@db:5432/stock?x=1", "pw").as_deref(),
            Ok("postgres://app:pw@db:5432/stock?x=1")
        );
    }

    #[test]
    fn with_password_replaces_a_password() {
        assert_eq!(
            with_password("postgres://app:old@db/stock", "new").as_deref(),
            Ok("postgres://app:new@db/stock")
        );
    }

    #[test]
    fn with_password_encodes_reserved_characters() {
        assert_eq!(
            with_password("postgres://app@db", "p@ss:w/rd%").as_deref(),
            Ok("postgres://app:p%40ss%3Aw%2Frd%25@db")
        );
    }

    #[test]
    fn with_password_needs_a_user() {
        assert!(with_password("postgres://db/stock", "pw").is_err());
        assert!(with_password("db/stock", "pw").is_err());
    }

    #[test]
    fn check_url_refuses_what_it_cannot_rewrite() {
        assert!(check_url("postgres://app@db/stock").is_ok());
        assert!(check_url("postgres://app@db/stock?password=old").is_err());
        assert!(check_url("postgres://app@db/stock?sslmode=require").is_err());
        assert!(check_url("host=db user=app").is_err());
    }

    #[test]
    fn asks_for_tls_reads_sslmode() {
        assert!(asks_for_tls("postgres://a@db/x?sslmode=require"));
        assert!(asks_for_tls("postgres://a@db/x?a=1&sslmode=verify-full"));
        assert!(!asks_for_tls("postgres://a@db/x?sslmode=disable"));
        assert!(!asks_for_tls("postgres://a@db/x"));
    }
}
