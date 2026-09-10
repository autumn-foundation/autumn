use std::sync::Arc;

use serde::Serialize;

use super::protocol::{self, OperationEvent, OperationRequest, PROTOCOL_VERSION};
use super::{
    AcceptOutcome, ActorId, CollaborationStore, CollaborativeTopic, Cursor, PresenceEvent,
    Selection, StoreError, TextState,
};
use crate::{Channels, Presence, PresenceHandle};

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Validation(#[from] protocol::ValidationError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("not authorized to edit this collaborative field")]
    Unauthorized,
    #[error("collaboration event serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("accepted operation could not be published: {0}")]
    Publish(#[from] crate::ChannelPublishError),
}

/// Coordinates validation, authorization, durable acceptance, and publication.
#[derive(Clone)]
pub struct CollaborationSession<S> {
    store: Arc<S>,
    channels: Channels,
    presence: Presence,
    authorize: Arc<dyn Fn(&ActorId, &CollaborativeTopic) -> bool + Send + Sync>,
}

impl<S: CollaborationStore> CollaborationSession<S> {
    #[must_use]
    pub fn new(store: Arc<S>, channels: Channels, presence: Presence) -> Self {
        Self {
            store,
            channels,
            presence,
            authorize: Arc::new(|_, _| true),
        }
    }
    #[must_use]
    pub fn with_authorizer(
        mut self,
        authorize: impl Fn(&ActorId, &CollaborativeTopic) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.authorize = Arc::new(authorize);
        self
    }

    /// Accept a reconnect/offline batch. Only newly persisted operations are
    /// broadcast, and publication occurs strictly after `store.accept`.
    pub fn submit(&self, request: OperationRequest) -> Result<TextState, SessionError> {
        protocol::validate_request(&request)?;
        if !(self.authorize)(&request.actor, &request.topic) {
            return Err(SessionError::Unauthorized);
        }
        for operation in request.operations {
            if let AcceptOutcome::Accepted(_) =
                self.store.accept(&request.topic, operation.clone())?
            {
                let event = OperationEvent {
                    version: PROTOCOL_VERSION,
                    topic: request.topic.clone(),
                    operation,
                };
                self.publish(request.topic.channel_name(), &event)?;
            }
        }
        Ok(self.store.snapshot(&request.topic)?)
    }

    pub fn snapshot(&self, topic: &CollaborativeTopic) -> Result<TextState, SessionError> {
        Ok(self.store.snapshot(topic)?)
    }

    /// Join ephemeral presence. Dropping the result removes the participant.
    pub fn join(
        &self,
        topic: &CollaborativeTopic,
        actor: ActorId,
        cursor: Cursor,
        selection: Option<Selection>,
    ) -> Result<CollaborationPresence, SessionError> {
        let event = PresenceEvent {
            version: PROTOCOL_VERSION,
            actor: actor.clone(),
            cursor,
            selection,
        };
        let meta = serde_json::to_value(&event)?;
        let handle = self
            .presence
            .track(topic.presence_name(), actor.0.clone(), meta);
        self.publish(topic.presence_name(), &event)?;
        Ok(CollaborationPresence { handle })
    }

    pub fn update_presence(
        &self,
        topic: &CollaborativeTopic,
        actor: ActorId,
        cursor: Cursor,
        selection: Option<Selection>,
    ) -> Result<(), SessionError> {
        self.publish(
            topic.presence_name(),
            &PresenceEvent {
                version: PROTOCOL_VERSION,
                actor,
                cursor,
                selection,
            },
        )?;
        Ok(())
    }

    fn publish(&self, topic: String, value: &impl Serialize) -> Result<(), SessionError> {
        self.channels
            .publish(&topic, serde_json::to_string(value)?)?;
        Ok(())
    }
}

pub struct CollaborationPresence {
    handle: PresenceHandle,
}
impl CollaborationPresence {
    pub fn heartbeat(&self) {
        self.handle.refresh();
    }
    #[must_use]
    pub fn actor(&self) -> &str {
        self.handle.key()
    }
}
