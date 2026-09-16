# Collaborative fields

Mark a text field `#[collaborative]` and two people can edit it at the same
time without losing a character. The merge runs in your binary: no
Liveblocks, no Yjs server, no PartyKit.

> Requires the `collab` feature. The session hub additionally needs
> `presence` (which implies `ws`).
>
> ```toml
> autumn-web = { version = "0.7", features = ["collab", "presence"] }
> ```

## Declare the field

```rust
use autumn_web::collab::CollabText;

#[autumn_web::model(table = "notes")]
pub struct Note {
    #[id]
    pub id: i64,
    pub title: String,
    #[collaborative]
    pub body: CollabText,
}
```

The column is a plain `TEXT` column holding the document as JSON — the same
storage shape `#[translatable]` uses. Give it the empty-document default
(`autumn_web::collab::EMPTY_DOCUMENT`, which is what `autumn generate` and the
declarative lane emit):

```sql
body TEXT NOT NULL DEFAULT '{"elems":[]}'
```

### Promoting an existing column

A column that already holds prose keeps its content: a value that is not a
document decodes as text, seeded deterministically from the text itself. The
column must be `NOT NULL` though — `CollabText` has no null form, and
`#[collaborative]` refuses `Option<CollabText>` — so backfill first:

```sql
UPDATE notes SET body = '' WHERE body IS NULL;
ALTER TABLE notes
  ALTER COLUMN body SET NOT NULL,
  ALTER COLUMN body SET DEFAULT '{"elems":[]}';
```

An empty string, whitespace, and a bare `'{}'` all read as an empty document.

The macro generates, for a field named `body`:

| Method | What it does |
|---|---|
| `body_text()` | the visible text |
| `body_insert(actor, index, text)` | insert, returning the operations to send |
| `body_remove(index, count)` | delete a span |
| `body_set_text(actor, text)` | rewrite with the smallest edit that gets there |
| `body_merge(&other)` | merge another replica's document |

Plus `Note::collaborative_fields()` and the field-name-keyed
`note.collaborative("body")` / `note.collaborative_mut("body")`.

## What you get

- **Convergence.** Replicas that hold the same operations render the same
  text, in any delivery order.
- **Causal safety.** An operation that arrives before the character it refers
  to waits in a buffer. Nothing is dropped.
- **Idempotence.** Applying an operation twice changes nothing, so a
  reconnect is safe.
- **Intention preservation.** An edit anchors to a neighbouring character,
  not to an index, so it lands where the author meant even when the document
  changed in flight.

## Serve a live session

`CollabHub` keeps one live document per field instance and streams operations
over the channel and presence seams the app already has.

```rust
use autumn_web::collab::hub::{doc_key, serve_socket};
use autumn_web::collab::CollabHub;
use autumn_web::prelude::*;
use autumn_web::ws::{WebSocket, WsHandler};

#[ws("/notes/{id}/collab")]
async fn collaborate(
    state: AppState,
    hub: CollabHub,
    session: Session,
    id: Path<i64>,
) -> impl WsHandler {
    let note_id = *id;
    // Authorize the RECORD, then load it, then open the document. Never open
    // from a client-supplied key before that: the hub would allocate a live
    // document for every id a caller can type.
    let note = load_note_for(&session, note_id).await;
    let doc = note.and_then(|note| {
        hub.open_with(&doc_key("notes", note_id, "body"), || note.body.clone()).ok()
    });
    let actor = state.entropy().uuid_v4().to_string();

    move |socket: WebSocket| async move {
        // Not allowed, no such note, or the registry is full.
        let Some(doc) = doc else { return };
        serve_socket(&doc, actor, "Guest", socket).await;
    }
}
```

`serve_socket` is the whole protocol: it joins the document, sends a
snapshot, renews the presence lease, forwards every broadcast to the socket
and every socket message to the hub.

> [!IMPORTANT]
> **Route auth is not record auth.** The hub applies no ownership check of
> its own: anyone who reaches the socket can edit the document the handler
> opens. Authorize the record in the handler, as above, and mark the route
> `#[public]` only when it genuinely is.

The actor id you pass is a **prefix**. `join` appends a per-connection number
and uses the result as the presence key, the cursor key, and the id every
character carries, so two tabs under one name still get their own caret.
Keep it ASCII: character ids break ties on the actor, and a browser replica
compares those as UTF-16 while the server compares UTF-8 bytes.

### Seeding from the database

```rust
let note = repo.find_by_id(id).await?.expect("note");
let doc = hub.open_with(&doc_key("notes", id, "body"), || note.body.clone())?;
```

`open_with` runs the seed only when the document is not already live, so the
second editor joins the document the first is editing rather than a stale
copy of the row. Write it back with `doc.document()`, and evict it with
`hub.close(key)`, which hands you the final state to persist:

```rust
if let Some(closing) = hub.close(&key) {
    repo.update(id, UpdateNote { body: Some(closing.text().clone()), ..Default::default() }).await?;
    closing.finalize();
}
```

The document stays discoverable until you `finalize`. That window is the
write: an editor who reconnects inside it joins the document on its way out
rather than seeding a second authority from a row the write has not reached,
and `finalize` then finds them there and leaves the document alone. Dropping
the guard without finalizing is safe — the next open re-seeds — but finalizing
is how you say the row is written.

It returns `CollabError::RegistryFull` when the registry is at
`CollabLimits::max_documents` and the key is not already live. The hub
refuses rather than serving an untracked document: a document outside the
registry is a second authority for the same record, so the next editor
would silently edit a different copy.

## The wire protocol

Clients send:

| Message | Meaning |
|---|---|
| `{"type":"insert","after":"12@ada","text":"hi"}` | add text after a character id (`after` absent means the start) |
| `{"type":"delete","ids":["12@ada"]}` | tombstone characters |
| `{"type":"cursor","index":7}` | report the caret |

The hub sends `snapshot`, `ops`, `presence` and `error`. A browser keeps a
flat list of characters — tombstones included — in the server's order, so it
can resolve an index to an id and apply incoming operations. See
`examples/collab-notes/static/collab.js` for a complete 150-line replica.

## Presence and cursors

Membership comes from [`Presence`](./presence.md); the caret comes from the
last `cursor` message each editor sent. `doc.participants()` merges the two:

```rust
for person in doc.participants() {
    println!("{} at {:?}", person.label, person.cursor);
}
```

Dropping the `CollabSession` ends the lease, clears the cursor, and tells the
remaining editors.

## Offline edits

The offline-sync engine resolves a conflicting push with last-write-wins,
which discards the older write. For a collaborative field that is data loss.
`CollabResolver` changes the verdict for the named fields only. It needs the
`offline-sync` Cargo feature alongside `collab`:

```toml
autumn-web = { version = "0.7", features = ["collab", "presence", "offline-sync"] }
```


```rust
use autumn_web::collab::CollabResolver;
use autumn_web::sync::server;

server::router(backend, Arc::new(CollabResolver::for_table("notes")))
```

`for_table` reads the registry the `#[model]` macro fills in, so adding a
marker to the model is enough, and it scopes itself to that collection.
`CollabResolver::new(["body"])` matches the field in *every* collection
instead; use `for_table` when another collection's `body` must keep
last-write-wins.

Every other column keeps last-write-wins, and a delete on either side is
still the wrapped resolver's decision — merging would resurrect a deleted
row.

## Limits

`CollabHub` bounds what one client may send, because it is a shared,
long-lived authority:

```rust
use autumn_web::collab::CollabLimits;

let hub = CollabHub::new(channels, presence).with_limits(CollabLimits {
    max_insert_chars: 4_000,
    max_document_chars: 50_000,
    max_delete_ids: 4_000,
});
```

The defaults are 10 000 / 200 000 / 10 000.

## Cost, and what is not here yet

The merge is a Replicated Growable Array. Integration scans the character
list, so a merge is linear in document length and a full replay is quadratic;
deleted characters stay as tombstones. That suits note-sized and
comment-sized fields, which is what this slice covers.

Not in this slice:

- CRDT types other than text — no lists, maps, counters or trees.
- Rich text: a block model, a document schema, a WYSIWYG widget.
- Undo/redo and time travel.
- Access control beyond the app's existing auth on the route.

## See also

- [`examples/collab-notes`](../../examples/collab-notes) — a runnable app.
- [Offline sync](./tauri-mobile-offline-sync.md) — the engine `CollabResolver` plugs into.
- [Presence](./presence.md) — the membership seam.
