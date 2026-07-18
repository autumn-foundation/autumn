# TLS & HTTPS

This guide covers serving your Autumn app over HTTPS. There are three ways to do
it, and which one you want depends on where TLS is terminated:

- **Direct in-process TLS** — the app terminates HTTPS itself on its own
  host:port using a certificate and key you supply. Best for a **single host
  where you already have a certificate** (from `certbot`, a corporate CA, or a
  cloud cert vendor).
- **Automatic ACME (Let's Encrypt)** — the app obtains and renews its own
  certificate over HTTP-01, with no static cert on disk and no proxy. Best for a
  **single host that should get and keep a valid public certificate
  automatically**.
- **Reverse-proxy / platform termination** — TLS is terminated in front of the
  app (kamal-proxy, nginx, a cloud load balancer) and the app serves plain HTTP
  behind it. Best for the **managed `autumn deploy` flow and multi-replica
  deployments**, and still the recommendation when a proxy already fronts your
  fleet.

> **Quick decision.** Own cert, single host → [Direct TLS](#direct-in-process-tls-servertls).
> Want auto-issued certs, single host, no proxy →
> [ACME](#automatic-acme-certificates-servertlsacme). Using `autumn deploy`, a
> proxy, or multiple replicas → [Reverse-proxy termination](#terminating-tls-at-a-reverse-proxy).

Direct TLS and ACME are both **off by default** and each gated behind an
off-by-default cargo feature (`tls` and `acme`), so a default build never links
the TLS stack. The reverse-proxy path needs neither feature — the app just
serves HTTP.

---

## Direct in-process TLS (`[server.tls]`)

With the `tls` cargo feature enabled and a `[server.tls]` section configured,
the app terminates HTTPS itself on the same host:port — no sidecar proxy.

Enable the feature in your app's `Cargo.toml`:

```toml
[features]
tls = ["autumn-web/tls"]
```

Then point the app at your PEM certificate and key:

```toml
[server]
host = "0.0.0.0"
port = 443

[server.tls]
cert_path = "/etc/letsencrypt/live/app.example.com/fullchain.pem"
key_path = "/etc/letsencrypt/live/app.example.com/privkey.pem"
# reload_interval_secs = 60      # certs hot-reload by polling file mtimes (default)
# handshake_timeout_secs = 10    # TLS handshake timeout (default)
```

The `[features]` block above only *defines* the forwarding feature — because
`tls` is off by default you must also build/run **with it enabled**, otherwise
the TLS stack is never linked:

- `cargo run --features tls` (or `cargo build --release --features tls`) — uses
  the `tls = ["autumn-web/tls"]` forwarding feature declared above.
- Or turn it on directly without the forwarding feature:
  `cargo run --features autumn-web/tls`.
- For a CLI-built single binary: `autumn build --embed --features tls` — the
  `--features` flag is forwarded to cargo through every build phase.

**Configuring `[server.tls]` without the `tls` feature compiled in is a
fail-fast boot error, not a silent fallback.** `AppBuilder::run` validates the
wiring before binding anything and exits non-zero with:

> `[server.tls] is configured but this binary was built without the `tls`
> feature; rebuild with `--features tls`, or remove [server.tls] to serve plain
> HTTP`

so a build that forgot the feature can never quietly serve plain HTTP on a port
you expect to be HTTPS.

Fields:

| Field | Default | Meaning |
|---|---|---|
| `cert_path` | required | PEM certificate chain (leaf first, then intermediates). |
| `key_path` | required | PEM private key for the leaf. |
| `reload_interval_secs` | `60` | How often the cert/key file mtimes are polled for a hot reload. |
| `handshake_timeout_secs` | `10` | Maximum time allowed for a TLS handshake. |

Once it is running you should get a valid HTTPS response with no proxy in front:

```bash
curl https://app.example.com/health   # -> {"status":"ok", ...}
```

### Fail-fast startup validation

The certificate is validated at startup, so a broken cert stops the boot instead
of silently serving an unusable listener. Startup fails fast on:

- a missing or unreadable `cert_path` / `key_path` file,
- an unparseable or empty PEM,
- a private key that does not match the certificate leaf,
- an expired or not-yet-valid leaf or intermediate.

### Hot reload (renewals without a restart)

The app polls the cert and key file mtimes every `reload_interval_secs` (default
60s) and swaps in the new material when either file changes. A certificate
renewal — from `certbot`, an ACME client, or any tool that rewrites the PEM
files in place — is picked up **without a restart and without dropping the
site**.

### The `autumn doctor` TLS check

`autumn doctor` gains a `tls` check that inspects the configured certificate: it
**fails** on a missing, invalid, or expired certificate and **warns** when the
leaf expires within 30 days — so an approaching expiry surfaces in CI or a
pre-deploy check rather than at the moment the cert lapses.

### Renewing with certbot

`certbot` pairs cleanly with direct TLS because it writes renewed certificates
back to the same paths, and the hot-reload picks them up.

1. Obtain a certificate. The standalone authenticator answers the HTTP-01
   challenge on port 80, so run it while nothing else holds that port:

   ```bash
   sudo certbot certonly --standalone -d app.example.com
   ```

   certbot writes the live certificate to
   `/etc/letsencrypt/live/app.example.com/fullchain.pem` and the key to
   `.../privkey.pem` — exactly the paths used in the `[server.tls]` sample above.

2. Renewal is automatic. certbot installs a systemd timer (or cron job) that runs
   `certbot renew` twice daily and rewrites `fullchain.pem` / `privkey.pem` in
   place when a certificate is within its renewal window. Because the app polls
   the file mtimes, the renewed certificate is served on the next poll with **no
   restart and no dropped requests** — you do not need a `--deploy-hook` to
   reload the app.

   ```bash
   sudo certbot renew --dry-run   # verify the renewal path works
   ```

If you would rather not run certbot at all, the app can obtain and renew its own
certificate — see [Automatic ACME certificates](#automatic-acme-certificates-servertlsacme)
below.

### Local development certificates (mkcert)

For `https://localhost` in development, [`mkcert`](https://github.com/FiloSottile/mkcert)
generates a certificate signed by a locally-trusted CA, so your browser accepts
it without warnings.

1. Install the local CA once (adds it to your system/browser trust store):

   ```bash
   mkcert -install
   ```

2. Generate a certificate for your dev hostnames:

   ```bash
   mkcert localhost 127.0.0.1 ::1
   # writes ./localhost+2.pem and ./localhost+2-key.pem
   ```

3. Point a dev config (or a `[profile.dev]` override) at the generated files:

   ```toml
   [server.tls]
   cert_path = "localhost+2.pem"
   key_path = "localhost+2-key.pem"
   ```

You can now load `https://localhost:<port>` with a trusted certificate. Keep the
generated `*.pem` files out of version control — they are per-developer and the
key is a secret.

---

## Automatic ACME certificates (`[server.tls.acme]`)

With the `acme` cargo feature, the app provisions and renews its own TLS
certificate from an ACME certificate authority (Let's Encrypt by default) over
the HTTP-01 challenge — no static certificate on disk and no reverse proxy. It
builds on the `tls` listener: the issued certificate hot-swaps into the same
reloadable resolver `[server.tls]` uses.

Enable the feature:

```toml
[features]
acme = ["autumn-web/acme"]
```

The happy path is ≤10 lines of config:

```toml
[server]
host = "0.0.0.0"
port = 443

[server.tls.acme]
domains = ["app.example.com"]
contact_email = "admin@example.com"
directory = "production"          # omit for Let's Encrypt STAGING (see below)
```

As with the `tls` feature, ACME is off by default, so build and run **with the
`acme` feature enabled** (it turns on `tls` transitively):

- `cargo run --features acme` — uses the `acme = ["autumn-web/acme"]` forwarding
  feature declared above (or `cargo run --features autumn-web/acme` without it).
- `autumn build --embed --features acme` for a CLI-built single binary.

**Configuring `[server.tls.acme]` without the `acme` feature compiled in is a
fail-fast boot error**, exactly like the `tls` guard above — `AppBuilder::run`
exits non-zero with:

> `[server.tls.acme] is configured but this binary was built without the `acme`
> feature; rebuild with `--features acme`, or configure a static
> cert_path/key_path instead`

On first boot the app answers the ACME HTTP-01 challenge on `http_challenge_port`
(default `80`), obtains a certificate for `domains`, and starts serving HTTPS.
That same `:80` listener also **redirects plain HTTP to HTTPS**, so visitors who
hit `http://` are upgraded automatically.

Fields:

| Field | Default | Meaning |
|---|---|---|
| `domains` | required | One or more non-wildcard domains to issue for. |
| `contact_email` | required | Contact address registered with the ACME account. |
| `directory` | Let's Encrypt **staging** | ACME directory. Built-in endpoints are the bare strings `"staging"` (default) and `"production"`. A private CA / Pebble uses the inline table `{ custom = { url = "https://your-ca.example/dir" } }` — a bare URL string is **not** accepted (see below). |
| `cache_dir` | `config/acme` | Where the account key and issued certificate are cached. |
| `http_challenge_port` | `80` | Port the HTTP-01 challenge (and HTTP→HTTPS redirect) listens on. |
| `renew_before_days` | `30` | Renew this many days before expiry (must be `< 90`). |

> **Staging is the default — switch to production deliberately.** When
> `directory` is unset the app uses the **Let's Encrypt staging** environment,
> which issues certificates from an untrusted test CA (browsers will warn) but
> has **very generous rate limits**. Staging-first is intentional: Let's
> Encrypt's production environment enforces strict issuance rate limits, and a
> misconfiguration loop (wrong domain, port 80 unreachable, DNS not pointed yet)
> can burn your production quota for a week. Validate end-to-end against staging,
> confirm the challenge succeeds, then set `directory = "production"` to get a
> publicly-trusted certificate. The `renew_before_days` window (default 30) keeps
> renewals well ahead of the 90-day certificate lifetime so a transient failure
> has many days of retries before anything expires.

> **Pointing at a private CA (e.g. Pebble).** `directory` is an enum: the
> built-in endpoints are the bare strings `directory = "staging"` and
> `directory = "production"`, but a custom directory must be given as an inline
> table naming the `custom` variant:
>
> ```toml
> [server.tls.acme]
> domains = ["app.example.com"]
> contact_email = "admin@example.com"
> directory = { custom = { url = "https://pebble.test/dir" } }
> ```
>
> A bare URL string (`directory = "https://pebble.test/dir"`) is **not** a valid
> value and makes the config fail to load at startup — use the inline-table form.

### How issuance and renewal work

- **Provisioning** is over HTTP-01: the `:80` listener serves the challenge
  token, the CA validates it, and the issued certificate is cached under
  `cache_dir` (default `config/acme`) and swapped into the live resolver.
- **Renewal** runs on a coordinator loop that wakes hourly and renews any
  certificate within `renew_before_days` of expiry; the refreshed certificate
  hot-swaps into the live resolver with no restart. Leader election only
  serializes **which** instance orders a certificate — it does **not** make ACME
  fleet-safe (see the caveat below).
- **Mutual exclusion.** ACME and static `cert_path` / `key_path` are mutually
  exclusive — configure exactly one. Set `[server.tls.acme]` to auto-issue, or
  `[server.tls]` with `cert_path` / `key_path` to serve your own certificate.

> **Single-process / single-host only.** This in-process ACME flow keeps the
> HTTP-01 challenge token map in the process and the issued certificate in a
> local on-disk cache (`cache_dir`). Behind a load balancer, or with more than
> one replica, the CA's `:80` challenge can be routed to a replica that lacks the
> token (issuance and renewal 404), and non-leader replicas cannot adopt a
> certificate renewed on another instance from that non-shared store. Leader
> election only decides **which** instance orders a certificate; it does not make
> multi-replica ACME work — the app logs a loud startup warning when a
> distributed scheduler backend is configured. For multi-replica or clustered
> deployments, terminate TLS at a shared reverse proxy / load balancer (or a
> single dedicated TLS-terminating instance) instead of in-process ACME
> ([#1620](https://github.com/madmax983/autumn/issues/1620)).

### Scope

This is a **single-host** slice. Wildcard certificates and the DNS-01 challenge
are out of scope (tracked in
[#1620](https://github.com/madmax983/autumn/issues/1620)). For multiple replicas
behind a shared entry point, terminate TLS at the proxy instead — see below.

---

## Terminating TLS at a reverse proxy

If a reverse proxy or platform already fronts your app, terminate TLS there and
let the app serve plain HTTP behind it. This is the right choice for the managed
`autumn deploy` flow and for multi-replica deployments behind a shared load
balancer.

The push-button [`autumn deploy`](./deployment.md#push-button-deploy-to-your-own-server-autumn-deploy)
path installs **kamal-proxy** in front of your app. kamal-proxy listens on your
configured public HTTP port (`server.port`) and, **by default, 443** — its HTTPS
listener is always bound and cannot be disabled, regardless of any app's TLS
setting. By default `autumn deploy` provisions **no** certificate for your app,
so nothing is served over HTTPS until you opt in. You enable TLS termination
**at the deploy-managed proxy** with an opt-in `[deploy.tls]` table:

```toml
[deploy.tls]
enabled = true
host = "app.example.com"   # public DNS name the certificate is issued for
```

With `[deploy.tls] enabled = true`, `autumn deploy` passes `--host <host> --tls`
on every kamal-proxy `route`/`flip` for your app, so kamal-proxy provisions an
**automatic Let's Encrypt** certificate for `host` on-demand and terminates TLS
for it on its always-bound 443 listener. This needs **no `server.port` change**
— issuance uses TLS-ALPN-01 on the already-bound 443, so it works on both the
first deploy and a later redeploy (enabling TLS on an already-deployed app never
restarts or reconfigures the shared proxy). With the table absent (the default)
the route/flip commands carry **no** `--host`/`--tls`, so the proxy serves your
app over plain HTTP only — byte-for-byte the historical behavior.

**Setting `server.port = 80` is recommended** (it is the default) so the proxy
also serves plain HTTP on 80 and can offer the HTTP→HTTPS redirect for visitors
who hit `http://`. It is **not required** for certificate issuance.

> **An external TLS terminator sharing the same host is not supported.** Because
> kamal-proxy always binds 443 and its HTTPS listener cannot be disabled, you
> cannot put your own nginx/Caddy/load-balancer TLS terminator on 443 on the
> same host as the deploy-managed proxy — the two would collide. Terminate TLS
> at kamal-proxy via `[deploy.tls]`, or run the terminator on a **separate**
> host/load balancer in front of the deploy host.

Either way TLS terminates at the **proxy**, not the app. Do **not** enable
in-process `[server.tls]`/ACME on a deploy-managed app: `autumn deploy` binds each app slot to a private loopback
**HTTP** port (the slot systemd unit sets `AUTUMN_SERVER__HOST=127.0.0.1`), the
readiness gate probes it over plain HTTP (`curl http://127.0.0.1:{port}/ready`),
and kamal-proxy routes to a plain `127.0.0.1:{port}` target — so putting a TLS
listener there would fail both the health checks and the plain-HTTP proxy hop.
When TLS is terminated in front, the app needs **neither** the `tls` **nor** the
`acme` cargo feature and **no** `[server.tls]` section — the terminating proxy
owns the certificate.

In-process [`[server.tls]`](#direct-in-process-tls-servertls) and
[ACME](#automatic-acme-certificates-servertlsacme) (the sections above) remain
the right choice for a **self-run / standalone** app you start yourself — one
that owns its own public host:port, not one deployed via `autumn deploy`.

The same applies to any terminating proxy (nginx, Caddy, a cloud load balancer):
point the proxy's certificate at the public `https://` port and forward to the
app's HTTP port. For the full `autumn deploy` walkthrough, fly.io, and
container-based deployment, see the [deployment guide](./deployment.md).
