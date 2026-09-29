### Added

- **cli:** `autumn routes --format postman` (PR #1567) [no-plugin] — exports the app's
  route table as a Postman Collection v2.1.0 JSON document, ready to import
  directly into Postman or Insomnia. Axum path captures like `/users/{id}`
  and wildcards like `/static/{*path}` become Postman path variables (`:id`,
  `:path`) declared on each request, and requests are addressed against a
  `{{base_url}}` collection variable that defaults to `http://localhost:3000`.
