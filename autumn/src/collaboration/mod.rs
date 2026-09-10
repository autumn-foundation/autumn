//! Server-authoritative, offline-capable collaborative plain text.
//!
//! The text CRDT is transport-independent. Deleted characters remain as
//! tombstones; compaction is intentionally deferred until a future protocol
//! carries acknowledgements proving every replica has observed a deletion.

pub mod protocol;
pub mod session;
pub mod store;
pub mod text;

pub use protocol::{
    Cursor, OperationEvent, OperationRequest, PROTOCOL_VERSION, PresenceEvent, Selection,
};
pub use session::{CollaborationPresence, CollaborationSession, SessionError};
pub use store::{AcceptOutcome, CollaborationStore, InMemoryCollaborationStore, StoreError};
pub use text::{ActorId, CharacterId, OperationId, TextError, TextOperation, TextState};

use serde::{Deserialize, Serialize};

/// Marker implemented by `#[model]` for its `#[collaborative]` text fields.
pub trait CollaborativeField {
    /// Stable Rust/model field names routed through the collaborative runtime.
    const COLLABORATIVE_FIELDS: &'static [&'static str];
}

/// A single typed identity used by storage, channels, and presence.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CollaborativeTopic {
    /// Model/collection name.
    pub model: String,
    /// Stable record primary key.
    pub record: String,
    /// Collaborative field name.
    pub field: String,
}

impl CollaborativeTopic {
    /// Construct a topic after checking that its components are safe and bounded.
    pub fn new(
        model: impl Into<String>,
        record: impl Into<String>,
        field: impl Into<String>,
    ) -> Result<Self, protocol::ValidationError> {
        let topic = Self {
            model: model.into(),
            record: record.into(),
            field: field.into(),
        };
        protocol::validate_topic(&topic)?;
        Ok(topic)
    }

    /// Canonical channel/storage key.
    #[must_use]
    pub fn channel_name(&self) -> String {
        format!(
            "collaboration:{}:{}:{}",
            self.model, self.record, self.field
        )
    }

    /// Canonical ephemeral presence key.
    #[must_use]
    pub fn presence_name(&self) -> String {
        format!("{}:presence", self.channel_name())
    }
}
