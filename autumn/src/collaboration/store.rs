use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::{CollaborativeTopic, TextError, TextOperation, TextState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptOutcome {
    Accepted(TextState),
    Duplicate(TextState),
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Text(#[from] TextError),
    #[error("collaboration store lock is poisoned")]
    Poisoned,
}

/// Authoritative persistence seam. Implementations must atomically deduplicate
/// and persist before returning `Accepted`.
pub trait CollaborationStore: Send + Sync + 'static {
    fn accept(
        &self,
        topic: &CollaborativeTopic,
        operation: TextOperation,
    ) -> Result<AcceptOutcome, StoreError>;
    fn snapshot(&self, topic: &CollaborativeTopic) -> Result<TextState, StoreError>;
}

/// Process-local authoritative store, useful for one-node apps and tests.
#[derive(Clone, Default)]
pub struct InMemoryCollaborationStore {
    inner: Arc<Mutex<BTreeMap<CollaborativeTopic, TextState>>>,
}

impl CollaborationStore for InMemoryCollaborationStore {
    fn accept(
        &self,
        topic: &CollaborativeTopic,
        operation: TextOperation,
    ) -> Result<AcceptOutcome, StoreError> {
        let mut states = self.inner.lock().map_err(|_| StoreError::Poisoned)?;
        let state = states.entry(topic.clone()).or_default();
        let accepted = state.apply(operation)?;
        Ok(if accepted {
            AcceptOutcome::Accepted(state.clone())
        } else {
            AcceptOutcome::Duplicate(state.clone())
        })
    }
    fn snapshot(&self, topic: &CollaborativeTopic) -> Result<TextState, StoreError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| StoreError::Poisoned)?
            .get(topic)
            .cloned()
            .unwrap_or_default())
    }
}
