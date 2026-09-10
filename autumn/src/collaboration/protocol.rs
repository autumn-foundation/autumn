use super::{ActorId, CollaborativeTopic, TextOperation, TextState};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_OPERATIONS: usize = 1_000;
pub const MAX_COMPONENT_BYTES: usize = 256;
pub const MAX_ACTOR_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRequest {
    pub version: u16,
    pub topic: CollaborativeTopic,
    pub actor: ActorId,
    pub operations: Vec<TextOperation>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationEvent {
    pub version: u16,
    pub topic: CollaborativeTopic,
    pub operation: TextOperation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub anchor: Option<super::CharacterId>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub anchor: Option<super::CharacterId>,
    pub focus: Option<super::CharacterId>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceEvent {
    pub version: u16,
    pub actor: ActorId,
    pub cursor: Cursor,
    pub selection: Option<Selection>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u16,
    pub topic: CollaborativeTopic,
    pub state: TextState,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("unsupported collaboration protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("collaborative operation batch exceeds {MAX_OPERATIONS}")]
    TooManyOperations,
    #[error("topic components must be non-empty and at most {MAX_COMPONENT_BYTES} bytes")]
    InvalidTopic,
    #[error("operation actor does not match the authenticated request actor")]
    ActorMismatch,
    #[error("actor ids must be non-empty and at most {MAX_ACTOR_BYTES} bytes")]
    InvalidActor,
}

pub fn validate_topic(topic: &CollaborativeTopic) -> Result<(), ValidationError> {
    if [&topic.model, &topic.record, &topic.field]
        .into_iter()
        .any(|v| {
            v.is_empty()
                || v.len() > MAX_COMPONENT_BYTES
                || v.chars()
                    .any(|character| character == ':' || character.is_control())
        })
    {
        Err(ValidationError::InvalidTopic)
    } else {
        Ok(())
    }
}

pub fn validate_request(request: &OperationRequest) -> Result<(), ValidationError> {
    if request.version != PROTOCOL_VERSION {
        return Err(ValidationError::UnsupportedVersion(request.version));
    }
    validate_topic(&request.topic)?;
    if request.operations.len() > MAX_OPERATIONS {
        return Err(ValidationError::TooManyOperations);
    }
    if request.actor.0.is_empty()
        || request.actor.0.len() > MAX_ACTOR_BYTES
        || request.operations.iter().any(|operation| {
            let id = operation.id();
            id.sequence == 0 || id.actor.0.is_empty() || id.actor.0.len() > MAX_ACTOR_BYTES
        })
    {
        return Err(ValidationError::InvalidActor);
    }
    if request
        .operations
        .iter()
        .any(|op| &op.id().actor != &request.actor)
    {
        return Err(ValidationError::ActorMismatch);
    }
    Ok(())
}
