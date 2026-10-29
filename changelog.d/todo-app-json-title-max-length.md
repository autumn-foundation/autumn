### Fixed

- **todo-app:** `POST /api/todos` and the `create_json` MCP tool now reject
  titles over 255 characters with `422`, like the HTML form (issue #2972).
