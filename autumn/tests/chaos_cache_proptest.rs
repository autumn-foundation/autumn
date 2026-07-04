#![allow(clippy::needless_pass_by_value)]

use autumn_web::cache::{Cache, FillLockStatus};
use autumn_web::cache::{CacheFillError, GetOrComputeOptions, get_or_compute_with};
use proptest::prelude::*;
use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

struct DummyCache {
    state: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    lock: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl Cache for DummyCache {
    fn get_value(&self, _key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        let state = self.state.lock().unwrap();
        state
            .as_ref()
            .map(|s| Arc::new(s.clone()) as Arc<dyn Any + Send + Sync>)
    }

    fn insert_value(&self, _key: &str, value: Arc<dyn Any + Send + Sync>) {
        if let Some(s) = value.downcast_ref::<String>() {
            let mut state = self.state.lock().unwrap();
            *state = Some(s.clone());
        }
    }

    fn invalidate(&self, _key: &str) {}
    fn clear(&self) {}

    fn try_acquire_fill_lock(&self, _key: &str, token: &str, _ttl: Duration) -> FillLockStatus {
        let mut lock = self.lock.lock().unwrap();
        if lock.is_none() {
            *lock = Some(token.to_string());
            FillLockStatus::Acquired
        } else if lock.as_deref() == Some(token) {
            FillLockStatus::Acquired
        } else {
            FillLockStatus::Held
        }
    }

    fn release_fill_lock(&self, _key: &str, token: &str) {
        let mut lock = self.lock.lock().unwrap();
        if lock.as_deref() == Some(token) {
            *lock = None;
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]
    #[test]
    fn get_or_compute_with_timeout_fuzz(timeout_ms in 0..100u64, poll_ms in 1..100u64) {
        let cache: Arc<dyn Cache> = Arc::new(DummyCache {
            state: std::sync::Arc::new(std::sync::Mutex::new(None)),
            lock: std::sync::Arc::new(std::sync::Mutex::new(Some("other_replica_token".to_string()))),
        });

        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let mut opts = GetOrComputeOptions::new();
                opts.distributed_fill_lock = true;
                opts.lock_wait_timeout = Duration::from_millis(timeout_ms);
                opts.lock_poll_interval = Duration::from_millis(poll_ms);

                let result: Result<String, CacheFillError<String>> = get_or_compute_with(&cache, "key", opts, || async {
                    Ok::<String, String>("val".to_string())
                }).await;

                assert!(result.is_ok());
                assert_eq!(result.unwrap(), "val");
            });
    }
}
