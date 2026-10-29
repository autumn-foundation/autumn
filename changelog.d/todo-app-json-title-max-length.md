### Fixed

- **todo-app:** `POST /api/todos` and the `create_json` MCP tool now reject
  titles over 255 characters with `422`, like the HTML form (issue #2972).
  A blank title now returns the standard field-error body, not the old
  "Title must not be empty" message.
