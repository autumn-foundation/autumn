# Server Connection Limits

The `[server.http]` section limits slow, idle and excess connections. It
protects the server from slowloris attacks and connection floods.

Every key is optional. When a key is not set, the server applies no limit
for it, or the hyper default. The `prod` profile sets the values below.
Other profiles set none.

```toml
[server.http]
header_read_timeout_ms = 10_000      # full request head within 10 s
keep_alive_timeout_ms = 75_000       # close a connection idle for 75 s
max_header_bytes = 65_536            # 431 above 64 KiB
http2_max_concurrent_streams = 100   # per HTTP/2 connection
max_connections = 10_000             # open connections per listener
```

| Key | Effect | Off |
|-----|--------|-----|
| `header_read_timeout_ms` | Disconnects a client that does not send a full request head in time. The timer starts when the connection opens, or at the first byte after an idle period. More bytes do not restart it. If it is not set, `keep_alive_timeout_ms` limits the wait for a head. | unset or `0` |
| `keep_alive_timeout_ms` | Closes a connection that has no request in flight for this long. | unset or `0` |
| `max_header_bytes` | HTTP/1 requests with a larger head get `431`. Also sets the HTTP/2 header list limit. Must be `8192` or more. | cannot be turned off; unset is about 400 KiB (HTTP/1) and 16 KiB (HTTP/2) |
| `http2_max_concurrent_streams` | The stream limit the server sends in its HTTP/2 `SETTINGS`. Must be `1` or more. | cannot be turned off; unset is 200 |
| `max_connections` | At the limit, the server does not accept new connections. The kernel queues new connections until one closes. A WebSocket or SSE stream counts until it closes. The count is per listener. On HTTPS, a connection counts after its TLS handshake. | unset or `0` |

The limits apply to the TCP, Unix socket and HTTPS listeners. They also apply
to the ACME `:80` challenge listener. The timers also run during a graceful
drain, so a slow client cannot hold the drain open.

The environment variables are `AUTUMN_SERVER__HTTP__HEADER_READ_TIMEOUT_MS`,
`AUTUMN_SERVER__HTTP__KEEP_ALIVE_TIMEOUT_MS`,
`AUTUMN_SERVER__HTTP__MAX_HEADER_BYTES`,
`AUTUMN_SERVER__HTTP__HTTP2_MAX_CONCURRENT_STREAMS` and
`AUTUMN_SERVER__HTTP__MAX_CONNECTIONS`.

## What the timers do not limit

- A request that is in flight. Use `[server.timeouts] request_timeout_ms` for
  slow handlers. See [Middleware](middleware.md).
- A slow request body. The request timeout bounds it.
- An established WebSocket. Use the [`[realtime]` limits](websockets.md#limits).

## Select values

- Set `header_read_timeout_ms` above the slowest real client. Mobile clients
  on bad networks can need several seconds.
- Set `keep_alive_timeout_ms` above the idle timeout of your load balancer.
  Then the load balancer closes idle connections first. Thus, no request goes
  to a closed connection. The AWS ALB idle timeout is 60 s. Thus the `prod`
  profile uses 75 s.
- Set `max_connections` below the process file descriptor limit
  (`ulimit -n`). Keep space for database and outbound connections. On HTTPS,
  up to 1280 more connections can wait in the TLS handshake queue.

## Related

- [Load shedding](resilience.md) limits requests in flight, not connections.
- [WebSockets](websockets.md#limits) has the `[realtime]` limits.
