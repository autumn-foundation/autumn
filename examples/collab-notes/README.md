# collab-notes — collaborative editing, one binary

Two browsers edit the same note at the same time. Every character both
people type survives, and each sees who else is editing and where their
caret is. No external real-time service.

## Prerequisites

- Rust 1.88.0+

No database and no container: the notes live in memory so the example starts
with one command. See [Persisting the document](#persisting-the-document) for
the one change a real app makes.

## Quick start

```bash
cargo run -p collab-notes
```

Open <http://localhost:3000/notes/1> in **two** browser windows and type in
both. The text merges character by character; the "Editing now" list shows
both sessions.

Success proof:

```bash
curl -s http://localhost:3000/notes/1 | grep -o 'data-socket="[^"]*"'
# data-socket="/notes/1/collab"
```

## What it shows

| Capability | Where |
|---|---|
| `#[collaborative]` field marker | `Note::body` in `src/main.rs` |
| `CollabText` — the text CRDT | the field's type |
| Session hub over channels + presence | `CollabHub` + `serve_socket` in the `#[ws]` handler |
| Participant list and cursors | the `#roster` list in the editor page, fed by presence messages |
| A thin browser replica | `static/collab.js` |

## How it fits together

1. The browser holds a flat list of characters, tombstones included, in the
   same order the server has them.
2. An edit is sent **anchored to the id of its left neighbour**, never to an
   index. An index goes stale in flight; an id does not.
3. The server merges the edit into the document and broadcasts the resulting
   operations. Every client applies them by id, so applying one twice — after
   a reconnect, or because it was already in the snapshot — changes nothing.

## Persisting the document

`Note::body` is a normal Diesel `TEXT` column: `CollabText` encodes itself as
JSON through the same codec `Translated` uses. A database-backed app replaces
the in-memory `Notes` store with a `#[repository]` and seeds the hub from the
row:

```rust
let note = repo.find_by_id(id).await?.expect("note");
let doc = hub.open_with(&doc_key("notes", id, "body"), || note.body.clone());
// ... later, write it back:
repo.update(id, UpdateNote { body: Some(doc.document()), ..Default::default() }).await?;
```

`open_with` seeds only the first time, so the second editor joins the live
document rather than a stale copy of the row.

## Tests

```bash
# Two Chromium pages editing one field (requires Chromium)
cargo test -p collab-notes --features system-tests --test smoke -- --include-ignored
```

## Further reading

- [Collaborative fields guide](../../docs/guide/collaboration.md)
- Issue [#1806](https://github.com/autumn-foundation/autumn/issues/1806)
