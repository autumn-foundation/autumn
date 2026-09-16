//! Live collaborative sessions over the real channel, presence and WebSocket
//! seams (issue #1806).
//!
//! Two halves:
//!
//! - **Hub level** — two sessions on one document converge, and the
//!   participant list carries cursors.
//! - **Socket level** — two real WebSocket clients against a real router
//!   converge on the same text, which is the AC2 claim minus the browser.
//!   The browser itself is covered by `examples/collab-notes`'s two-page
//!   Chromium smoke.

#![cfg(all(feature = "collab", feature = "presence"))]

use std::net::SocketAddr;
use std::time::Duration;

use autumn_web::channels::Channels;
use autumn_web::collab::hub::{doc_key, serve_socket};
use autumn_web::collab::{
    CollabClientMessage, CollabError, CollabHub, CollabLimits, CollabServerMessage, CollabText,
    OpId,
};
use autumn_web::extract::Path;
use autumn_web::prelude::*;
use autumn_web::presence::Presence;
use autumn_web::test::TestApp;
use autumn_web::ws::{WebSocket, WsHandler};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as TMessage;

fn hub() -> CollabHub {
    let channels = Channels::new(64);
    let presence = Presence::new(channels.clone());
    CollabHub::new(channels, presence)
}

// ── Hub level ────────────────────────────────────────────────────────────────

/// A document is seeded once: the second editor joins the live document, not
/// a second copy of the row.
#[test]
fn a_document_is_seeded_once_and_shared() {
    let hub = hub();
    let first = hub.open_with("notes:1:body", || CollabText::from_text("seed", "hello"));
    let second = hub.open_with("notes:1:body", || {
        panic!("the seed must not run for an already-live document")
    });
    first
        .handle(
            "ada",
            CollabClientMessage::Insert {
                after: first.document().id_at(4),
                text: "!".to_owned(),
            },
        )
        .expect("insert");
    assert_eq!(second.text(), "hello!");
    assert_eq!(hub.open_keys(), vec!["notes:1:body".to_owned()]);
}

/// AC2 at the hub: two editors' concurrent operations reach both, and the
/// document converges with no lost character.
#[test]
fn two_sessions_converge_over_the_channel() {
    let hub = hub();
    let doc = hub.open_with(&doc_key("notes", 1, "body"), || {
        CollabText::from_text("seed", "hello world")
    });
    let mut watcher = doc.subscribe();

    let ada = doc.join("ada", "Ada");
    let linus = doc.join("linus", "Linus");

    let after_hello = doc.document().id_at(4);
    let end = doc.document().id_at(10);
    ada.handle(CollabClientMessage::Insert {
        after: after_hello,
        text: ",".to_owned(),
    })
    .expect("ada inserts");
    linus
        .handle(CollabClientMessage::Insert {
            after: end,
            text: "!".to_owned(),
        })
        .expect("linus inserts");

    assert_eq!(doc.text(), "hello, world!", "both edits survive");

    // The channel carried the operations, not just the final text.
    let mut ops_seen = 0;
    while let Ok(message) = watcher.try_recv() {
        if let Ok(CollabServerMessage::Ops { ops }) =
            serde_json::from_str::<CollabServerMessage>(message.as_str())
        {
            ops_seen += ops.len();
        }
    }
    assert_eq!(ops_seen, 2, "one operation per inserted character");
}

/// AC3: the participant list names both editors and carries their cursors.
#[test]
fn presence_reports_participants_and_cursors() {
    let hub = hub();
    let doc = hub.document("notes:2:body");

    let ada = doc.join("ada", "Ada");
    {
        let linus = doc.join("linus", "Linus");
        let names: Vec<String> = doc.participants().into_iter().map(|p| p.label).collect();
        assert_eq!(names, vec!["Ada".to_owned(), "Linus".to_owned()]);

        linus
            .handle(CollabClientMessage::Cursor { index: 3 })
            .expect("cursor");
        ada.handle(CollabClientMessage::Cursor { index: 0 })
            .expect("cursor");
        let cursors: Vec<Option<usize>> =
            doc.participants().into_iter().map(|p| p.cursor).collect();
        assert_eq!(cursors, vec![Some(0), Some(3)]);
    }

    // Linus dropped: he is gone from the list, and so is his cursor.
    let remaining = doc.participants();
    assert_eq!(remaining.len(), 1);
    assert!(
        remaining[0].actor.starts_with("ada#"),
        "the hub appends a per-connection number: {}",
        remaining[0].actor
    );
    assert_eq!(remaining[0].label, "Ada");
}

/// A cursor move is broadcast so the other editors can render the caret.
#[test]
fn a_cursor_move_is_broadcast() {
    let hub = hub();
    let doc = hub.document("notes:3:body");
    let ada = doc.join("ada", "Ada");
    let mut watcher = doc.subscribe();

    ada.handle(CollabClientMessage::Cursor { index: 2 })
        .expect("cursor");

    let raw = watcher
        .try_recv()
        .expect("a presence message was published");
    let message: CollabServerMessage = serde_json::from_str(raw.as_str()).expect("decode");
    match message {
        CollabServerMessage::Presence { participants } => {
            assert_eq!(participants.len(), 1);
            assert_eq!(participants[0].cursor, Some(2));
        }
        other => panic!("expected a presence message, got {other:?}"),
    }
}

/// The joining editor gets the whole document plus who else is here.
#[test]
fn the_snapshot_carries_the_document_and_the_participants() {
    let hub = hub();
    let doc = hub.open_with("notes:4:body", || CollabText::from_text("seed", "hi"));
    let _ada = doc.join("ada", "Ada");

    match doc.snapshot() {
        CollabServerMessage::Snapshot {
            elems,
            participants,
            actor,
        } => {
            assert_eq!(elems.len(), 2);
            assert_eq!(elems.iter().map(|e| e.ch).collect::<String>(), "hi");
            assert_eq!(participants.len(), 1);
            assert!(
                actor.is_none(),
                "a document-level snapshot names no editor; `CollabSession::snapshot` does"
            );
        }
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

/// The hub bounds what one client may send: it is a shared authority, and an
/// unbounded document is a memory leak any editor could trigger.
#[test]
fn oversized_messages_are_refused() {
    let hub = hub().with_limits(CollabLimits {
        max_insert_chars: 4,
        max_document_chars: 8,
        max_delete_ids: 2,
        ..CollabLimits::default()
    });
    let doc = hub.document("notes:5:body");

    let too_long = doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: None,
            text: "abcde".to_owned(),
        },
    );
    assert!(too_long.is_err(), "an oversized insert is refused");
    assert_eq!(doc.text(), "", "and nothing was applied");

    for _ in 0..2 {
        doc.handle(
            "ada",
            CollabClientMessage::Insert {
                after: doc.document().id_at(doc.document().len().saturating_sub(1)),
                text: "abcd".to_owned(),
            },
        )
        .expect("within the limit");
    }
    let full = doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: None,
            text: "x".to_owned(),
        },
    );
    assert!(full.is_err(), "a full document is refused");
}

/// Operations from somewhere other than a live editor — a reconnecting
/// offline client, another replica — merge and reach the live editors.
#[test]
fn remote_operations_merge_and_broadcast() {
    let hub = hub();
    let doc = hub.open_with("notes:6:body", || CollabText::from_text("seed", "ab"));
    let mut watcher = doc.subscribe();

    // An offline replica branched from the same base and edited.
    let mut offline = doc.document();
    let ops = offline.insert("offline", 1, "-");

    let waiting = doc.apply_remote(&ops).expect("within the limits");
    assert_eq!(waiting, 0, "the peer sent a complete history");
    assert_eq!(doc.text(), "a-b");
    assert!(
        watcher.try_recv().is_ok(),
        "live editors are told about the merge"
    );
}

/// Closing a document hands back its final state to persist, and forgets it.
#[test]
fn closing_a_document_returns_its_final_state() {
    let hub = hub();
    let doc = hub.open_with("notes:7:body", || CollabText::from_text("seed", "draft"));
    doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: None,
            text: "a ".to_owned(),
        },
    )
    .expect("insert");

    let final_state = hub.close("notes:7:body").expect("the document was live");
    assert_eq!(final_state.text(), "a draft");
    assert!(hub.open_keys().is_empty());
    assert!(
        hub.close("notes:7:body").is_none(),
        "closing twice is quiet"
    );
}

// ── Socket level ─────────────────────────────────────────────────────────────

#[ws("/collab/{key}/{actor}")]
#[public]
async fn collaborate(hub: CollabHub, path: Path<(String, String)>) -> impl WsHandler {
    let (key, actor) = path.0.clone();
    let doc = hub.open_with(&key, || CollabText::from_text("seed", "hello world"));
    move |socket: WebSocket| async move {
        serve_socket(&doc, actor.clone(), actor, socket).await;
    }
}

async fn serve() -> SocketAddr {
    let router = TestApp::new()
        .routes(routes![collaborate])
        .build()
        .into_router();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(addr: SocketAddr, key: &str, actor: &str) -> Socket {
    let (stream, response) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/collab/{key}/{actor}"))
            .await
            .expect("ws connect");
    assert_eq!(response.status().as_u16(), 101);
    stream
}

/// Read messages until one decodes as a snapshot, and return it.
async fn read_snapshot(socket: &mut Socket) -> CollabServerMessage {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("a snapshot arrives")
            .expect("stream open")
            .expect("no error");
        if let TMessage::Text(text) = frame
            && let Ok(message @ CollabServerMessage::Snapshot { .. }) =
                serde_json::from_str::<CollabServerMessage>(&text)
        {
            return message;
        }
    }
}

/// Apply operation messages to `doc` until its text is `wanted`.
///
/// Deadline-bounded rather than budget-bounded: the `test` lane runs at full
/// parallelism, so "wait 400 ms and hope" is a flake on a loaded runner. This
/// stops the moment the expected state arrives and only fails if it never
/// does.
async fn drain_until(socket: &mut Socket, doc: &mut CollabText, wanted: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while doc.text() != wanted {
        let frame = tokio::time::timeout_at(deadline, socket.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {wanted:?}, have {:?}", doc.text()))
            .expect("stream open")
            .expect("no error");
        if let TMessage::Text(text) = frame
            && let Ok(CollabServerMessage::Ops { ops }) = serde_json::from_str(&text)
        {
            doc.apply_all(ops);
        }
    }
}

/// AC2 over the wire: two independent WebSocket clients editing the same
/// field at the same time converge on one text, with no character lost.
#[tokio::test(flavor = "multi_thread")]
async fn two_websocket_clients_converge_on_the_same_text() {
    let addr = serve().await;
    let mut ada = connect(addr, "notes:9:body", "ada").await;
    let mut linus = connect(addr, "notes:9:body", "linus").await;

    // Each client builds its own replica from the snapshot it was sent.
    let mut ada_view = CollabText::new();
    let mut linus_view = CollabText::new();
    for (socket, view) in [(&mut ada, &mut ada_view), (&mut linus, &mut linus_view)] {
        let CollabServerMessage::Snapshot { elems, actor, .. } = read_snapshot(socket).await else {
            unreachable!("read_snapshot only returns snapshots")
        };
        assert!(
            actor.is_some(),
            "a joining editor is told its own actor id, so it can recognise its echoes"
        );
        assert_eq!(
            elems.iter().map(|e| e.ch).collect::<String>(),
            "hello world"
        );
        // Rebuild the replica from the snapshot the way a browser does.
        let mut after = None;
        for element in elems {
            view.apply(autumn_web::collab::CollabOp::Insert {
                id: element.id.clone(),
                after: after.take(),
                ch: element.ch,
            });
            after = Some(element.id);
        }
    }

    // Both edit at once, each anchored to a character it can see.
    let ada_anchor = ada_view.id_at(4);
    let linus_anchor = linus_view.id_at(10);
    ada.send(TMessage::Text(
        serde_json::to_string(&CollabClientMessage::Insert {
            after: ada_anchor,
            text: ",".to_owned(),
        })
        .expect("encode")
        .into(),
    ))
    .await
    .expect("ada sends");
    linus
        .send(TMessage::Text(
            serde_json::to_string(&CollabClientMessage::Insert {
                after: linus_anchor,
                text: "!".to_owned(),
            })
            .expect("encode")
            .into(),
        ))
        .await
        .expect("linus sends");

    drain_until(&mut ada, &mut ada_view, "hello, world!").await;
    drain_until(&mut linus, &mut linus_view, "hello, world!").await;

    assert_eq!(ada_view.text(), "hello, world!");
    assert_eq!(
        linus_view.text(),
        ada_view.text(),
        "both browser-side replicas converge"
    );

    ada.close(None).await.ok();
    linus.close(None).await.ok();
}

/// An unreadable message is answered, not silently dropped, so a client bug
/// is visible instead of looking like a lost edit.
#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_message_is_answered_with_an_error() {
    let addr = serve().await;
    let mut ada = connect(addr, "notes:10:body", "ada").await;
    let _ = read_snapshot(&mut ada).await;

    ada.send(TMessage::Text("{\"type\":\"nonsense\"}".into()))
        .await
        .expect("send");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let frame = tokio::time::timeout_at(deadline, ada.next())
            .await
            .expect("an answer arrives")
            .expect("stream open")
            .expect("no error");
        if let TMessage::Text(text) = frame
            && let Ok(CollabServerMessage::Error { message }) = serde_json::from_str(&text)
        {
            assert!(message.contains("unreadable"), "got {message}");
            break;
        }
    }
    ada.close(None).await.ok();
}

// ── Fixes from the review pass ───────────────────────────────────────────────

/// One message must not be able to wedge a document. An anchor the hub never
/// minted is refused, so it can neither drag the Lamport clock up nor sit in
/// the buffer forever.
#[test]
fn an_anchor_the_hub_never_minted_is_refused() {
    let hub = hub();
    let doc = hub.open_with("notes:11:body", || CollabText::from_text("seed", "hi"));

    let refused = doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: Some(OpId::new(u64::MAX, "x")),
            text: "z".to_owned(),
        },
    );
    assert!(matches!(refused, Err(CollabError::UnknownCharacter { .. })));

    // The document is untouched and still usable.
    assert_eq!(doc.text(), "hi");
    doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: doc.document().id_at(1),
            text: "!".to_owned(),
        },
    )
    .expect("a real anchor still works");
    assert_eq!(doc.text(), "hi!");
}

/// A delete for a character nobody has typed yet is dropped, not buffered.
/// Buffered, it would tombstone that character the moment it appeared.
#[test]
fn a_delete_cannot_pre_empt_a_character_that_does_not_exist() {
    let hub = hub();
    let doc = hub.open_with("notes:12:body", || CollabText::from_text("seed", "ab"));
    let victim = doc.join("victim", "Victim");

    // An attacker names the victim's next thousand character ids.
    let clock = doc.document().clock();
    let future: Vec<OpId> = (1..=1000).map(|n| OpId::new(clock + n, "victim")).collect();
    doc.handle("attacker", CollabClientMessage::Delete { ids: future })
        .expect("the message is within the limits");
    assert_eq!(
        doc.document().pending_len(),
        0,
        "nothing was buffered against the future"
    );

    // The victim types, and the text survives.
    victim
        .handle(CollabClientMessage::Insert {
            after: doc.document().id_at(1),
            text: "cde".to_owned(),
        })
        .expect("insert");
    assert_eq!(doc.text(), "abcde");
}

/// Two connections under one name keep their own caret and their own row.
#[test]
fn two_connections_sharing_a_name_stay_distinct() {
    let hub = hub();
    let doc = hub.document("notes:13:body");
    let one = doc.join("ada", "Ada");
    let two = doc.join("ada", "Ada");

    assert_ne!(one.actor(), two.actor(), "each connection gets its own id");
    one.handle(CollabClientMessage::Cursor { index: 1 })
        .expect("cursor");
    two.handle(CollabClientMessage::Cursor { index: 7 })
        .expect("cursor");

    let people = doc.participants();
    assert_eq!(people.len(), 2, "both connections are listed");
    let cursors: Vec<Option<usize>> = people.iter().map(|p| p.cursor).collect();
    assert!(cursors.contains(&Some(1)) && cursors.contains(&Some(7)));
}

/// A document lives exactly as long as a handle to it does: no leak, and no
/// eviction while the handler still needs it.
#[test]
fn a_document_lives_as_long_as_its_handles() {
    let hub = hub();
    {
        let doc = hub.open_with("notes:14:body", || CollabText::from_text("seed", "x"));
        {
            let _first = doc.join("ada", "Ada");
            let second = doc.join("linus", "Linus");
            assert_eq!(hub.open_keys(), vec!["notes:14:body".to_owned()]);
            // An occupied document is not the app's to evict.
            assert!(hub.close("notes:14:body").is_none());
            drop(second);
        }
        // Every editor has left, but the handler still holds the handle — the
        // window in which it persists. The document must stay findable.
        assert_eq!(
            hub.open_keys(),
            vec!["notes:14:body".to_owned()],
            "the handler's handle keeps it discoverable while it persists"
        );
        assert_eq!(doc.text(), "x");
    }
    assert!(
        hub.open_keys().is_empty(),
        "dropping the last handle releases it"
    );
}

/// The race the weak registry closes: a reconnect that lands *after* the last
/// editor left but *before* the handler persisted must reach the same
/// document, not a second one seeded from the stale row.
#[test]
fn a_reconnect_before_persistence_finds_the_same_document() {
    let hub = hub();
    // The handler's handle, held across the whole save window.
    let doc = hub.open_with("notes:17:body", || CollabText::from_text("seed", "base"));
    {
        let editor = doc.join("ada", "Ada");
        editor
            .handle(CollabClientMessage::Insert {
                after: doc.document().id_at(3),
                text: "!".to_owned(),
            })
            .expect("insert");
    }
    // Editor gone; the handler has not written the row back yet. A reconnect
    // arrives and would re-seed from the stale "base" if the document had
    // been evicted.
    let reconnected = hub.open_with("notes:17:body", || {
        panic!("a reconnect must not re-seed a document the handler still holds")
    });
    assert_eq!(reconnected.text(), "base!", "the edit is still there");

    // Only once every handle is gone does the key free up.
    drop(reconnected);
    drop(doc);
    assert!(hub.open_keys().is_empty());
}

/// An operation the authority refuses is not broadcast. A browser replica has
/// no counter ceiling of its own, so forwarding a refused one would put a
/// character in every client that the document does not have.
#[test]
fn a_refused_remote_operation_is_not_broadcast() {
    let hub = hub();
    let doc = hub.open_with("notes:18:body", || CollabText::from_text("seed", "ab"));
    let mut watcher = doc.subscribe();

    let refused = autumn_web::collab::CollabOp::Insert {
        id: OpId::new(autumn_web::collab::MAX_COUNTER + 1, "peer"),
        after: None,
        ch: 'X',
    };
    doc.apply_remote(&[refused]).expect("within the budget");

    assert_eq!(doc.text(), "ab", "the document refused it");
    assert!(
        watcher.try_recv().is_err(),
        "and nothing was published, so no client can integrate it"
    );
}

/// The registry is bounded: a route that opens documents from a
/// client-supplied key cannot grow it without limit.
#[test]
fn the_document_registry_is_bounded() {
    let hub = hub().with_limits(CollabLimits {
        max_documents: 2,
        ..CollabLimits::default()
    });
    let _a = hub.document("a");
    let _b = hub.document("b");
    let c = hub.document("c");
    assert_eq!(hub.open_keys(), vec!["a".to_owned(), "b".to_owned()]);
    // The overflow document still works; it is simply not shared.
    c.handle(
        "ada",
        CollabClientMessage::Insert {
            after: None,
            text: "hi".to_owned(),
        },
    )
    .expect("insert");
    assert_eq!(c.text(), "hi");
    assert_eq!(hub.document("c").text(), "", "and it was not registered");
}

/// The buffer counts toward the document budget, so unintegrable operations
/// cannot grow a document past its limit.
#[test]
fn buffered_operations_count_toward_the_document_limit() {
    let hub = hub().with_limits(CollabLimits {
        max_document_chars: 4,
        ..CollabLimits::default()
    });
    let doc = hub.open_with("notes:15:body", || CollabText::new());

    // Fill the budget with operations from a peer that will never integrate.
    let orphans: Vec<autumn_web::collab::CollabOp> = (1..=4)
        .map(|n| autumn_web::collab::CollabOp::Insert {
            id: OpId::new(n, "peer"),
            after: Some(OpId::new(900 + n, "ghost")),
            ch: 'x',
        })
        .collect();
    doc.apply_remote(&orphans).expect("within the budget");
    assert_eq!(doc.document().pending_len(), 4);
    assert_eq!(doc.text(), "");

    let full = doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: None,
            text: "y".to_owned(),
        },
    );
    assert!(
        matches!(full, Err(CollabError::DocumentFull { .. })),
        "the buffer is part of what the document costs"
    );
}

/// AC3 end to end: a cursor reported on one real WebSocket reaches the other
/// as a `presence` frame naming both editors. The hub-level tests prove the
/// merge; this proves it crosses the wire.
#[tokio::test(flavor = "multi_thread")]
async fn a_cursor_reaches_the_other_editor_over_the_wire() {
    let addr = serve().await;
    let mut ada = connect(addr, "notes:16:body", "ada").await;
    let mut linus = connect(addr, "notes:16:body", "linus").await;
    let _ = read_snapshot(&mut ada).await;
    let _ = read_snapshot(&mut linus).await;

    ada.send(TMessage::Text(
        serde_json::to_string(&CollabClientMessage::Cursor { index: 4 })
            .expect("encode")
            .into(),
    ))
    .await
    .expect("ada reports her caret");

    // Linus receives a participant list holding both editors and Ada's caret.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let frame = tokio::time::timeout_at(deadline, linus.next())
            .await
            .expect("a presence frame arrives")
            .expect("stream open")
            .expect("no error");
        if let TMessage::Text(text) = frame
            && let Ok(CollabServerMessage::Presence { participants }) =
                serde_json::from_str::<CollabServerMessage>(&text)
            && participants.iter().any(|p| p.cursor == Some(4))
        {
            assert_eq!(
                participants.len(),
                2,
                "both editors are listed: {participants:?}"
            );
            assert!(
                participants.iter().all(|p| p.actor.contains('#')),
                "each connection carries its own minted actor: {participants:?}"
            );
            break;
        }
    }

    ada.close(None).await.ok();
    linus.close(None).await.ok();
}

/// A reconnecting peer replays its whole history. That replay adds nothing,
/// so a capacity preflight must not charge for it — charging the batch length
/// would refuse an idempotent resend from a document at its limit.
#[test]
fn replaying_a_history_is_not_charged_against_the_limit() {
    let hub = hub().with_limits(CollabLimits {
        max_document_chars: 4,
        ..CollabLimits::default()
    });
    let doc = hub.open_with("notes:19:body", || CollabText::from_text("seed", "abcd"));
    assert_eq!(doc.document().element_count(), 4, "the document is full");

    // The peer resends everything it has. None of it is new.
    let replay = doc.document().ops();
    assert!(
        doc.apply_remote(&replay).is_ok(),
        "an idempotent replay adds nothing and must be accepted"
    );
    assert_eq!(doc.text(), "abcd");

    // A genuinely new character is still refused: the limit still holds.
    let overflow = doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: doc.document().id_at(3),
            text: "e".to_owned(),
        },
    );
    assert!(matches!(overflow, Err(CollabError::DocumentFull { .. })));
}

/// A peer id at the ceiling must not pin the clock there: every later local
/// keystroke would mint nothing, silently and permanently.
#[test]
fn a_peer_id_at_the_ceiling_cannot_exhaust_local_minting() {
    let hub = hub();
    let doc = hub.open_with("notes:20:body", || CollabText::from_text("seed", "a"));

    doc.apply_remote(&[autumn_web::collab::CollabOp::Insert {
        id: OpId::new(autumn_web::collab::MAX_COUNTER, "peer"),
        after: None,
        ch: 'X',
    }])
    .expect("within the budget");
    assert_eq!(doc.text(), "a", "the ceiling id was refused");

    // Local editing still works.
    let ops = doc
        .handle(
            "ada",
            CollabClientMessage::Insert {
                after: doc.document().id_at(0),
                text: "b".to_owned(),
            },
        )
        .expect("insert");
    assert_eq!(ops.len(), 1, "the replica can still mint");
    assert_eq!(doc.text(), "ab");
}

/// Operations delivered on the document's own channel — how another replica's
/// edits arrive under a Redis backend — reach the local authority, not just
/// the socket. Otherwise a client anchors its next edit to a character this
/// replica has never heard of.
#[test]
fn operations_delivered_on_the_channel_reach_the_local_authority() {
    let hub = hub();
    let doc = hub.open_with("notes:21:body", || CollabText::from_text("seed", "ab"));

    // Another replica's edit, as it would arrive off the channel.
    let mut elsewhere = doc.document();
    let ops = elsewhere.insert("other-replica", 2, "!");

    doc.merge_delivered(&ops);
    assert_eq!(doc.text(), "ab!", "the local authority has it");

    // And a client can now anchor to the character it just saw.
    let anchor = doc.document().id_at(2);
    doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: anchor,
            text: "?".to_owned(),
        },
    )
    .expect("anchoring to a replicated character is accepted");
    assert_eq!(doc.text(), "ab!?");
}
