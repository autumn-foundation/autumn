use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Stable replica identity. It is generated independently of wall-clock time.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ActorId(pub String);

/// Stable operation identity `(actor, sequence)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OperationId {
    pub actor: ActorId,
    pub sequence: u64,
}

/// Stable identity of an inserted Unicode scalar value.
pub type CharacterId = OperationId;

/// A plain-text CRDT operation. Every insert introduces exactly one character.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TextOperation {
    Insert {
        id: OperationId,
        after: Option<CharacterId>,
        value: char,
    },
    Delete {
        id: OperationId,
        target: CharacterId,
    },
}

impl TextOperation {
    #[must_use]
    pub fn id(&self) -> &OperationId {
        match self {
            Self::Insert { id, .. } | Self::Delete { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Node {
    after: Option<CharacterId>,
    value: char,
    deleted: bool,
}

/// Serializable CRDT state. `BTree*` storage makes equal replicas byte-identical.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextState {
    nodes: BTreeMap<CharacterId, Node>,
    operations: BTreeMap<OperationId, TextOperation>,
    pending_deletes: BTreeSet<CharacterId>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TextError {
    #[error("operation id was reused with different content")]
    OperationIdReused,
}

impl TextState {
    /// Apply an operation idempotently. Missing insert/delete dependencies may
    /// arrive later, so delivery order cannot change the eventual state.
    pub fn apply(&mut self, operation: TextOperation) -> Result<bool, TextError> {
        if let Some(previous) = self.operations.get(operation.id()) {
            return if previous == &operation {
                Ok(false)
            } else {
                Err(TextError::OperationIdReused)
            };
        }
        match &operation {
            TextOperation::Insert { id, after, value } => {
                let deleted = self.pending_deletes.remove(id);
                self.nodes.insert(
                    id.clone(),
                    Node {
                        after: after.clone(),
                        value: *value,
                        deleted,
                    },
                );
            }
            TextOperation::Delete { target, .. } => {
                if let Some(node) = self.nodes.get_mut(target) {
                    node.deleted = true;
                } else {
                    self.pending_deletes.insert(target.clone());
                }
            }
        }
        self.operations.insert(operation.id().clone(), operation);
        Ok(true)
    }

    /// Materialize Unicode text by recursively walking children in ID order.
    #[must_use]
    pub fn text(&self) -> String {
        let mut children: BTreeMap<Option<CharacterId>, Vec<CharacterId>> = BTreeMap::new();
        for (id, node) in &self.nodes {
            children
                .entry(node.after.clone())
                .or_default()
                .push(id.clone());
        }
        fn walk(
            parent: Option<&CharacterId>,
            children: &BTreeMap<Option<CharacterId>, Vec<CharacterId>>,
            nodes: &BTreeMap<CharacterId, Node>,
            out: &mut String,
        ) {
            let key = parent.cloned();
            if let Some(ids) = children.get(&key) {
                for id in ids {
                    if let Some(node) = nodes.get(id) {
                        if !node.deleted {
                            out.push(node.value);
                        }
                        walk(Some(id), children, nodes, out);
                    }
                }
            }
        }
        let mut out = String::new();
        walk(None, &children, &self.nodes, &mut out);
        out
    }

    #[must_use]
    pub fn operation_count(&self) -> usize {
        self.operations.len()
    }
}
