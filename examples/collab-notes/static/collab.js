// A thin replica of the server's text CRDT.
//
// The server is the authority: it holds the document and merges every edit.
// The browser keeps a flat list of characters — tombstones included — in the
// same order the server has them, so it can do two things:
//
//   1. Anchor an edit to the id of its left neighbour, not to an index. The
//      index may be stale by the time the server sees it; the id is not.
//   2. Apply the operations the server broadcasts, including its own echoed
//      back, by id. Applying one twice is a no-op, so a reconnect is safe.

(function () {
  const editor = document.getElementById("editor");
  const roster = document.getElementById("roster");
  const status = document.getElementById("status");
  if (!editor) return;

  /** @type {{id: string, ch: string, deleted: boolean}[]} */
  let elems = [];
  const known = new Set();
  /** This editor's own actor id, from the snapshot. */
  let myActor = null;
  /**
   * Characters this editor typed and spliced in locally, waiting for the
   * server to mint their real ids. One entry per sent insert, oldest first;
   * the server answers our messages in order, so the oldest entry matches the
   * next echo. Holding them in `elems` is what lets a remote character land
   * beside them instead of appearing to replace them.
   */
  let provisional = [];
  let provisionalSeq = 0;
  /** Delete ids sent but not yet echoed back. */
  const unsentDeletes = new Set();
  // Operations whose left neighbour has not arrived. The server buffers the
  // same way; dropping one would leave this replica short a character.
  let waiting = [];

  // Ids are "<counter>@<actor>". Order by counter, then actor — the same
  // order the server uses, which is what keeps the two lists identical.
  //
  // The actor half compares UTF-8 bytes, because that is what Rust's `String`
  // ordering does. A plain JS `>` compares UTF-16 code units, which disagrees
  // outside the basic multilingual plane; the server's ids are ASCII, so this
  // only guards the case where an app passes something else.
  const utf8 = new TextEncoder();
  function actorGreater(a, b) {
    const x = utf8.encode(a);
    const y = utf8.encode(b);
    const shared = Math.min(x.length, y.length);
    for (let i = 0; i < shared; i++) {
      if (x[i] !== y[i]) return x[i] > y[i];
    }
    return x.length > y.length;
  }
  function greater(a, b) {
    const at = a.indexOf("@");
    const bt = b.indexOf("@");
    const an = Number(a.slice(0, at));
    const bn = Number(b.slice(0, bt));
    if (an !== bn) return an > bn;
    return actorGreater(a.slice(at + 1), b.slice(bt + 1));
  }

  function indexOfId(id) {
    return elems.findIndex((e) => e.id === id);
  }

  function applyInsert(id, after, ch) {
    if (known.has(id)) return true;
    let at = after == null ? 0 : indexOfId(after) + 1;
    if (after != null && at === 0) return false; // cause not here yet
    while (at < elems.length && greater(elems[at].id, id)) at += 1;
    elems.splice(at, 0, { id, ch, deleted: false });
    known.add(id);
    return true;
  }

  function applyDelete(target) {
    const at = indexOfId(target);
    if (at < 0) return false;
    elems[at].deleted = true;
    return true;
  }

  function apply(op) {
    return op.op === "insert"
      ? applyInsert(op.id, op.after ?? null, op.ch)
      : applyDelete(op.target);
  }

  // Integrate an operation, or hold it until its cause arrives. Retry the
  // buffer after every success, since one arrival can unblock several.
  function integrate(op) {
    if (!apply(op)) {
      waiting.push(op);
      return;
    }
    let progressed = true;
    while (progressed && waiting.length > 0) {
      progressed = false;
      waiting = waiting.filter((held) => {
        if (apply(held)) {
          progressed = true;
          return false;
        }
        return true;
      });
    }
  }

  function visible() {
    return elems.filter((e) => !e.deleted);
  }

  // True when the server has echoed everything this editor sent.
  function settled() {
    return provisional.length === 0 && unsentDeletes.size === 0;
  }

  // Our own operation coming back. Drop the placeholder it replaces so the
  // real, server-ordered character can take its place.
  function acknowledge(op) {
    if (op.op === "insert") {
      if (myActor !== null && op.id.slice(op.id.indexOf("@") + 1) === myActor) {
        const placeholder = provisional.shift();
        if (placeholder !== undefined) {
          const at = indexOfId(placeholder);
          if (at >= 0) {
            elems.splice(at, 1);
            known.delete(placeholder);
          }
        }
      }
    } else {
      unsentDeletes.delete(op.target);
    }
  }

  // Drop every placeholder and un-delete anything the server refused. Used
  // when the server rejects an edit, and when a snapshot resets the world.
  function discardProvisional() {
    for (const id of provisional) {
      const at = indexOfId(id);
      if (at >= 0) {
        elems.splice(at, 1);
        known.delete(id);
      }
    }
    provisional = [];
    for (const id of unsentDeletes) {
      const at = indexOfId(id);
      if (at >= 0) elems[at].deleted = false;
    }
    unsentDeletes.clear();
  }

  function text() {
    return visible()
      .map((e) => e.ch)
      .join("");
  }

  // `selectionStart` counts UTF-16 code units; the document counts Unicode
  // scalars, one per element. They differ for any character outside the basic
  // multilingual plane, so convert at the boundary rather than mix them.
  function caretToCodePoints(offset) {
    return [...editor.value.slice(0, offset)].length;
  }

  function codePointsToCaret(count) {
    return visible()
      .slice(0, count)
      .map((e) => e.ch)
      .join("").length;
  }

  // The caret rides an element id, so it stays put when somebody else types
  // above it.
  function caretAnchor() {
    const at = caretToCodePoints(editor.selectionStart);
    const shown = visible();
    return at === 0 ? null : (shown[at - 1] || {}).id || null;
  }

  function restoreCaret(anchor) {
    if (anchor == null) {
      editor.setSelectionRange(0, 0);
      return;
    }
    const at = visible().findIndex((e) => e.id === anchor);
    const caret = at < 0 ? editor.value.length : codePointsToCaret(at + 1);
    editor.setSelectionRange(caret, caret);
  }

  // The textarea is closed while an edit is in flight.
  //
  // The server mints the character ids, so between sending an edit and seeing
  // its echo this client cannot give a new keystroke an id — it would live in
  // the textarea only, and the next redraw would drop it. Rather than guess,
  // the example waits: one round trip, and the box opens again.
  //
  // A production client does not wait. It runs the same RGA, mints its own
  // ids, and applies its edits locally the moment they are typed; the server
  // then merges rather than numbers. That is a client-side CRDT, which is
  // more than this example is for.
  function updateWritability() {
    editor.readOnly = !settled();
  }

  // Redraw from the local view, which includes this editor's own pending
  // characters as placeholders. Safe at any time: a remote character merges
  // in beside them rather than appearing to replace them.
  function render() {
    const anchor = caretAnchor();
    const next = text();
    if (editor.value !== next) {
      editor.value = next;
      restoreCaret(anchor);
    }
  }

  function renderRoster(participants) {
    if (!roster) return;
    roster.innerHTML = "";
    for (const person of participants) {
      const item = document.createElement("li");
      const where = person.cursor == null ? "" : ` @${person.cursor}`;
      item.textContent = `${person.label}${where}`;
      roster.appendChild(item);
    }
  }

  // Nothing can be sent before the socket is open and the snapshot has named
  // our actor, and a dropped message would leave the bookkeeping below
  // permanently out of step. Hold the editor closed until then.
  editor.disabled = true;

  const socket = new WebSocket(
    `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}${editor.dataset.socket}`,
  );

  socket.addEventListener("open", () => {
    if (status) status.textContent = "connected";
  });
  socket.addEventListener("close", () => {
    editor.disabled = true;
    if (status) status.textContent = "disconnected — reload to rejoin";
  });

  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    if (message.type === "snapshot") {
      elems = [];
      known.clear();
      waiting = [];
      // The snapshot is authoritative and already holds anything the server
      // accepted from us, so nothing is outstanding after it.
      provisional = [];
      unsentDeletes.clear();
      if (message.actor) myActor = message.actor;
      for (const element of message.elems) {
        elems.push({ id: element.id, ch: element.ch, deleted: !!element.deleted });
        known.add(element.id);
      }
      editor.disabled = false;
      render();
      updateWritability();
      renderRoster(message.participants);
    } else if (message.type === "ops") {
      for (const op of message.ops) {
        acknowledge(op);
        integrate(op);
      }
      render();
      updateWritability();
      flush();
      updateWritability();
    } else if (message.type === "presence") {
      renderRoster(message.participants);
    } else if (message.type === "error") {
      // The server refused an edit — a document at its limit, or a message it
      // could not read. Clear the bookkeeping for it: left counted, `settled`
      // would never come back true and the editor would freeze, sending
      // nothing and showing nobody else's changes again. The refused text is
      // dropped, so redraw from the authority to show what really happened.
      discardProvisional();
      render();
      updateWritability();
      if (status) status.textContent = message.message;
    }
  });

  function send(message) {
    if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(message));
  }

  // Turn "the textarea says this now" into character operations: keep the
  // common prefix and suffix, delete the middle, insert the replacement.
  //
  // The edit is not applied locally — the server mints the ids, echoes the
  // operations back, and `render()` puts them in. So a second keystroke inside
  // one round trip cannot be diffed yet: the document still lacks the first
  // one, and diffing against it would send that character twice. `flush`
  // therefore does nothing while an edit is outstanding; the `ops` handler
  // calls it again as soon as the echo lands, and the characters typed in
  // between go out together.
  function flush() {
    if (!settled()) return;

    const before = [...text()];
    const after = [...editor.value];

    let prefix = 0;
    while (prefix < before.length && prefix < after.length && before[prefix] === after[prefix]) {
      prefix += 1;
    }
    let suffix = 0;
    while (
      suffix < before.length - prefix &&
      suffix < after.length - prefix &&
      before[before.length - 1 - suffix] === after[after.length - 1 - suffix]
    ) {
      suffix += 1;
    }

    const shown = visible();
    const removed = shown.slice(prefix, before.length - suffix);
    if (removed.length > 0) {
      // Only real, server-known characters can be deleted. A placeholder has
      // no server id yet, so it is simply dropped locally.
      const ids = [];
      for (const element of removed) {
        if (element.provisional) {
          const at = indexOfId(element.id);
          if (at >= 0) {
            elems.splice(at, 1);
            known.delete(element.id);
          }
          provisional = provisional.filter((id) => id !== element.id);
        } else {
          element.deleted = true; // optimistic
          unsentDeletes.add(element.id);
          ids.push(element.id);
        }
      }
      if (ids.length > 0) send({ type: "delete", ids });
    }

    const added = after.slice(prefix, after.length - suffix);
    if (added.length > 0) {
      // Anchor to the element left of the insertion point in the full list,
      // tombstones included — that is the neighbour the server knows. A
      // placeholder cannot be an anchor: the server has never heard of it.
      let slot = prefix === 0 ? 0 : elems.indexOf(shown[prefix - 1]) + 1;
      let anchor = null;
      for (let i = slot - 1; i >= 0; i--) {
        if (!elems[i].provisional) {
          anchor = elems[i].id;
          break;
        }
      }
      send({ type: "insert", after: anchor, text: added.join("") });

      // Splice the characters in locally so the textarea and `elems` agree.
      // A remote character arriving before the echo then merges in beside
      // them, rather than looking like the user deleted it.
      for (const ch of added) {
        const id = `local-${(provisionalSeq += 1)}`;
        elems.splice(slot, 0, { id, ch, deleted: false, provisional: true });
        known.add(id);
        provisional.push(id);
        slot += 1;
      }
    }
  }

  editor.addEventListener("input", () => {
    flush();
    updateWritability();
  });

  const reportCaret = () =>
    send({ type: "cursor", index: caretToCodePoints(editor.selectionStart) });
  editor.addEventListener("keyup", reportCaret);
  editor.addEventListener("click", reportCaret);
})();
