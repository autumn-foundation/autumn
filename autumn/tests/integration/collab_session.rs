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
    MAX_ACTOR_LEN, MAX_WIRE_PENDING, OpId,
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
    let first = hub
        .open_with("notes:1:body", || {
            CollabText::from_text("seed", "hello").expect("collab edit refused")
        })
        .expect("open the document");
    let second = hub
        .open_with("notes:1:body", || {
            panic!("the seed must not run for an already-live document")
        })
        .expect("open the document");
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
    let doc = hub
        .open_with(&doc_key("notes", 1, "body"), || {
            CollabText::from_text("seed", "hello world").expect("collab edit refused")
        })
        .expect("open the document");
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
    let doc = hub.document("notes:2:body").expect("open the document");

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
    let doc = hub.document("notes:3:body").expect("open the document");
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
    let doc = hub
        .open_with("notes:4:body", || {
            CollabText::from_text("seed", "hi").expect("collab edit refused")
        })
        .expect("open the document");
    let _ada = doc.join("ada", "Ada");

    match doc.snapshot() {
        CollabServerMessage::Snapshot {
            elems,
            pending,
            participants,
            actor,
        } => {
            assert_eq!(elems.len(), 2);
            assert_eq!(elems.iter().map(|e| e.ch).collect::<String>(), "hi");
            assert!(
                pending.is_empty(),
                "nothing is buffered, so the snapshot carries no operations"
            );
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
    let doc = hub.document("notes:5:body").expect("open the document");

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
    let doc = hub
        .open_with("notes:6:body", || {
            CollabText::from_text("seed", "ab").expect("collab edit refused")
        })
        .expect("open the document");
    let mut watcher = doc.subscribe();

    // An offline replica branched from the same base and edited.
    let mut offline = doc.document();
    let ops = offline
        .insert("offline", 1, "-")
        .expect("collab edit refused");

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
    let doc = hub
        .open_with("notes:7:body", || {
            CollabText::from_text("seed", "draft").expect("collab edit refused")
        })
        .expect("open the document");
    doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: None,
            text: "a ".to_owned(),
        },
    )
    .expect("insert");

    let closing = hub.close("notes:7:body").expect("the document was live");
    assert_eq!(closing.text().text(), "a draft");
    // Still discoverable: the row has not been written yet, so a reconnecting
    // editor must find this document rather than seed a second one from the
    // stale row and overwrite it.
    assert_eq!(hub.open_keys(), vec!["notes:7:body".to_owned()]);

    assert!(closing.finalize().is_none(), "nothing changed since");
    assert!(hub.open_keys().is_empty(), "the write committed");
    assert!(
        hub.close("notes:7:body").is_none(),
        "closing twice is quiet"
    );
}

/// An editor who reconnects while the row is being written joins the document
/// that is on its way out, and keeps it alive.
///
/// Evicting on `close` rather than on `finalize` put a second authority on the
/// same record: this editor would have seeded from a row the write had not
/// reached, and the two copies would have overwritten each other.
#[test]
fn a_reconnect_during_the_write_window_finds_the_live_document() {
    let hub = hub();
    let doc = hub
        .open_with("notes:20:body", || {
            CollabText::from_text("seed", "draft").expect("collab edit refused")
        })
        .expect("open the document");

    // The last editor has gone and the handler is closing up.
    let closing = hub.close("notes:20:body").expect("the document was live");
    assert_eq!(closing.text().text(), "draft");

    // The handler lets its handle go while the write runs. Only the guard
    // keeps the document discoverable now — which is the whole point of it.
    drop(doc);

    // The write is in flight. A reconnect arrives.
    let rejoined = hub
        .open_with("notes:20:body", || {
            panic!("seeding here would be the second authority")
        })
        .expect("open the document");
    let editor = rejoined.join("ada", "Ada");
    rejoined
        .handle(
            "ada",
            CollabClientMessage::Insert {
                after: rejoined.document().id_at(4),
                text: "ed".to_owned(),
            },
        )
        .expect("insert");
    assert_eq!(rejoined.text(), "drafted");

    // The write commits. The document is occupied now, so it is not evicted
    // and not handed back: that editor's handler owns persisting it.
    assert!(closing.finalize().is_none(), "an editor is on it");
    assert_eq!(
        hub.open_keys(),
        vec!["notes:20:body".to_owned()],
        "an editor is on it: evicting would strand them"
    );
    assert_eq!(
        hub.document("notes:20:body").expect("still live").text(),
        "drafted",
        "and it is the same document, with the reconnect's edit"
    );
    drop(editor);
}

// ── Socket level ─────────────────────────────────────────────────────────────

#[ws("/collab/{key}/{actor}")]
#[public]
async fn collaborate(hub: CollabHub, path: Path<(String, String)>) -> impl WsHandler {
    let (key, actor) = &path.0;
    let doc = hub
        .open_with(key, || {
            CollabText::from_text("seed", "hello world").expect("collab edit refused")
        })
        .expect("open the document");
    let actor = actor.clone();
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
    let doc = hub
        .open_with("notes:11:body", || {
            CollabText::from_text("seed", "hi").expect("collab edit refused")
        })
        .expect("open the document");

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
    let doc = hub
        .open_with("notes:12:body", || {
            CollabText::from_text("seed", "ab").expect("collab edit refused")
        })
        .expect("open the document");
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
    let doc = hub.document("notes:13:body").expect("open the document");
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
        let doc = hub
            .open_with("notes:14:body", || {
                CollabText::from_text("seed", "x").expect("collab edit refused")
            })
            .expect("open the document");
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
    let doc = hub
        .open_with("notes:17:body", || {
            CollabText::from_text("seed", "base").expect("collab edit refused")
        })
        .expect("open the document");
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
    let reconnected = hub
        .open_with("notes:17:body", || {
            panic!("a reconnect must not re-seed a document the handler still holds")
        })
        .expect("open the document");
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
    let doc = hub
        .open_with("notes:18:body", || {
            CollabText::from_text("seed", "ab").expect("collab edit refused")
        })
        .expect("open the document");
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

/// An editor who arrives and leaves inside the write window is not dropped.
///
/// `finalize` checking occupancy alone could not see this: by the time it
/// runs, the reconnecting editor has gone and `sessions` is zero again — but
/// the row being committed was taken before they typed. Evicting there would
/// throw away the only copy of their characters.
#[test]
fn an_edit_made_and_ended_inside_the_write_window_survives() {
    let hub = hub();
    let doc = hub
        .open_with("notes:21:body", || {
            CollabText::from_text("seed", "draft").expect("collab edit refused")
        })
        .expect("open the document");

    let closing = hub.close("notes:21:body").expect("the document was live");
    assert_eq!(closing.text().text(), "draft", "what the app will persist");
    drop(doc);

    // A reconnect lands, types, and goes again — all before the write lands.
    {
        let rejoined = hub
            .open_with("notes:21:body", || panic!("a second authority"))
            .expect("open the document");
        let _editor = rejoined.join("ada", "Ada");
        rejoined
            .handle(
                "ada",
                CollabClientMessage::Insert {
                    after: rejoined.document().id_at(4),
                    text: "ed".to_owned(),
                },
            )
            .expect("insert");
        assert_eq!(rejoined.text(), "drafted");
    }

    // The write commits the pre-reconnect snapshot. The document moved on
    // since, so finalize does not release it — it hands back what the
    // document actually holds, because this guard is the last thing keeping
    // that text alive and no row has it.
    let again = closing
        .finalize()
        .expect("the document moved on while the write was in flight");
    assert_eq!(
        again.text().text(),
        "drafted",
        "the reconnect's edit, handed back to persist"
    );

    // The app persists that, and finalizes again. Nothing has changed since,
    // so this time the document is released.
    assert!(
        again.finalize().is_none(),
        "the second write matched the document, so it is released"
    );
    assert!(hub.open_keys().is_empty());
}

/// A backend that refuses every publish, exactly as the Redis one does when
/// its publisher queue is full or closed: it returns before its own local
/// fan-out, so the editors on this node lose the message too.
struct RefusingBackend {
    local: autumn_web::channels::LocalChannelsBackend,
}

impl autumn_web::channels::ChannelsBackend for RefusingBackend {
    fn publish(
        &self,
        _topic: &str,
        _msg: autumn_web::channels::ChannelMessage,
    ) -> Result<usize, autumn_web::channels::ChannelPublishError> {
        Err(autumn_web::channels::ChannelPublishError::QueueFull)
    }

    fn ensure_topic(
        &self,
        topic: &str,
    ) -> std::sync::Arc<tokio::sync::broadcast::Sender<autumn_web::channels::ChannelMessage>> {
        self.local.ensure_topic(topic)
    }

    fn subscribe(&self, topic: &str) -> autumn_web::channels::Subscriber {
        self.local.subscribe(topic)
    }

    fn channel_count(&self) -> usize {
        self.local.channel_count()
    }

    fn gc(&self) {
        self.local.gc();
    }

    fn snapshot(&self) -> std::collections::HashMap<String, autumn_web::channels::ChannelStats> {
        self.local.snapshot()
    }
}

/// An operation still reaches this node's editors when the publish fails.
///
/// The Redis backend returns before its own local fan-out when its publisher
/// queue is full, so logging and moving on cost the editors here the
/// operation too — including the one who sent it, whose editor waits forever
/// for an echo, because a snapshot is only ever sent on join.
#[tokio::test]
async fn a_failed_publish_still_reaches_this_node() {
    let channels = Channels::with_backend(RefusingBackend {
        local: autumn_web::channels::LocalChannelsBackend::new(16),
    });
    let presence = Presence::new(channels.clone());
    let hub = CollabHub::new(channels, presence);
    let doc = hub
        .open_with("notes:25:body", || {
            CollabText::from_text("seed", "hi").expect("collab edit refused")
        })
        .expect("open the document");
    let mut watcher = doc.subscribe();

    doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: doc.document().id_at(1),
            text: "!".to_owned(),
        },
    )
    .expect("insert");

    let delivered = watcher.try_recv().expect(
        "the publish failed, but the editors on this node must still see the \
         operation the document already applied",
    );
    let message: CollabServerMessage =
        serde_json::from_str(delivered.as_str()).expect("a server message");
    assert!(
        matches!(message, CollabServerMessage::Ops { .. }),
        "and it is the operation, not something else: {message:?}"
    );
    assert_eq!(doc.text(), "hi!");
}

/// A replayed operation that is already buffered is not broadcast again.
///
/// A reconnecting peer replays its whole history. An operation whose cause
/// never arrives stays in the buffer, and re-broadcasting it on every replay
/// put another copy into every connected editor's causal buffer — a buffer
/// only ever drained by the cause arriving.
#[test]
fn an_already_buffered_replay_is_not_broadcast_again() {
    let hub = hub();
    let doc = hub
        .open_with("notes:28:body", || {
            CollabText::from_text("seed", "hi").expect("collab edit refused")
        })
        .expect("open the document");
    let mut watcher = doc.subscribe();

    // An operation whose anchor the document has never seen.
    let orphan = autumn_web::collab::CollabOp::Insert {
        id: OpId::new(500, "peer"),
        after: Some(OpId::new(900, "ghost")),
        ch: 'Z',
    };

    doc.apply_remote(std::slice::from_ref(&orphan))
        .expect("within the budget");
    assert!(
        watcher.try_recv().is_ok(),
        "the first arrival is news: every editor needs it buffered"
    );

    // The same peer reconnects and replays it. Nothing has changed.
    doc.apply_remote(std::slice::from_ref(&orphan))
        .expect("within the budget");
    assert!(
        watcher.try_recv().is_err(),
        "the replay is not news, and a client's buffer only grows"
    );
    assert_eq!(doc.document().pending_len(), 1, "still the one copy");
}

/// A replay of history the document already integrated is not broadcast
/// either — and does not pretend the document changed.
///
/// `apply` answers `true` for an idempotent replay exactly as it does for a
/// first arrival, so a reconnecting peer could fan its whole history out to
/// every socket, again on every reconnect. The revision it bumped on the way
/// is what tells a close guard the document moved on, so the same replay
/// could keep a document from ever being released.
#[test]
fn a_replay_of_integrated_history_is_not_broadcast_again() {
    let hub = hub();
    let doc = hub
        .open_with("notes:29:body", || {
            CollabText::from_text("seed", "ab").expect("collab edit refused")
        })
        .expect("open the document");

    // Everything the document already holds, replayed exactly as a
    // reconnecting peer would send it.
    let history = doc.document().ops();
    assert!(!history.is_empty());

    let mut watcher = doc.subscribe();
    doc.apply_remote(&history).expect("a replay costs nothing");

    assert!(
        watcher.try_recv().is_err(),
        "none of it is news, so none of it goes out"
    );
    assert_eq!(doc.text(), "ab", "and the document is untouched");

    // The document did not move, so a close still releases it. Close first,
    // while the handler still holds its handle: dropping it would take the
    // last strong reference and leave nothing to close.
    let closing = hub.close("notes:29:body").expect("live");
    drop(doc);
    assert!(
        closing.finalize().is_none(),
        "a replay that changed nothing must not look like a change"
    );
    assert!(hub.open_keys().is_empty());
}

/// Only one close is outstanding for a document at a time.
///
/// Two persistence paths can reach `close` together — an idle sweep against a
/// disconnect hook. Handing each a guard over the same state let both start a
/// write: the first finalizes and releases the document, an editor reopens
/// and edits a fresh authority, and the second write lands on top with the
/// older text while its `finalize` sees a different `Arc` and says nothing.
#[test]
fn a_second_close_is_refused_while_one_is_outstanding() {
    let hub = hub();
    let doc = hub
        .open_with("notes:26:body", || {
            CollabText::from_text("seed", "draft").expect("collab edit refused")
        })
        .expect("open the document");

    let first = hub.close("notes:26:body").expect("the document was live");
    assert!(
        hub.close("notes:26:body").is_none(),
        "a second writer would hold its own copy of the same document"
    );
    drop(doc);

    // Once the outstanding close is done, the key is closable again — though
    // here it was released, so there is nothing left to close.
    assert!(first.finalize().is_none(), "nothing changed since");
    assert!(hub.open_keys().is_empty());

    // A guard that is dropped rather than finalized releases the claim too.
    let doc = hub
        .open_with("notes:27:body", || {
            CollabText::from_text("seed", "x").expect("collab edit refused")
        })
        .expect("open the document");
    drop(hub.close("notes:27:body").expect("live"));
    assert!(
        hub.close("notes:27:body").is_some(),
        "the dropped guard did not leave the key permanently unclosable"
    );
    drop(doc);
}

/// A seed already past the document limit is refused, not installed.
///
/// Nobody has to type for this: a row written before the limit was lowered,
/// or one whose elements and buffered operations are each inside the wire
/// bounds but together are not. Installing it would start the authority over
/// its own advertised bound, serving a document it then refuses edits to.
#[test]
fn a_seed_past_the_document_limit_is_refused() {
    let hub = hub().with_limits(CollabLimits {
        max_document_chars: 4,
        ..CollabLimits::default()
    });

    let refused = hub
        .open_with("notes:23:body", || {
            CollabText::from_text("import", "far too long").expect("collab edit refused")
        })
        .expect_err("the row does not fit");
    assert!(
        matches!(refused, CollabError::DocumentFull { limit: 4, .. }),
        "the refusal names the limit: {refused:?}"
    );
    assert!(
        hub.open_keys().is_empty(),
        "and nothing was registered, so the next open is not handed an \
         over-budget document"
    );

    // One that fits still opens.
    let ok = hub
        .open_with("notes:24:body", || {
            CollabText::from_text("import", "fits").expect("collab edit refused")
        })
        .expect("within the limit");
    assert_eq!(ok.text(), "fits");
}

/// `with_limits` binds the handles taken after it, not the ones already out.
#[test]
fn with_limits_binds_the_next_handle_not_the_last_one() {
    let loose = hub();
    let early = loose
        .open_with("notes:22:body", CollabText::new)
        .expect("open the document");

    let tight = loose.with_limits(CollabLimits {
        max_document_chars: 2,
        ..CollabLimits::default()
    });

    // The handle taken before the call keeps the bounds it was given.
    early
        .handle(
            "ada",
            CollabClientMessage::Insert {
                after: None,
                text: "abcdef".to_owned(),
            },
        )
        .expect("the earlier handle keeps its own bounds");

    // A handle taken after it does not. It is the same document — the
    // registry is shared — so the characters above are already there.
    let late = tight
        .document("notes:22:body")
        .expect("the same live document");
    assert_eq!(late.text(), "abcdef");
    let refused = late
        .handle(
            "ada",
            CollabClientMessage::Insert {
                after: None,
                text: "g".to_owned(),
            },
        )
        .expect_err("over the tightened limit");
    assert!(
        matches!(refused, CollabError::DocumentFull { .. }),
        "the new bounds apply to the new handle: {refused:?}"
    );
}

/// The registry is bounded: a route that opens documents from a
/// client-supplied key cannot grow it without limit.
///
/// At capacity the hub **refuses**. Serving an untracked document instead
/// bounded nothing — each open still allocated one, and the caller kept it —
/// and it split the record: two editors of one key would each get their own
/// authority and each persist over the other.
#[test]
fn the_document_registry_refuses_at_its_limit() {
    let hub = hub().with_limits(CollabLimits {
        max_documents: 2,
        ..CollabLimits::default()
    });
    let _a = hub.document("a").expect("open the document");
    let b = hub.document("b").expect("open the document");

    let refused = hub.document("c").expect_err("the registry is full");
    assert!(
        matches!(
            refused,
            autumn_web::collab::CollabError::RegistryFull { open: 2, limit: 2 }
        ),
        "the refusal names the limit: {refused:?}"
    );
    assert_eq!(hub.open_keys(), vec!["a".to_owned(), "b".to_owned()]);

    // An editor already on a live key is never refused: turning them away
    // would split a document that is open.
    assert!(hub.document("a").is_ok(), "a live key still opens");

    // A freed slot admits the next key.
    drop(b);
    let c = hub.document("c").expect("the slot freed by `b`");
    c.handle(
        "ada",
        CollabClientMessage::Insert {
            after: None,
            text: "hi".to_owned(),
        },
    )
    .expect("insert");
    assert_eq!(
        hub.document("c").expect("still live").text(),
        "hi",
        "and this one is registered, so the next editor finds it"
    );
}

/// A snapshot carries the operations the document cannot place yet.
///
/// The hub broadcasts an operation when it arrives, not when it later
/// integrates. An editor who joined after one was buffered would never hear
/// of it, and would diverge the moment its cause landed.
#[test]
fn a_snapshot_carries_the_buffered_operations() {
    let hub = hub();
    let doc = hub
        .open_with("notes:19:body", || {
            CollabText::from_text("seed", "ab").expect("collab edit refused")
        })
        .expect("open the document");

    // A peer's operation arrives before the character it is anchored to.
    let orphan = autumn_web::collab::CollabOp::Insert {
        id: OpId::new(500, "peer"),
        after: Some(OpId::new(900, "ghost")),
        ch: 'Z',
    };
    doc.apply_remote(std::slice::from_ref(&orphan))
        .expect("within the budget");
    assert_eq!(doc.document().pending_len(), 1, "it is buffered, not lost");

    let CollabServerMessage::Snapshot { elems, pending, .. } = doc.snapshot() else {
        panic!("a document snapshot is a snapshot");
    };
    assert_eq!(
        elems.iter().map(|e| e.ch).collect::<String>(),
        "ab",
        "the integrated characters"
    );
    assert_eq!(pending, vec![orphan], "and the one still waiting");
}

/// The buffer counts toward the document budget, so unintegrable operations
/// cannot grow a document past its limit.
#[test]
fn buffered_operations_count_toward_the_document_limit() {
    let hub = hub().with_limits(CollabLimits {
        max_document_chars: 4,
        ..CollabLimits::default()
    });
    let doc = hub
        .open_with("notes:15:body", CollabText::new)
        .expect("open the document");

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
    let doc = hub
        .open_with("notes:19:body", || {
            CollabText::from_text("seed", "abcd").expect("collab edit refused")
        })
        .expect("open the document");
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
    let doc = hub
        .open_with("notes:20:body", || {
            CollabText::from_text("seed", "a").expect("collab edit refused")
        })
        .expect("open the document");

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
    let doc = hub
        .open_with("notes:21:body", || {
            CollabText::from_text("seed", "ab").expect("collab edit refused")
        })
        .expect("open the document");

    // Another replica's edit, as it would arrive off the channel.
    let mut elsewhere = doc.document();
    let ops = elsewhere
        .insert("other-replica", 2, "!")
        .expect("collab edit refused");

    doc.merge_delivered(&ops).expect("within the budget");
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

/// The hub must not build a document its own decoder refuses.
///
/// `max_document_chars` does not cover this: a thousand-odd distinct inserts
/// whose anchors never arrive are far below the ten-thousand character limit,
/// yet every one of them stays in the causal buffer — past `MAX_WIRE_PENDING`,
/// the bound `CollabText`'s `Deserialize` enforces. Accepting the batch would
/// leave a row that cannot be read back: the offline resolver skips the value
/// and `decode_column` shows the raw JSON.
#[tokio::test]
async fn a_remote_batch_that_would_overfill_the_causal_buffer_is_refused() {
    let hub = hub();
    let doc = hub
        .open_with("notes:16:body", CollabText::new)
        .expect("open the document");

    // Each op anchors to a character nobody will ever send, so each one
    // buffers. One past the wire limit is one too many.
    let orphans: Vec<autumn_web::collab::CollabOp> = (1..=(MAX_WIRE_PENDING + 1) as u64)
        .map(|n| autumn_web::collab::CollabOp::Insert {
            id: OpId::new(n, "peer"),
            after: Some(OpId::new(9_000_000 + n, "ghost")),
            ch: 'x',
        })
        .collect();

    let refused = doc
        .apply_remote(&orphans)
        .expect_err("past the causal buffer limit");
    assert!(
        matches!(
            refused,
            CollabError::CausalBufferFull {
                limit: MAX_WIRE_PENDING,
                ..
            }
        ),
        "refused for the buffer, not the character count: {refused}"
    );
    assert_eq!(
        doc.document().pending_len(),
        0,
        "a refused batch leaves nothing behind"
    );

    // The same bound on the replica-delivery path.
    let delivered = doc
        .merge_delivered(&orphans)
        .expect_err("past the causal buffer limit");
    assert!(matches!(delivered, CollabError::CausalBufferFull { .. }));

    // What the limit protects: a batch that fits is accepted, and the
    // document it produces round-trips through its own decoder.
    let fits: Vec<autumn_web::collab::CollabOp> = orphans[..MAX_WIRE_PENDING].to_vec();
    doc.apply_remote(&fits).expect("within the buffer limit");
    let encoded = serde_json::to_string(&doc.document()).expect("encode");
    assert!(
        serde_json::from_str::<CollabText>(&encoded).is_ok(),
        "the hub only builds documents its own decoder accepts"
    );
}

/// A long in-order replay buffers nothing, so it must not be refused for a
/// buffer it never fills — the reason the bound is measured on the real
/// outcome rather than on the batch length.
#[tokio::test]
async fn a_long_in_order_replay_is_not_refused_by_the_buffer_bound() {
    let hub = hub();
    let doc = hub
        .open_with("notes:17:body", CollabText::new)
        .expect("open the document");

    let mut source = CollabText::new();
    let text = "x".repeat(MAX_WIRE_PENDING + 50);
    let ops = source.insert("peer", 0, &text).expect("seed the replay");
    assert!(ops.len() > MAX_WIRE_PENDING, "longer than the buffer bound");

    doc.apply_remote(&ops)
        .expect("an in-order replay integrates as it goes and buffers nothing");
    assert_eq!(doc.document().pending_len(), 0);
    assert_eq!(doc.text().chars().count(), ops.len());
}

/// Typing over a selection must not be able to delete the text and then fail
/// to put anything back.
///
/// A tombstone costs what a character costs, so a document at its limit
/// refuses the insert — and when the delete travelled as its own message, it
/// had already landed. The editor destroyed the text it was asked to replace.
#[tokio::test]
async fn a_replacement_at_the_document_limit_leaves_the_text_alone() {
    let hub = hub().with_limits(CollabLimits {
        max_document_chars: 5,
        ..CollabLimits::default()
    });
    let doc = hub
        .open_with("notes:18:body", CollabText::new)
        .expect("open the document");

    doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: None,
            text: "hello".to_owned(),
        },
    )
    .expect("fills the document exactly");
    assert_eq!(doc.text(), "hello");

    let ids: Vec<OpId> = doc
        .document()
        .elements()
        .iter()
        .map(|e| e.id.clone())
        .collect();

    // The old shape, for the record: the delete alone is accepted, because a
    // delete is never refused for size.
    let refused = doc.handle(
        "ada",
        CollabClientMessage::Replace {
            ids,
            after: None,
            text: "goodbye".to_owned(),
        },
    );
    assert!(
        matches!(refused, Err(CollabError::DocumentFull { .. })),
        "the replacement is refused: {refused:?}"
    );
    assert_eq!(
        doc.text(),
        "hello",
        "and refused whole — the selection is still there"
    );
}

/// A replacement that fits applies both halves and broadcasts them together.
#[tokio::test]
async fn a_replacement_that_fits_removes_and_adds_in_one_edit() {
    let hub = hub();
    let doc = hub
        .open_with("notes:19:body", CollabText::new)
        .expect("open the document");

    doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: None,
            text: "hello world".to_owned(),
        },
    )
    .expect("seed");

    let elems = doc.document();
    let elems = elems.elements();
    // Replace "hello" with "goodbye", anchored before the span.
    let ids: Vec<OpId> = elems[..5].iter().map(|e| e.id.clone()).collect();

    doc.handle(
        "ada",
        CollabClientMessage::Replace {
            ids,
            after: None,
            text: "goodbye".to_owned(),
        },
    )
    .expect("within the limits");
    assert_eq!(doc.text(), "goodbye world");
}

/// A session whose name is long enough to push `name#seat` past the actor
/// limit must still be able to type.
///
/// The bound that rejects an overlong actor is exactly what made this
/// reachable: the session joined, took a presence lease, and then failed
/// every edit. It also depended on the seat counter's digit count, so the
/// same name worked until the process had served enough sessions.
#[tokio::test]
async fn a_long_session_name_still_mints_ids() {
    let hub = hub();
    let doc = hub
        .open_with("notes:20:body", CollabText::new)
        .expect("open the document");

    let session = doc.join("l".repeat(MAX_ACTOR_LEN * 2), "long");
    assert!(
        session.actor().len() <= MAX_ACTOR_LEN,
        "the generated actor fits: {} bytes",
        session.actor().len()
    );

    doc.handle(
        session.actor(),
        CollabClientMessage::Insert {
            after: None,
            text: "hi".to_owned(),
        },
    )
    .expect("a joined session can type");
    assert_eq!(doc.text(), "hi");
}

/// The seed is the third door into a document the decoder refuses.
///
/// A thousand-odd unresolved operations are far below `max_document_chars`
/// and past `MAX_WIRE_PENDING`, and a caller can build such a value without a
/// hub at all — `apply` and `encode_column` are public — then seed from the
/// stored row later. Serving it would persist something offline sync and
/// every other serde consumer refuse to read.
#[tokio::test]
async fn a_seed_past_the_causal_buffer_limit_is_refused() {
    let hub = hub();

    let mut seed = CollabText::new();
    for n in 1..=(MAX_WIRE_PENDING + 1) as u64 {
        seed.apply(autumn_web::collab::CollabOp::Insert {
            id: OpId::new(n, "peer"),
            after: Some(OpId::new(8_000_000 + n, "ghost")),
            ch: 'x',
        });
    }
    assert_eq!(seed.pending_len(), MAX_WIRE_PENDING + 1);
    assert!(
        seed.element_count() < CollabLimits::default().max_document_chars,
        "well inside the character limit, which is the point"
    );

    let refused = hub
        .open_with("notes:21:body", || seed.clone())
        .expect_err("past the causal buffer limit");
    assert!(
        matches!(
            refused,
            CollabError::CausalBufferFull {
                limit: MAX_WIRE_PENDING,
                ..
            }
        ),
        "refused for the buffer, not the character count: {refused}"
    );

    // One fewer is served, so the bound is exact rather than conservative.
    let mut fits = CollabText::new();
    for n in 1..=MAX_WIRE_PENDING as u64 {
        fits.apply(autumn_web::collab::CollabOp::Insert {
            id: OpId::new(n, "peer"),
            after: Some(OpId::new(8_000_000 + n, "ghost")),
            ch: 'x',
        });
    }
    let doc = hub
        .open_with("notes:22:body", || fits.clone())
        .expect("within the buffer limit");
    let encoded = serde_json::to_string(&doc.document()).expect("encode");
    assert!(
        serde_json::from_str::<CollabText>(&encoded).is_ok(),
        "the hub only serves documents its own decoder accepts"
    );
}

/// A handle that outlived the registry entry must not be able to lose a write.
///
/// `CollabDoc` is `Clone` and holding one is not a session, so a background
/// job that kept one still owns the state after the last editor leaves and
/// the close commits. Editing it then writes to a document nothing persists
/// and no `open_with` returns — the write vanishes with no error anywhere.
#[tokio::test]
async fn a_released_document_refuses_edits_through_a_retained_handle() {
    let hub = hub();
    let doc = hub
        .open_with("notes:23:body", CollabText::new)
        .expect("open the document");

    doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: None,
            text: "hi".to_owned(),
        },
    )
    .expect("seed");

    // No session is live — the handle alone is what keeps this alive.
    let guard = hub.close("notes:23:body").expect("nobody is editing");
    assert_eq!(guard.text().text(), "hi");
    assert!(guard.finalize().is_none(), "released");

    // Reading still works: this is the final state, and reading cannot lose it.
    assert_eq!(doc.text(), "hi");

    // Editing does not, through any of the paths that could lose the write.
    let refused = doc.handle(
        "ada",
        CollabClientMessage::Insert {
            after: None,
            text: "!".to_owned(),
        },
    );
    assert!(
        matches!(refused, Err(CollabError::DocumentReleased { .. })),
        "the write is refused, not silently orphaned: {refused:?}"
    );
    assert!(matches!(
        doc.apply_remote(&[]),
        Err(CollabError::DocumentReleased { .. })
    ));
    assert!(matches!(
        doc.merge_delivered(&[]),
        Err(CollabError::DocumentReleased { .. })
    ));

    // And the next opener gets a fresh document from the row, not the orphan.
    let reopened = hub
        .open_with("notes:23:body", CollabText::new)
        .expect("reopen");
    assert_eq!(reopened.text(), "", "seeded fresh, as the caller intended");
}
