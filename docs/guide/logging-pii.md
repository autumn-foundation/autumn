# Logging: log levels, format, and PII scrubbing

This page is the one place Autumn's logging is configured. It answers four
questions, in the order people usually arrive with them:

- [how loud the logs are](#set-the-log-level) — `[log] level`
- [what shape they come out in](#choose-the-log-format-pretty-or-json) —
  `[log] format`, pretty or JSON
- [how to turn up a log level in production without a
  redeploy](#change-log-levels-at-runtime-without-a-restart) —
  `PUT /actuator/loggers/{name}`
- [what is kept out of them](#scrub-pii-from-logs) — the parameter scrubber

The [access log](#access-log) — one structured line per served request — is on
by default and is configured here too.

## Set the log level

The global level lives in `[log] level`, and defaults to `info`:

```toml
[log]
level = "info"
```

The five levels are `trace`, `debug`, `info`, `warn` and `error`.

For a deployment with no `autumn.toml` — a container, a platform that only
hands you environment variables — the same field is `AUTUMN_LOG__LEVEL`:

```bash
AUTUMN_LOG__LEVEL=debug cargo run
```

Either spelling is read once, at startup. To change a level on a process that
is already running, see [Change log levels at
runtime](#change-log-levels-at-runtime-without-a-restart) below.

### Turn on debug logging for one target

The field takes the full [`tracing` filter
syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html),
not just a bare level, so one target can be turned up without raising the floor
for everything else — which is usually what "turn on debug logging" should
mean, since a global `debug` on a busy service buries the lines you came for:

```toml
[log]
level = "info,autumn_web=debug,tower_http=trace"
```

The same syntax works in the environment variable:
`AUTUMN_LOG__LEVEL="info,my_app::orders=debug"`.

## Choose the log format (pretty or JSON)

`[log] format` decides whether log lines are rendered for a human reading a
terminal or for a collector parsing JSON:

```toml
[log]
format = "Auto"
```

| Format   | Behavior                                                   |
|----------|------------------------------------------------------------|
| `Auto`   | Pretty in development, JSON when the profile is production |
| `Pretty` | Always human-readable, colorized                           |
| `Json`   | Always structured JSON                                     |

`Auto` is the default, and is why the same binary prints readable lines on a
laptop and JSON in production without the config changing. Set `Json`
explicitly when something parses the output in development too — a local log
shipper, a test that asserts on fields. The environment spelling is
`AUTUMN_LOG__FORMAT=Json`.

The format applies to every line the standard subscriber renders, the [access
log](#access-log) included.

## Change log levels at runtime, without a restart

`[log] level` is read at startup, so raising verbosity to investigate
something in production would normally mean a redeploy — by which time the
thing you wanted to see has usually stopped happening. `PUT
/actuator/loggers/{name}` changes the live `tracing` subscriber instead, and
takes effect on the next event:

```bash
# Raise the global level
curl -X PUT http://localhost:3000/actuator/loggers/root \
  -H 'content-type: application/json' -d '{"level":"debug"}'

# Raise one target, leaving everything else at its configured level
curl -X PUT http://localhost:3000/actuator/loggers/my_app::orders \
  -H 'content-type: application/json' -d '{"level":"trace"}'

# Put it back
curl -X PUT http://localhost:3000/actuator/loggers/root \
  -H 'content-type: application/json' -d '{"level":"info"}'
```

`{name}` is either `root` (the global level) or a `tracing` target — a module
path such as `my_app::orders`. `GET /actuator/loggers` reports what is in
force:

```json
{
  "current_level": "info",
  "available_levels": ["trace", "debug", "info", "warn", "error"],
  "loggers": { "my_app::orders": "trace" }
}
```

Four things are worth knowing before you rely on this in an incident:

- **Overrides are ephemeral.** They live in the running process. A restart,
  a redeploy or a replacement replica is back at the configured `[log] level`.
  Nothing here edits `autumn.toml`.
- **`applied` is the field to check, not the status code.** A successful
  change answers `"status": "ok"` with `"applied": true`. If the app was built
  with a subscriber that cannot be reloaded, the change is remembered but
  never reaches the log stream, and the response says so —
  `"status": "recorded"`, `"applied": false` — rather than reporting a
  false-positive `ok`. Both are `200`; only `applied` distinguishes them.
- **A bad level or a bad target is a `400`.** Levels outside the five above
  are rejected, and so are target names carrying `EnvFilter` metacharacters
  (`=`, `,`, `[`, `]`, `{`, `}`, whitespace), so a malformed directive can
  never reach the subscriber.
- **The endpoint is mounted only in sensitive actuator mode.** It can change
  what a production process logs, so it lives behind the same switch as
  `/actuator/env` and `/actuator/configprops`:

  ```toml
  [actuator]
  sensitive = true
  ```

  A `404` on `/actuator/loggers` means the profile has not enabled it, not
  that the path is wrong. See
  [Deployment](deployment.md) for what sensitive mode exposes and how to keep
  it reachable only from inside your network.

## Access log

Every served HTTP request emits one structured access-log line by default
(`tracing` target `autumn::access`, level `INFO`) carrying `method`, `route`
(the matched low-cardinality template, e.g. `/users/{id}` — never the raw
path), `status`, `duration_ms`, and `request_id` (the same id as the
`x-request-id` header and error pages). It renders through the standard
subscriber, so `log.format` controls its shape, and it requires no telemetry
feature or collector.

The line never includes query strings, headers, or bodies, so it cannot leak
the sensitive values the [parameter scrubber](#scrub-pii-from-logs) protects.

Probe and asset noise is excluded by default; both knobs live in `[log]`:

```toml
[log]
# On by default; set to false to silence the access log without recompiling.
access_log = true

# Path prefixes to skip (whole-segment match; replaces the default set:
# "/health", "/live", "/ready", "/startup", "/actuator", "/static").
access_log_exclude = ["/health", "/actuator", "/static", "/uptime-probe"]
```

Both knobs also honor environment overrides for TOML-less deployments:
`AUTUMN_LOG__ACCESS_LOG=false` and
`AUTUMN_LOG__ACCESS_LOG_EXCLUDE=/health,/internal` (comma-separated).

## Scrub PII from logs

Autumn includes a parameter scrubber for structured payloads. Today, it is wired
into dev HTML error-badge request context rendering (headers/query) and helper APIs.
It is **not yet globally applied to every tracing/log event payload**.

### Built-in defaults

By default, the scrubber filters keys such as:

- `password`, `password_confirmation`
- `token`, `access_token`, `refresh_token`
- `secret`, `authorization`
- `api_key`
- `cookie`, `set-cookie`
- `ssn`, `credit_card`, `card_number`, `cvv`

Matched values are replaced with:

```text
[FILTERED]
```

### Add or remove scrubbed keys

Both lists live in `[log]`, beside the level and format above:

```toml
[log]
# Add app-specific sensitive keys
filter_parameters = ["pin", "private_note"]

# Opt out of built-in defaults (use sparingly)
unfilter_parameters = ["password"]
```

### Important behavior

- Matching is case-insensitive.
- Matching is normalization-aware for separators/casing (`api_key`, `apiKey`,
  `API-KEY`, `apikey` are treated equivalently).
- Empty custom keys are ignored to avoid accidental “scrub everything”.

### Startup warnings

If you opt out of built-in sensitive defaults via `unfilter_parameters`, Autumn
emits a startup warning listing the opted-out keys.

### Programmatic use

```rust
use autumn_web::log::filter::scrub;
use serde_json::json;

let payload = json!({
    "email": "user@example.com",
    "password": "secret"
});

let scrubbed = scrub(&payload);
assert_eq!(scrubbed["password"], "[FILTERED]");
```

## In-memory log capture (`/actuator/logfile`)

Autumn can buffer recent structured log entries in memory and expose them via the
`/actuator/logfile` endpoint — useful for inspecting application log output without
SSH access or an external aggregator.

### Enabling

```toml
[log.capture]
enabled  = true   # default: false
capacity = 1000   # max entries retained (ring buffer; default: 1000)
```

The endpoint requires the sensitive actuator to be enabled:

```toml
[actuator]
sensitive = true   # required; always on in the "dev" profile
```

### Querying

```
GET /actuator/logfile
GET /actuator/logfile?level=warn
GET /actuator/logfile?level=error&limit=50
```

| Parameter | Description |
|-----------|-------------|
| `level`   | Minimum severity to return: `trace`, `debug`, `info`, `warn`, or `error` (case-insensitive). Returns `400 Bad Request` for unrecognised values. Omit to return all levels. |
| `limit`   | Cap the response to the most-recent *N* entries. Omit to return all retained entries. |

Results are returned in chronological order (oldest first), newest-last.

### Response shape

```json
{
  "capture_enabled": true,
  "total": 312,
  "entries": [
    {
      "timestamp": "2026-01-15T12:34:56.789Z",
      "level": "INFO",
      "target": "myapp::orders",
      "message": "order placed",
      "fields": { "order_id": "A-1001", "user_id": "42" },
      "request_id": "req-abc123"
    }
  ]
}
```

When `log.capture.enabled = false` (the default), the endpoint still responds with
`200` and `"capture_enabled": false` so API consumers can handle the case uniformly.

### Request context fields

When a log event is emitted inside a request, the capture layer automatically
includes `request_id`, `user_id`, `tenant_id`, and any custom fields set via
`LogContext` (e.g. `ctx.set_user_id("42")` or `ctx.insert_field("region", "eu-1")`).
Fields on the tracing event take priority over the same key from the request context.

### Security

The capture buffer uses the same scrubber as the rest of the logging pipeline:

- Sensitive field values (passwords, tokens, SSNs, …) are replaced with
  `[FILTERED]` **before** storage — they never enter the buffer.
- If your app uses `#[model]` encrypted columns, their names are automatically
  added to the scrubber so plaintext values are filtered even if not listed in
  `log.filter_parameters`.
- The endpoint is only reachable when `actuator.sensitive = true` (off by default
  in production profiles).

