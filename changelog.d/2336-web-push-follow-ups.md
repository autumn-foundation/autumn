### Added

- **http-client:** `RequestBuilder::pin_to_addrs` pins one request to several
  addresses of a host (issue #2336). reqwest tries the next address only if it
  cannot connect. `pin_to` is no longer a `const fn`.

### Fixed

- **push:** nine Web Push follow-ups (issue #2336).
  - A `mailto:` VAPID subject with a space or a second `@` now fails at boot.
  - `WebPush::new` trims the subject, so the JWT `sub` matches what was checked.
  - A move to a full principal now returns `TooManySubscriptions` in the memory store.
  - `send_many` serves up to four principals at once. Every recipient finishes before an error returns.
  - One corrupt database row no longer stops a send. It is skipped and logged.
  - The transport sends one request over all checked addresses. It does not re-send a POST after a late failure.
  - The generated service worker opens the app root when a notification URL is malformed.
  - The generated `autumnPushUnsubscribe()` revokes the browser subscription even when server cleanup fails.
