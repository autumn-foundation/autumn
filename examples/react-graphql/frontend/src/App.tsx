import { useCallback, useEffect, useState, type FormEvent } from "react";
import { createNote, deleteNote, listNotes, togglePinned } from "./api";
import type { Note } from "./types";

type Status = { kind: "loading" } | { kind: "ready" } | { kind: "error"; message: string };

export function App() {
  const [notes, setNotes] = useState<Note[]>([]);
  const [status, setStatus] = useState<Status>({ kind: "loading" });
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [submitting, setSubmitting] = useState(false);
  // Notes with a mutation in flight. Their Pin/Delete buttons are disabled
  // meanwhile, so two toggles can never be outstanding at once: the server
  // serialises flips under a row lock, but two responses on separate
  // connections may still settle out of order, and the UI would apply the
  // stale one last.
  const [pending, setPending] = useState<Set<string>>(() => new Set());

  function withPending<T>(id: string, work: () => Promise<T>): Promise<T> {
    setPending((current) => new Set(current).add(id));
    return work().finally(() =>
      setPending((current) => {
        const next = new Set(current);
        next.delete(id);
        return next;
      }),
    );
  }

  const refresh = useCallback(async () => {
    try {
      setNotes(await listNotes());
      setStatus({ kind: "ready" });
    } catch (err) {
      setStatus({ kind: "error", message: (err as Error).message });
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  async function onSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (title.trim() === "") return;
    setSubmitting(true);
    try {
      const created = await createNote({ title: title.trim(), body: body.trim() });
      // The mutation returns the created row, so no refetch is needed.
      setNotes((current) => [created, ...current]);
      setTitle("");
      setBody("");
      setStatus({ kind: "ready" });
    } catch (err) {
      setStatus({ kind: "error", message: (err as Error).message });
    } finally {
      setSubmitting(false);
    }
  }

  async function onTogglePinned(id: string) {
    if (pending.has(id)) return;
    try {
      const updated = await withPending(id, () => togglePinned(id));
      setNotes((current) => current.map((n) => (n.id === id ? updated : n)));
      setStatus({ kind: "ready" });
    } catch (err) {
      setStatus({ kind: "error", message: (err as Error).message });
    }
  }

  async function onDelete(id: string) {
    if (pending.has(id)) return;
    try {
      // `false` means the row was already gone (another tab, a REST client):
      // either way it is absent now, so drop it locally rather than leave a
      // ghost that can never be removed.
      await withPending(id, () => deleteNote(id));
      setNotes((current) => current.filter((n) => n.id !== id));
      // A success clears any alert left by an earlier failed action (e.g.
      // "note is pinned" from a refused delete that has since been unpinned).
      setStatus({ kind: "ready" });
    } catch (err) {
      setStatus({ kind: "error", message: (err as Error).message });
    }
  }

  const pinned = notes.filter((n) => n.pinned);
  const unpinned = notes.filter((n) => !n.pinned);

  return (
    <main className="app">
      <header>
        <h1>Autumn Notes</h1>
        <p className="tagline">
          A React + TypeScript front end talking GraphQL to an Autumn backend.
        </p>
      </header>

      <form className="composer" onSubmit={onSubmit}>
        <label>
          Title
          <input
            name="title"
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            placeholder="What is this note about?"
            maxLength={120}
            required
          />
        </label>
        <label>
          Body
          <textarea
            name="body"
            value={body}
            onChange={(e) => setBody(e.target.value)}
            placeholder="Optional details"
            rows={3}
          />
        </label>
        {/* Disabled until the first load lands: a create that raced the
            initial fetch could otherwise be overwritten by its stale result. */}
        <button
          type="submit"
          disabled={submitting || status.kind === "loading" || title.trim() === ""}
        >
          {submitting ? "Saving…" : "Add note"}
        </button>
      </form>

      {status.kind === "error" && (
        <p role="alert" className="error">
          {status.message}
        </p>
      )}

      {status.kind === "loading" ? (
        <p className="muted">Loading notes…</p>
      ) : notes.length === 0 ? (
        <p className="muted" id="notes-empty">
          No notes yet — add one above.
        </p>
      ) : (
        <>
          <p className="muted" id="notes-count">
            {notes.length} {notes.length === 1 ? "note" : "notes"}
            {pinned.length > 0 && `, ${pinned.length} pinned`}
          </p>
          {pinned.length > 0 && (
            <NoteList
              heading="Pinned"
              notes={pinned}
              pending={pending}
              onTogglePinned={onTogglePinned}
              onDelete={onDelete}
            />
          )}
          <NoteList
            heading={pinned.length > 0 ? "Everything else" : "All notes"}
            notes={unpinned}
            pending={pending}
            onTogglePinned={onTogglePinned}
            onDelete={onDelete}
          />
        </>
      )}

      <footer>
        <p className="muted">
          Backend: <code>POST /graphql</code> · schema at{" "}
          <a href="/graphql/sdl">
            <code>/graphql/sdl</code>
          </a>{" "}
          · health at{" "}
          <a href="/health">
            <code>/health</code>
          </a>
        </p>
      </footer>
    </main>
  );
}

interface NoteListProps {
  heading: string;
  notes: Note[];
  pending: Set<string>;
  onTogglePinned: (id: string) => void;
  onDelete: (id: string) => void;
}

function NoteList({ heading, notes, pending, onTogglePinned, onDelete }: NoteListProps) {
  if (notes.length === 0) return null;
  return (
    <section>
      <h2>{heading}</h2>
      <ul className="notes">
        {notes.map((note) => (
          <li key={note.id} className={note.pinned ? "note pinned" : "note"} data-note-id={note.id}>
            <div className="note-head">
              <h3>{note.title}</h3>
              <time dateTime={note.createdAt}>{formatDate(note.createdAt)}</time>
            </div>
            {note.body !== "" && <p className="note-body">{note.body}</p>}
            <div className="note-actions">
              <button
                type="button"
                disabled={pending.has(note.id)}
                onClick={() => onTogglePinned(note.id)}
              >
                {note.pinned ? "Unpin" : "Pin"}
              </button>
              <button
                type="button"
                className="danger"
                disabled={pending.has(note.id)}
                onClick={() => onDelete(note.id)}
              >
                Delete
              </button>
            </div>
          </li>
        ))}
      </ul>
    </section>
  );
}

function formatDate(iso: string): string {
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? iso : date.toLocaleString();
}
