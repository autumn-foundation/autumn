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

  async function onTogglePinned(id: number) {
    try {
      const updated = await togglePinned(id);
      setNotes((current) => current.map((n) => (n.id === id ? updated : n)));
      setStatus({ kind: "ready" });
    } catch (err) {
      setStatus({ kind: "error", message: (err as Error).message });
    }
  }

  async function onDelete(id: number) {
    try {
      if (await deleteNote(id)) {
        setNotes((current) => current.filter((n) => n.id !== id));
      }
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
        <button type="submit" disabled={submitting || title.trim() === ""}>
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
            <NoteList heading="Pinned" notes={pinned} onTogglePinned={onTogglePinned} onDelete={onDelete} />
          )}
          <NoteList
            heading={pinned.length > 0 ? "Everything else" : "All notes"}
            notes={unpinned}
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
  onTogglePinned: (id: number) => void;
  onDelete: (id: number) => void;
}

function NoteList({ heading, notes, onTogglePinned, onDelete }: NoteListProps) {
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
              <button type="button" onClick={() => onTogglePinned(note.id)}>
                {note.pinned ? "Unpin" : "Pin"}
              </button>
              <button type="button" className="danger" onClick={() => onDelete(note.id)}>
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
