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
//   3. Show a character the moment it is typed, as a placeholder, and queue
//      the message that tells the server about it. The editor stays open
//      while an edit is in flight, so no keystroke is lost.

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
   * Placeholders for characters this editor sent, waiting for the server to
   * mint their real ids. One entry per sent character, oldest first; the
   * server answers our messages in order, so the oldest entry matches the next
   * echo. A placeholder the queue still holds is in `elems` but not here.
   * Holding them in `elems` is what lets a remote character land beside them
   * instead of appearing to replace them.
   */
  let provisional = [];
  let provisionalSeq = 0;
  /** Delete ids sent but not yet echoed back. */
  const sentDeletes = new Set();
  /** Delete ids the queue holds, for characters the server already knows. */
  const queuedDeletes = [];
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
      // Hold it until its cause arrives — but only once. A server that
      // re-sent an operation whose cause never came would otherwise grow this
      // buffer on every delivery, and this list is only ever drained by
      // something arriving.
      const key = JSON.stringify(op);
      if (!waiting.some((held) => JSON.stringify(held) === key)) {
        waiting.push(op);
      }
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
    return provisional.length === 0 && sentDeletes.size === 0;
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
            // Erased while it was in flight. The server names the character
            // here, which is the id the delete was waiting for.
            if (elems[at].doomed) queuedDeletes.push(op.id);
            elems.splice(at, 1);
            known.delete(placeholder);
          }
        }
      }
    } else {
      sentDeletes.delete(op.target);
    }
  }

  // Drop every placeholder, queued or in flight, and un-delete anything the
  // server refused. Used when the server rejects an edit, and when a snapshot
  // resets the world.
  function discardProvisional() {
    const restore = [...sentDeletes, ...queuedDeletes];
    elems = elems.filter((element) => {
      if (!element.provisional) return true;
      // A queued `replace` carries the ids it removes. They stay if the
      // message that would have removed them never goes out.
      for (const id of element.replaces ?? []) restore.push(id);
      known.delete(element.id);
      return false;
    });
    provisional = [];
    for (const id of restore) {
      const at = indexOfId(id);
      if (at >= 0) elems[at].deleted = false;
    }
    sentDeletes.clear();
    queuedDeletes.length = 0;
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
      sentDeletes.clear();
      queuedDeletes.length = 0;
      if (message.actor) myActor = message.actor;
      for (const element of message.elems) {
        elems.push({ id: element.id, ch: element.ch, deleted: !!element.deleted });
        known.add(element.id);
      }
      // Operations the server holds but cannot place yet. They are broadcast
      // when they arrive, not when they integrate, so an editor who joins
      // after one was buffered hears of it here or never. `integrate` puts
      // each in the same waiting list the server keeps it in.
      for (const op of message.pending ?? []) {
        integrate(op);
      }
      editor.disabled = false;
      render();
      renderRoster(message.participants);
    } else if (message.type === "ops") {
      for (const op of message.ops) {
        acknowledge(op);
        integrate(op);
      }
      // A character erased while in flight is real now. Hide it until its
      // delete lands.
      for (const id of queuedDeletes) {
        const at = indexOfId(id);
        if (at >= 0) elems[at].deleted = true;
      }
      render();
      pump();
    } else if (message.type === "presence") {
      renderRoster(message.participants);
    } else if (message.type === "error") {
      // The server refused an edit — a document at its limit, or a message it
      // could not read. Clear the bookkeeping for it: left counted, `settled`
      // would never come back true, the queue would never drain, and this
      // editor would send nothing again. The refused text is dropped, so
      // redraw from the authority to show what really happened.
      discardProvisional();
      render();
      if (status) status.textContent = message.message;
    }
  });

  function send(message) {
    if (socket.readyState === WebSocket.OPEN) socket.send(JSON.stringify(message));
  }

  // Turn "the textarea says this now" into character operations: keep the
  // common prefix and suffix, delete the middle, insert the replacement.
  //
  // The characters go into `elems` at once, as placeholders. The model and the
  // textarea therefore always hold the same text, and a redraw can never drop
  // a keystroke. What waits for the round trip is the message, not the
  // character: `pump` sends it when the edit before it is echoed.
  function capture() {
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
    // Held until we know whether an insert rides along with them.
    let replaced = [];
    for (const element of shown.slice(prefix, before.length - suffix)) {
      if (!element.provisional) {
        element.deleted = true; // optimistic
        replaced.push(element.id);
      } else if (provisional.includes(element.id)) {
        // In flight, so the server will name it whatever we do now. Hide it
        // and mark it; `acknowledge` turns the mark into a delete.
        element.deleted = true;
        element.doomed = true;
      } else {
        // Still in the queue. It was never sent, so it is simply dropped.
        const at = indexOfId(element.id);
        if (at >= 0) {
          elems.splice(at, 1);
          known.delete(element.id);
        }
      }
    }

    const added = after.slice(prefix, after.length - suffix);
    if (added.length === 0) {
      // A pure delete: nothing is waiting on it, so queue it as it is.
      queuedDeletes.push(...replaced);
      return;
    }

    let slot = prefix === 0 ? 0 : elems.indexOf(shown[prefix - 1]) + 1;
    for (const ch of added) {
      const id = `local-${(provisionalSeq += 1)}`;
      const cell = { id, ch, deleted: false, provisional: true };
      // The ids this edit removes ride with the first character it adds, so
      // the two go out as one `replace` — typing over a selection. Two
      // messages let the delete land while the insert is refused for a
      // document at its limit, which is the editor destroying the text it was
      // asked to replace. `replace` is refused whole or not at all.
      if (replaced.length > 0) {
        cell.replaces = replaced;
        replaced = [];
      }
      elems.splice(slot, 0, cell);
      known.add(id);
      slot += 1;
    }
  }

  // Send what the queue holds: one message per run of queued characters, then
  // one for the queued deletes.
  //
  // The server mints the character ids, so this client cannot name a keystroke
  // itself. It does not refuse the keystroke — it holds the message until the
  // edit before it is echoed. Nothing is in flight at that moment, so the
  // character to the left of a run is one the server knows, which is the
  // anchor an insert needs. A run goes out whole, so the server chains its ids
  // and keeps the characters in the order they were typed.
  //
  // A production client does not queue. It runs the same RGA, mints its own
  // ids, and applies its edits locally the moment they are typed; the server
  // then merges rather than numbers. That is a client-side CRDT, which is
  // more than this example is for.
  function pump() {
    if (!settled()) return;

    let i = 0;
    while (i < elems.length) {
      if (!elems[i].provisional) {
        i += 1;
        continue;
      }
      const ids = [];
      let typed = "";
      let end = i;
      while (end < elems.length && elems[end].provisional) {
        for (const id of elems[end].replaces ?? []) ids.push(id);
        delete elems[end].replaces;
        typed += elems[end].ch;
        provisional.push(elems[end].id);
        end += 1;
      }
      // Anchor to the element left of the run in the full list, tombstones
      // included — that is the neighbour the server knows.
      const anchor = i === 0 ? null : elems[i - 1].id;
      if (ids.length > 0) {
        send({ type: "replace", ids, after: anchor, text: typed });
        for (const id of ids) sentDeletes.add(id);
      } else {
        send({ type: "insert", after: anchor, text: typed });
      }
      i = end;
    }

    if (queuedDeletes.length > 0) {
      send({ type: "delete", ids: queuedDeletes });
      for (const id of queuedDeletes) sentDeletes.add(id);
      queuedDeletes.length = 0;
    }
  }

  editor.addEventListener("input", () => {
    capture();
    pump();
  });

  const reportCaret = () =>
    send({ type: "cursor", index: caretToCodePoints(editor.selectionStart) });
  editor.addEventListener("keyup", reportCaret);
  editor.addEventListener("click", reportCaret);
})();
