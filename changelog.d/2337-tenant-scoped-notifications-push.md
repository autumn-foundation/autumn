### Breaking Changes

- **Breaking:** the `notifications` and `push_subscriptions` tables get a
  `tenant_id` column ([migration guide](docs/migrations/next.md)). Add the
  column and backfill it. `Notifications::topic` includes the tenant inside a
  tenant scope.

### Fixed

- **tenancy:** notifications and Web Push subscriptions are now tenant-scoped
  (issue #2337). Two tenants that use the same user id no longer share a feed,
  an unread count, a channel topic or push devices. The tenant comes from the
  resolved tenant of the request, never from the request body.
