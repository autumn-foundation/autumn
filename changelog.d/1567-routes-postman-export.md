### Added

- **cli:** `autumn routes --format postman` (PR #1567) [no-plugin] — exports the app's
  route table as a Postman Collection v2.1.0 JSON document, ready to import
  directly into Postman or Insomnia. Axum path parameters like `/users/{id}`
  are translated to Postman's `/users/:id` syntax and requests are addressed
  against a `{{base_url}}` collection variable.
