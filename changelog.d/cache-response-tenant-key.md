### Security

- **cache:** `CacheResponseLayer` now folds the resolved tenant into its key.
  Under subdomain or header tenancy, two tenants request the same URI, and the
  layer used to serve the first tenant's stored response to the second, without
  running the handler. An app without tenancy, or a route with no tenant in
  scope, keeps the old `http:<uri>` key and its warm entries
  (`docs/security/2026-11-28-cache-response-tenant-key/`).
