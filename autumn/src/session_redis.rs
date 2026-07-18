//! Redis-backed session storage.
//!
//! Provides the [`RedisStore`] implementation for the [`SessionStore`] trait,
//! using the `redis` crate to persist session data in a Redis database.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
    )
)]

use std::collections::HashMap;

use redis::AsyncCommands;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};

use crate::session::{SessionBackendConfigError, SessionConfig, SessionStore, SessionStoreError};

#[derive(Clone, Debug)]
pub struct RedisStore {
    connection: ConnectionManager,
    key_prefix: String,
    ttl_secs: u64,
}

impl RedisStore {
    pub(crate) fn from_config(config: &SessionConfig) -> Result<Self, SessionBackendConfigError> {
        let url = config
            .redis
            .url
            .clone()
            .filter(|url| !url.trim().is_empty())
            .ok_or(SessionBackendConfigError::MissingRedisUrl)?;
        let client = redis::Client::open(url)
            .map_err(|error| SessionBackendConfigError::InvalidRedisUrl(error.to_string()))?;
        let connection =
            ConnectionManager::new_lazy_with_config(client, ConnectionManagerConfig::new())
                .map_err(|error| SessionBackendConfigError::InvalidRedisUrl(error.to_string()))?;

        Ok(Self {
            connection,
            key_prefix: config.redis.key_prefix.clone(),
            ttl_secs: config.max_age_secs,
        })
    }

    fn key_for(&self, id: &str) -> String {
        format!("{}:{id}", self.key_prefix)
    }
}

impl SessionStore for RedisStore {
    async fn load(&self, id: &str) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
        let mut connection = self.connection.clone();
        let key = self.key_for(id);
        match connection.get::<_, Option<String>>(&key).await {
            Ok(Some(payload)) => match serde_json::from_str::<HashMap<String, String>>(&payload) {
                Ok(session) => Ok(Some(session)),
                Err(error) => Err(SessionStoreError::backend(
                    "deserialize session payload",
                    format!("{key}: {error}"),
                )),
            },
            Ok(None) => Ok(None),
            Err(error) => Err(SessionStoreError::backend(
                "load session",
                format!("{key}: {error}"),
            )),
        }
    }

    async fn save(&self, id: &str, data: HashMap<String, String>) -> Result<(), SessionStoreError> {
        let mut connection = self.connection.clone();
        let key = self.key_for(id);
        match serde_json::to_string(&data) {
            Ok(payload) => {
                connection
                    .set_ex::<_, _, ()>(&key, payload, self.ttl_secs)
                    .await
                    .map_err(|error| {
                        SessionStoreError::backend("save session", format!("{key}: {error}"))
                    })?;
                Ok(())
            }
            Err(error) => Err(SessionStoreError::backend(
                "serialize session payload",
                format!("{key}: {error}"),
            )),
        }
    }

    async fn destroy(&self, id: &str) -> Result<(), SessionStoreError> {
        let mut connection = self.connection.clone();
        let key = self.key_for(id);
        connection.del::<_, ()>(&key).await.map_err(|error| {
            SessionStoreError::backend("destroy session", format!("{key}: {error}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionRedisConfig;

    #[tokio::test]
    async fn redis_store_from_config_missing_url() {
        let config = SessionConfig {
            redis: SessionRedisConfig {
                url: None,
                ..Default::default()
            },
            ..Default::default()
        };
        let result = RedisStore::from_config(&config);
        assert!(matches!(
            result,
            Err(SessionBackendConfigError::MissingRedisUrl)
        ));
    }

    #[tokio::test]
    async fn redis_store_from_config_empty_url() {
        let config = SessionConfig {
            redis: SessionRedisConfig {
                url: Some("   ".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let result = RedisStore::from_config(&config);
        assert!(matches!(
            result,
            Err(SessionBackendConfigError::MissingRedisUrl)
        ));
    }

    #[tokio::test]
    async fn redis_store_from_config_invalid_url() {
        let config = SessionConfig {
            redis: SessionRedisConfig {
                url: Some("not a redis url".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let result = RedisStore::from_config(&config);
        assert!(matches!(
            result,
            Err(SessionBackendConfigError::InvalidRedisUrl(_))
        ));
    }

    #[tokio::test]
    async fn redis_store_key_for() {
        let store = RedisStore {
            connection: ConnectionManager::new_lazy_with_config(
                redis::Client::open("redis://127.0.0.1/").unwrap(),
                ConnectionManagerConfig::new(),
            )
            .unwrap(),
            key_prefix: "autumn:session".to_string(),
            ttl_secs: 3600,
        };
        assert_eq!(store.key_for("12345"), "autumn:session:12345");
    }
}
