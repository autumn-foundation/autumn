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

The global level lives in `[log] level`:

```toml
[log]
level = "info"
```

**The effective default depends on the profile**, which is the usual answer to
"why is dev so noisy and prod so quiet":

| Profile | Default `level` |
|---------|-----------------|
| `dev`   | `debug`         |
| `prod`  | `info`          |
| any other profile (`staging`, `test`, a custom name) | `info` |

`dev` and `prod` are the only profiles with smart defaults; anything else falls
back to the struct default, `info`. Whatever you write in `autumn.toml` — or
pass in the environment — overrides the profile's default, so the fence above
pins `info` in dev too.

The levels are `trace`, `debug`, `info`, `warn`, `error` and `off` — `off`
silences the subscriber entirely. It is valid here, at startup, and only
here: the [runtime endpoint](#change-log-levels-at-runtime-without-a-restart)
takes the five named levels and rejects `off` with a `400`.

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
| `Auto`   | Pretty unless the profile is `prod`/`production` (or the environment says production), then JSON |
| `Pretty` | Always human-readable, colorized                           |
| `Json`   | Always structured JSON                                     |

As with the level, the profile picks the default: `dev` defaults to `Pretty`
and `prod` to `Json`, both set outright, so the same binary reads well on a
laptop and parses in production without the config changing. `Auto` is the
struct default and is what any other profile gets — it reaches the same
outcome by looking at the profile at startup. Set `Json` explicitly when
something parses the output in development too: a local log shipper, a test
that asserts on fields. The environment spelling is `AUTUMN_LOG__FORMAT=Json`.

The format applies to every line the standard subscriber renders, the [access
log](#access-log) included.

## Change log levels at runtime, without a restart

`[log] level` is read at startup, so raising verbosity to investigate
something in production would normally mean a redeploy — by which time the
thing you wanted to see has usually stopped happening. `PUT
/actuator/loggers/{name}` changes the live `tracing` subscriber instead, and
takes effect on the next event:

```bash
LOGGERS=http://localhost:3000/actuator/loggers

# Raise the global level, KEEPING what it was. Do not assume `info`: the level
# in force is whatever the profile or config set, and once you have replaced
# it the response is the only place it still exists.
PREV=$(curl -sX PUT "$LOGGERS/root" \
  -H 'content-type: application/json' -d '{"level":"debug"}' | jq -r .previous)

# Raise one target, leaving everything else where it is
curl -sX PUT "$LOGGERS/my_app::orders" \
  -H 'content-type: application/json' -d '{"level":"trace"}'

# … investigate …

# Put the global level back to exactly what it was — but only if it is a
# level this endpoint accepts. See below for when it is not.
case "$PREV" in
  trace|debug|info|warn|error)
    curl -sX PUT "$LOGGERS/root" \
      -H 'content-type: application/json' -d "{\"level\":\"$PREV\"}" ;;
  *) echo "cannot restore '$PREV' over HTTP — restart to clear it" ;;
esac
```

(`jq` only to pull one field out; any JSON reader will do, and `GET
$LOGGERS` shows `current_level` if you would rather read it by eye first.)

That `case` is not defensive padding. **The endpoint accepts only the five
named levels, and `previous` can hold something outside them** — so there are
configurations this API cannot put back at all:

| What `previous` holds | When | Restorable over HTTP? |
|---|---|---|
| `trace`/`debug`/`info`/`warn`/`error` | the usual case | yes |
| `""` | `[log] level` names only targets (`"my_app=debug"`), so there is no global directive | **no** — and there is no way to set it back to "unset" either |
| `off` | `[log] level = "off"` | **no** — `off` is valid at startup and rejected by this endpoint |

The last two both mean the same thing in practice: **restart to clear it.**
Guarding on the accepted set rather than on "is it empty" is deliberate —
`off` was the second case to turn up this way, and a membership test covers
whatever the third would have been.

**`previous` is the whole point of that first response.** Every `PUT` returns
the level that target had before the change, and it is the only record of what
to restore:

```json
{ "status": "ok", "message": "Logger 'root' set to 'debug'",
  "previous": "info", "applied": true }
```

For `root`, `previous` is the global level you just replaced. For a target,
it is that target's previous override — or `null`, which is the case worth
understanding before an incident rather than during one:

- **A target that had an override** (one named in `[log] level` at startup,
  like the `tower_http` in `level = "info,tower_http=warn"`) goes back by
  setting it to that `previous` value — `warn` here, not the global level.
- **A target that had none** — `"previous": null` — takes two steps, and the
  first one is not optional. You raised it from the global level to `trace`,
  so **it is still emitting at `trace` until you lower it**. Set it back to
  the level you want it at, normally the global one:

  ```bash
  case "$PREV" in
    trace|debug|info|warn|error)
      curl -sX PUT "$LOGGERS/my_app::orders" \
        -H 'content-type: application/json' -d "{\"level\":\"$PREV\"}" ;;
    *) echo "cannot lower this target over HTTP — restart now" ;;
  esac
  ```

  That stops the volume immediately. What it cannot do is remove the override:
  there is no "remove" call, so the target stays *pinned* at that level and
  will not follow later changes to `root`. Only a restart clears the pin,
  since overrides live in the process.

  The same membership guard, and for a sharper reason than on the global
  restore. Whenever `$PREV` is outside the five — empty, or `off` — there is
  **no level you can send that restores this target**, so it stays at `trace`
  until the process restarts, and the advice below hardens accordingly:
  **restart now, not when convenient.**

So the honest summary depends on which case you are in:

- **`previous` had a value** — the usual case. **Lower it now, restart when
  convenient.** Lowering stops a trace-level firehose, potentially
  high-volume and on a busy target full of request detail, from running on
  after the investigation is closed. The restart is only the tidy-up that
  stops a later `root` change from silently missing that target.
- **`previous` was `null`, empty, or `off`** — **restart now.** The guarded
  command above does not fire, because there is no level it could send, so
  nothing has lowered the target and it is still at `trace`. Here the restart
  is not tidy-up; it is the only thing that stops the volume.

`GET /actuator/loggers` lists everything currently overridden, and is the
check worth running before you call the incident closed — not least because it
is the only way to notice a target someone else raised.

Those run as shown in development. **In production they need a CSRF token**,
and the failure is a `403` that never reaches the handler — see [Getting a
`PUT` past CSRF](#getting-a-put-past-csrf) below before you need this at 3am.

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

  A `404` on `/actuator/loggers` usually means the profile has not enabled it
  — but check the prefix before you go changing `sensitive`, since a wrong
  path 404s identically. See
  [Deployment](deployment.md) for what sensitive mode exposes and how to keep
  it reachable only from inside your network.

  ```toml
  [actuator]
  prefix = "/actuator"
  ```

  Every path on this page assumes that default. If your app sets
  `[actuator] prefix = "/ops"`, the endpoint is `/ops/loggers` and
  `/ops/loggers/{name}`, and the CSRF exemption below has to name `/ops/`
  too — an `exempt_paths` entry still pointing at `/actuator/` matches
  nothing and leaves the `PUT` returning `403`.

### Getting a `PUT` past CSRF

The `prod` profile turns CSRF on (`[security.csrf] enabled = true`, a smart
default), `PUT` is not one of the safe methods, and the actuator carries no
built-in exemption. So the bare `curl` above is a `403` in production: the
CSRF layer wraps the whole router, the actuator endpoints included, so the
request is rejected before `loggers_put` ever runs. In development, where CSRF is off by default, it works as
written, which is exactly how this is discovered at the worst moment.

Two ways through, and which one you want is a standing decision to make before
an incident, not during one.

**Exempt the actuator prefix.** `[security.csrf] exempt_paths` exists for
management and API paths that authenticate with something other than a cookie:

```toml
[security.csrf]
exempt_paths = ["/actuator/"]
```

(Or whatever `[actuator] prefix` is set to — the two have to agree.)

CSRF defends against a browser being made to send a request with the user's
ambient cookies. An actuator reached over an internal network, with no
cookie-based session in play, is not that threat — which is why exempting it
is a reasonable posture and not a hole. It is only reasonable if the actuator
is *actually* unreachable from the public internet, so make this change
together with the network restriction in
[Deployment](deployment.md), never instead of it.

**Or send the token.** CSRF here is double-submit: the cookie value and the
`X-CSRF-Token` header must match. Any `GET` through the same origin mints the
cookie, so pick one up and send it back:

```bash
# Mint the cookie (any GET will do) and keep it in a jar
curl -c /tmp/jar -s -o /dev/null http://localhost:3000/actuator/health

# Resend it as both cookie and header
TOKEN=$(awk '/autumn-csrf/ {print $7}' /tmp/jar)
curl -X PUT http://localhost:3000/actuator/loggers/root \
  -b /tmp/jar -H "X-CSRF-Token: $TOKEN" \
  -H 'content-type: application/json' -d '{"level":"debug"}'
```

`autumn-csrf` and `X-CSRF-Token` are the default cookie and header names
(`[security.csrf] cookie_name` and `token_header` if you have changed them).

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

