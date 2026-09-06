import type { NewNote, Note } from "./types";

/** Shape of a GraphQL-over-HTTP response body (spec §Response). */
interface GraphqlResponse<T> {
  data?: T;
  errors?: { message: string; path?: (string | number)[] }[];
}

export class GraphqlError extends Error {
  constructor(messages: string[]) {
    super(messages.join("; "));
    this.name = "GraphqlError";
  }
}

/**
 * Minimal typed GraphQL client: one `fetch` per operation, same-origin, no
 * caching layer. The endpoint is served by the Autumn `GraphqlPlugin`
 * (see `../../src/graphql_plugin.rs`); under `npm run dev` Vite proxies it
 * to the Rust server.
 */
export async function graphql<T>(
  query: string,
  variables: Record<string, unknown> = {},
): Promise<T> {
  const response = await fetch("/graphql", {
    method: "POST",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify({ query, variables }),
  });
  if (!response.ok) {
    throw new GraphqlError([`HTTP ${response.status} from /graphql`]);
  }
  const payload = (await response.json()) as GraphqlResponse<T>;
  if (payload.errors && payload.errors.length > 0) {
    throw new GraphqlError(payload.errors.map((e) => e.message));
  }
  if (payload.data === undefined) {
    throw new GraphqlError(["response carried neither data nor errors"]);
  }
  return payload.data;
}

const NOTE_FIELDS = `fragment NoteFields on Note { id title body pinned createdAt }`;

export async function listNotes(): Promise<Note[]> {
  const data = await graphql<{ notes: Note[] }>(
    `${NOTE_FIELDS} query Notes { notes { ...NoteFields } }`,
  );
  return data.notes;
}

export async function createNote(input: NewNote): Promise<Note> {
  const data = await graphql<{ createNote: Note }>(
    `${NOTE_FIELDS} mutation CreateNote($input: NewNoteInput!) { createNote(input: $input) { ...NoteFields } }`,
    { input },
  );
  return data.createNote;
}

export async function togglePinned(id: number): Promise<Note> {
  const data = await graphql<{ togglePinned: Note }>(
    `${NOTE_FIELDS} mutation TogglePinned($id: Int!) { togglePinned(id: $id) { ...NoteFields } }`,
    { id },
  );
  return data.togglePinned;
}

export async function deleteNote(id: number): Promise<boolean> {
  const data = await graphql<{ deleteNote: boolean }>(
    `mutation DeleteNote($id: Int!) { deleteNote(id: $id) }`,
    { id },
  );
  return data.deleteNote;
}
