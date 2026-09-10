use std::sync::Arc;

use autumn_web::collaboration::protocol::{OperationRequest, PROTOCOL_VERSION};
use autumn_web::collaboration::*;
use autumn_web::{Channels, Presence};

fn actor(value: &str) -> ActorId {
    ActorId(value.into())
}
fn id(actor_name: &str, sequence: u64) -> OperationId {
    OperationId {
        actor: actor(actor_name),
        sequence,
    }
}
fn insert(
    actor_name: &str,
    sequence: u64,
    after: Option<CharacterId>,
    value: char,
) -> TextOperation {
    TextOperation::Insert {
        id: id(actor_name, sequence),
        after,
        value,
    }
}
fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    fn visit<T: Clone>(left: Vec<T>, current: Vec<T>, out: &mut Vec<Vec<T>>) {
        if left.is_empty() {
            out.push(current);
            return;
        }
        for index in 0..left.len() {
            let mut rest = left.clone();
            let item = rest.remove(index);
            let mut next = current.clone();
            next.push(item);
            visit(rest, next, out);
        }
    }
    let mut out = Vec::new();
    visit(items.to_vec(), Vec::new(), &mut out);
    out
}

#[test]
fn every_delivery_permutation_converges_byte_identically_without_drops() {
    let a = insert("a", 1, None, 'A');
    let b = insert("b", 1, None, 'B');
    let c = insert("a", 2, Some(id("a", 1)), 'C');
    let delete = TextOperation::Delete {
        id: id("b", 2),
        target: id("a", 1),
    };
    let operations = [a, b, c, delete];
    let mut encodings = Vec::new();
    for delivery in permutations(&operations) {
        let mut state = TextState::default();
        for operation in delivery {
            state.apply(operation).unwrap();
        }
        assert_eq!(state.text(), "CB");
        encodings.push(serde_json::to_vec(&state).unwrap());
    }
    assert!(encodings.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn duplicate_delayed_and_offline_operations_converge() {
    let server_edit = insert("server", 1, None, 'S');
    let offline_parent = insert("offline", 1, None, 'O');
    let offline_child = insert("offline", 2, Some(id("offline", 1)), 'K');
    let mut server = TextState::default();
    server.apply(server_edit.clone()).unwrap();
    server.apply(offline_child.clone()).unwrap(); // dependency delayed
    server.apply(offline_parent.clone()).unwrap();
    assert!(!server.apply(offline_parent.clone()).unwrap()); // reconnect duplicate
    let mut replica = TextState::default();
    for op in [offline_parent, server_edit, offline_child] {
        replica.apply(op).unwrap();
    }
    assert_eq!(server.text().chars().count(), 3);
    assert_eq!(server.text(), replica.text());
    assert_eq!(
        serde_json::to_vec(&server).unwrap(),
        serde_json::to_vec(&replica).unwrap()
    );
}

#[tokio::test]
async fn accepted_operations_and_typed_cursor_selection_reach_both_sessions() {
    let channels = Channels::new(16);
    let presence = Presence::new(channels.clone());
    let session = CollaborationSession::new(
        Arc::new(InMemoryCollaborationStore::default()),
        channels.clone(),
        presence,
    );
    let topic = CollaborativeTopic::new("Note", "42", "body").unwrap();
    let mut operations_a = channels.subscribe(&topic.channel_name());
    let mut operations_b = channels.subscribe(&topic.channel_name());
    let request = OperationRequest {
        version: PROTOCOL_VERSION,
        topic: topic.clone(),
        actor: actor("a"),
        operations: vec![insert("a", 1, None, 'x')],
    };
    session.submit(request.clone()).unwrap();
    session.submit(request).unwrap(); // duplicate is neither reapplied nor republished
    assert!(
        operations_a
            .recv()
            .await
            .unwrap()
            .0
            .contains("\"kind\":\"insert\"")
    );
    assert!(
        operations_b
            .recv()
            .await
            .unwrap()
            .0
            .contains("\"kind\":\"insert\"")
    );

    let mut cursor_a = channels.subscribe(&topic.presence_name());
    let mut cursor_b = channels.subscribe(&topic.presence_name());
    session
        .update_presence(
            &topic,
            actor("a"),
            Cursor {
                anchor: Some(id("a", 1)),
            },
            Some(Selection {
                anchor: None,
                focus: Some(id("a", 1)),
            }),
        )
        .unwrap();
    let a: PresenceEvent = serde_json::from_str(&cursor_a.recv().await.unwrap().0).unwrap();
    let b: PresenceEvent = serde_json::from_str(&cursor_b.recv().await.unwrap().0).unwrap();
    assert_eq!(a, b);
}

#[test]
fn authorization_and_malformed_requests_are_rejected_without_persistence() {
    let channels = Channels::new(4);
    let topic = CollaborativeTopic::new("Note", "42", "body").unwrap();
    let store = Arc::new(InMemoryCollaborationStore::default());
    let session = CollaborationSession::new(store, channels.clone(), Presence::new(channels))
        .with_authorizer(|_, _| false);
    let request = OperationRequest {
        version: PROTOCOL_VERSION,
        topic: topic.clone(),
        actor: actor("a"),
        operations: vec![insert("a", 1, None, 'x')],
    };
    assert!(matches!(
        session.submit(request),
        Err(SessionError::Unauthorized)
    ));
    assert_eq!(session.snapshot(&topic).unwrap().text(), "");
}

#[test]
fn collaborative_field_bypasses_lww_while_ordinary_field_keeps_it() {
    use autumn_web::sync::{Change, ConflictResolver, LwwResolver, Op, RemoteRow, Resolution};
    use chrono::{Duration, Utc};

    let now = Utc::now();
    let ordinary = Change {
        change_id: "title-1".into(),
        collection: "notes".into(),
        pk: "42".into(),
        op: Op::Upsert,
        payload: Some(serde_json::json!({"title":"offline"})),
        base_version: 1,
        updated_at: now - Duration::seconds(1),
    };
    let row = RemoteRow {
        collection: "notes".into(),
        pk: "42".into(),
        payload: Some(serde_json::json!({"title":"server"})),
        version: 2,
        deleted: false,
        updated_at: now,
        device_id: "server".into(),
    };
    assert_eq!(
        LwwResolver.resolve("offline", &ordinary, &row),
        Resolution::KeepServer
    );

    let mut collaborative = TextState::default();
    collaborative.apply(insert("server", 1, None, 'S')).unwrap();
    collaborative
        .apply(insert("offline", 1, None, 'O'))
        .unwrap();
    assert_eq!(
        collaborative.text().chars().count(),
        2,
        "neither concurrent collaborative edit loses to LWW"
    );
}
