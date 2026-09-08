// Hand-written mirror of `../../schema.graphql` — the SDL the backend
// publishes at `GET /graphql/sdl` and drift-tests against the committed file.
// Small enough to keep by hand; a larger schema would generate these with
// GraphQL Code Generator pointed at that endpoint.

export interface Note {
  /** The GraphQL `ID` scalar: a string on the wire, even though the column is a BIGINT. */
  id: string;
  title: string;
  body: string;
  pinned: boolean;
  createdAt: string;
}

// Wire name: `NewNoteInput` (the server keeps `NewNote` for its Diesel insert type).
export interface NewNote {
  title: string;
  body: string;
}
