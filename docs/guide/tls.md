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

### What behaves the same under TLS

Turning on `[server.tls]` changes the transport, not the app. The framework
probes (`/health`, `/live`, `/ready`, `/startup`), `/actuator/health`, the
inbound request timeout (`[server.timeouts]`), SSE streams, `wss://` WebSockets,
and graceful shutdown of an in-flight request all behave exactly as they do over
plain HTTP — including that an SSE body still streams incrementally rather than
being buffered, and that a stream is still exempt from the request deadline.

That parity is enforced, not asserted: `autumn/tests/integration/tls_app_surface.rs`
serves the *same* router twice — once over the TLS listener, once over plain TCP
— and requires each probe response to match on status, body, and content type;
it also drives a `wss://` echo, an SSE stream, a timed-out handler, and a
shutdown drain over TLS on every CI run.

Two things do differ, both by construction:

- **`[server.tls]` cannot be combined with `server.unix_socket`.** Direct TLS
  terminates on `host:port`; configuring both is a startup error.
- **In-place upgrades (`SIGUSR2` handoff) do not apply to a TLS build.** A
  handoff passes the listening socket to a successor process, and a successor
  that terminates TLS cannot adopt a plaintext listener whose clients are
  mid-connection. So a TLS build never offers its socket for handoff (the signal
  logs `in-place upgrade refused` and the running app carries on), and a TLS
  build handed an inherited socket exits at startup rather than failing every
  connection. Restart the process to deploy a new build.

### Serving HTTPS from the release image

`autumn release init` generates a Dockerfile whose builder runs a bare
`cargo build --release` (or `autumn build --embed` with `--embed`). Neither
passes `--features`, so the `tls` feature has to be a **default** feature of
your app for the image to link the TLS stack:

```toml
[features]
default = ["flash", "tls"]
tls = ["autumn-web/tls"]
```

Mount the certificate and key into the container and point `[server.tls]` at
them with the environment (no `autumn.toml` edit needed — the runtime
materializes the section from these vars), and re-point the image's own
HEALTHCHECK at `https://`:

```bash
docker run -d \
  -v /etc/letsencrypt:/etc/letsencrypt:ro \
  -e AUTUMN_SERVER__TLS__CERT_PATH=/etc/letsencrypt/live/app.example.com/fullchain.pem \
  -e AUTUMN_SERVER__TLS__KEY_PATH=/etc/letsencrypt/live/app.example.com/privkey.pem \
  -e AUTUMN_HEALTHCHECK_URL=https://localhost:3000/health \
  -e AUTUMN_HEALTHCHECK_INSECURE=1 \
  -p 443:3000 \
  my-app
```

**Mount the whole `/etc/letsencrypt` tree, at the same path**, not just the
`live/<domain>` directory. certbot's `live/*.pem` are *relative symlinks* into
`../../archive/<domain>/`, so a bind mount of `live/<domain>` alone leaves every
target outside the mount: the container resolves them under `/etc/archive`,
finds nothing, and the app fails fast on an unreadable certificate instead of
serving HTTPS. Mounting the tree at the identical path keeps the symlinks valid
and lets a renewal — which writes a new `archive/` file and re-points the
symlink — be picked up by the poller, since the mtime it stats is the
symlink's target.

With a certificate that is *not* symlinked (a corporate or vendor PEM), any
directory works:

```bash
docker run -d \
  -v /srv/tls:/etc/autumn/tls:ro \
  -e AUTUMN_SERVER__TLS__CERT_PATH=/etc/autumn/tls/fullchain.pem \
  -e AUTUMN_SERVER__TLS__KEY_PATH=/etc/autumn/tls/privkey.pem \
  -e AUTUMN_HEALTHCHECK_URL=https://localhost:3000/health \
  -e AUTUMN_HEALTHCHECK_INSECURE=1 \
  -p 443:3000 \
  my-app
```

Both health-check variables matter. `AUTUMN_HEALTHCHECK_URL` re-points the
generated `HEALTHCHECK`, which defaults to `http://localhost:3000/health`: a
plain-HTTP probe against an HTTPS listener marks the container **unhealthy**
forever — and in the generated `docker-compose.yml`, anything waiting on
`condition: service_healthy` then never starts.

`AUTUMN_HEALTHCHECK_INSECURE=1` lets that probe skip certificate verification.
It is needed because the probe is a loopback call to the container's own
listener while your certificate is issued to the app's *public* hostname, so it
can never validate as `localhost`. Set it only with a loopback URL — it applies
to whatever URL you configure, and unset (the default) the probe always
verifies. The opt-in is deliberate rather than inferred from the URL: `user@`,
`#fragment`, and lookalike hostnames all make a URL *read* as loopback while
curl resolves it somewhere else, so a scheme/host parser in the probe would
quietly stop verifying certificates for a remote endpoint.

Mount the key read-only and keep it `0600` on the host, owned by a user the
container's `autumn` user (uid 10001) can read. (certbot's `archive/` is
`0700 root` by default, so either run the container as a user that can read it
or relax the group permission for the app's user — a certificate the app cannot
open is a fail-fast boot error naming the path, not a silent fallback.) Because
the certificate lives on a bind mount, a host-side `certbot renew` rewrites the
files the container polls — the hot reload works exactly as it does outside a
container, with no image rebuild and no restart.

CI exercises this path on every change to the deployment scaffold: the
`https-target` job in `.github/workflows/release-image-boot.yml` builds the
generated image with `tls` on, boots it with a self-signed test certificate,
and requires an HTTPS `/health` + `/actuator/health` 200 (validated with
`--cacert`), that plain HTTP on the same port does *not* answer, and that the
container's own HEALTHCHECK reaches `healthy`.

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
| `domains` | required | One or more non-wildcard domains to issue for. Each entry is used **verbatim** as the certificate's SAN and as the ACME order's DNS identifier, so an entry with leading or trailing whitespace is rejected at startup rather than requested as-is. |
| `contact_email` | required | Contact address registered with the ACME account. |
| `directory` | Let's Encrypt **staging** | ACME directory. Built-in endpoints are the bare strings `"staging"` (default) and `"production"`. A private CA / Pebble uses the inline table `{ custom = { url = "https://your-ca.example/dir" } }` — a bare URL string is **not** accepted (see below). |
| `cache_dir` | `config/acme` | Where the account key and issued certificate are cached. |
| `http_challenge_port` | `80` | Port the HTTP-01 challenge (and HTTP→HTTPS redirect) listens on. |
| `renew_before_days` | `30` | Renew this many days before expiry (a whole number, unquoted, and `< 90`). |
| `ca_root_path` | unset | PEM root that signs the **ACME directory's own HTTPS certificate**. Only needed for a private CA / Pebble; see below. |

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
>
> A custom directory almost always also needs **`ca_root_path`**. The ACME client
> speaks HTTPS to the directory and, by default, verifies it against the
> **platform trust store** — the host's own installed CA certificates. That is
> right for Let's Encrypt (both its staging and production API endpoints carry
> publicly-trusted certificates) and wrong for a private CA or a Pebble test
> server, whose API certificate chains to a root the host does not know. Unless
> you have installed that root system-wide, the TLS handshake to the directory
> fails and **every** order dies before an authorization is even created:
>
> ```toml
> [server.tls.acme]
> domains = ["app.example.com"]
> contact_email = "admin@example.com"
> directory = { custom = { url = "https://pebble.test/dir" } }
> ca_root_path = "config/pebble-root.pem"
> ```
>
> `ca_root_path` replaces the client's trust anchors *for the ACME control plane
> only* — it has no bearing on which certificates browsers accept from your site.
> `autumn doctor` grades it (`acme_ca_root`) and **fails** on a path that is
> missing or holds no certificate, since that state can only ever produce failed
> orders.

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
> deployments, terminate TLS at a shared reverse proxy / load balancer, or use a
> single dedicated TLS-terminating instance.
>
> [DNS-01](#wildcard-certificates-via-dns-01-servertlsacmedns) removes **half**
> of this: its challenge record lives in your zone rather than in one replica's
> memory, so the `:80` routing problem goes away and the CA never connects to
> your host at all. It does not distribute certificates. The store is still
> local disk, so replicas that did not win the renewal lease never receive the
> issued certificate and keep serving the self-signed placeholder — the startup
> warning still fires under DNS-01, naming the certificate store. To run ACME
> across replicas at all, `cache_dir` must be on storage every replica shares.

### Scope

HTTP-01 as described above is a **single-host** path: the challenge token lives
in the process, so behind a load balancer the CA's `:80` probe can land on a
replica that never published it. For a wildcard certificate — or for a
multi-replica deployment — use **DNS-01**, next.

---

## Wildcard certificates via DNS-01 (`[server.tls.acme.dns]`)

If you run subdomain-per-tenant (`tenant1.myapp.com`, `tenant2.myapp.com`, …),
HTTP-01 is the wrong shape: it needs one issuance per hostname, so tenant *N*'s
first request waits on a certificate order, and Let's Encrypt's rate limits
become a ceiling on how fast you can onboard. A **wildcard** certificate for
`*.myapp.com` covers every tenant that exists and every tenant that ever will —
but no CA will validate a wildcard over HTTP-01. It requires **DNS-01**: proving
you control the zone by publishing a `_acme-challenge` TXT record.

Add a `[server.tls.acme.dns]` section naming your DNS provider, and list the
wildcard in `domains`:

```toml
[server.tls.acme]
domains = ["myapp.com", "*.myapp.com"]
contact_email = "ops@myapp.com"
directory = "production"

[server.tls.acme.dns]
provider = "cloudflare"
```

That is the whole configuration. Everything else — issuance on first boot,
renewal before expiry with no restart, persistence across restarts, the staging
directory, the health indicator — is the same lifecycle the HTTP-01 path above
already has; only the challenge answer changes.

Onboarding a tenant after that costs **zero** certificate work: no issuance, no
restart, no config change. `tenant42.myapp.com` serves valid HTTPS from the
moment it resolves to your host.

### Supported providers

| `provider` | Credential fields | Notes |
|---|---|---|
| `cloudflare` | `api_token` | A scoped API token with **Zone:Read** *and* **DNS:Edit** on the zone — `Zone:Read` because the zone id is discovered from the record name. |
| `route53` | `access_key_id`, `secret_access_key`, optionally `session_token`, `hosted_zone_id`, `region` | Needs `route53:ChangeResourceRecordSets` and `route53:ListResourceRecordSets` on the zone (plus `route53:ListHostedZonesByName` unless you set `hosted_zone_id`). |
| `exec` | none — the hook authenticates itself | The escape hatch: any other provider. See below. |

### Where the credential lives

**Never in `autumn.toml`.** The section has no field that could hold a token,
and it rejects unknown keys — so an `api_token = "..."` written into the config
file is a startup error naming the key, not a plaintext secret nobody notices.

Put it in the encrypted credentials store:

```console
$ autumn credentials edit
```

```console
$ autumn credentials edit --env prod
```

```toml
# config/credentials/prod.toml.enc (decrypted for editing)
[acme_dns]
api_token = "your-cloudflare-token"
```

> Pass the profile you will run under. `AUTUMN_ENV=production` resolves to the
> **`prod`** profile, so the server reads `config/credentials/prod.toml.enc` —
> while a bare `autumn credentials edit` writes `development.toml.enc`. A
> credentials file that does not exist loads as an *empty store*, so the
> mismatch surfaces as "no Cloudflare API token found" rather than as a missing
> file.

The table name is the `credential` key from `[server.tls.acme.dns]`, which
defaults to `acme_dns`. For Route 53 the same table carries the AWS fields:

```toml
[acme_dns]
access_key_id = "AKIA..."
secret_access_key = "..."
# optional; skips the hosted-zone lookup
hosted_zone_id = "Z0123456789ABCDEFGHIJ"
```

The store is decrypted with `AUTUMN_MASTER_KEY` (or `config/master.key`) exactly
like every other credential — see the [credentials guide](./credentials.md).

If you inject secrets through the environment instead, these variables override
the store, field for field:

| Variable | Field |
|---|---|
| `AUTUMN_ACME_DNS_API_TOKEN` | `api_token` |
| `AUTUMN_ACME_DNS_ACCESS_KEY_ID` | `access_key_id` |
| `AUTUMN_ACME_DNS_SECRET_ACCESS_KEY` | `secret_access_key` |
| `AUTUMN_ACME_DNS_SESSION_TOKEN` | `session_token` |
| `AUTUMN_ACME_DNS_HOSTED_ZONE_ID` | `hosted_zone_id` |
| `AUTUMN_ACME_DNS_REGION` | `region` |

Tokens never reach logs, error messages, or `/actuator` output: the secret type
they are held in renders as `<redacted>`, and provider errors carry the API's
own message and status, never the request headers.

### The escape hatch: `provider = "exec"`

Any provider autumn does not ship support for — RFC 2136 dynamic updates, a
registrar's CLI, a webhook shim — is reachable through a hook program:

```toml
[server.tls.acme.dns]
provider = "exec"
command = ["/usr/local/bin/acme-dns-hook"]
```

Autumn runs it twice per challenge record, appending three arguments:

```console
$ /usr/local/bin/acme-dns-hook present _acme-challenge.myapp.com <txt-value>
$ /usr/local/bin/acme-dns-hook cleanup _acme-challenge.myapp.com <txt-value>
```

Exit `0` means the record was written (or removed); anything else fails the
order, with the hook's `stderr` quoted in the message. `command` is an **argv
array** run without a shell, so the record value is never interpreted as shell
syntax. A hook that has not finished within 120 seconds is killed.

An RFC 2136 hook is a five-line script:

```sh
#!/bin/sh
# $1 = present|cleanup, $2 = fqdn, $3 = value
[ "$1" = present ] && ACTION="add $2. 60 TXT \"$3\"" || ACTION="delete $2. TXT \"$3\""
printf 'server ns1.myapp.com\nupdate %s\nsend\n' "$ACTION" | nsupdate -k /etc/autumn/tsig.key
```

### Waiting for propagation

After writing the records, autumn waits until they are visible before telling
the CA to validate — signalling early is how a DNS-01 authorization gets burnt.

The probe goes to the zone's **authoritative** nameservers, not to the resolvers
you configure. That is deliberate, and it is the difference between a feature
that works and one that fails every renewal: a probe sent to a public recursive
resolver the instant the provider's API returns arrives *before* the record is
live on the zone's own servers, so the resolver answers `NXDOMAIN` and **caches
that negatively** for the zone's SOA minimum — 900s on Route 53, 1800s on
Cloudflare, both longer than the propagation budget. Every later probe would
then read the cached "not there". So `resolvers` is used to *discover* the
zone's nameservers (an `NS` lookup, then an `A` lookup per server), and the
propagation probe is sent to those directly. When discovery fails — a
split-horizon setup, a resolver that will not answer `NS` — autumn falls back to
probing `resolvers` directly and logs a warning.

Discovery runs once per distinct challenge name, so an order spanning several
domains probes each zone through *its own* nameservers. This matters: a
nameserver answers only for the zones it is authoritative for, so probing
`myapp.io`'s challenge record at `myapp.com`'s nameservers can never see the
record, and the wait would time out on an order that is in fact correct. An
apex + wildcard order challenges a single name, so the common case still costs
one discovery. Fallback is per name too — one domain with a broken delegation
does not pull the others onto recursive resolvers.

`[server.tls.acme.dns]` fields:

| Field | Default | Meaning |
|---|---|---|
| `provider` | required | `cloudflare`, `route53`, or `exec`. |
| `credential` | `acme_dns` | Key in the encrypted credentials store holding the provider credential. A key *name*, never a token. |
| `propagation_timeout_secs` | `300` | Give up if the records are still not visible after this long. Capped at `3600`. |
| `poll_interval_secs` | `5` | Gap between propagation probes. |
| `resolvers` | `["1.1.1.1:53", "8.8.8.8:53"]` | Resolvers used to discover the zone's nameservers, and probed directly if that fails. Each entry is an IP (port 53 implied) or `IP:port`; hostnames are rejected. |
| `command` | — | The `exec` hook's argv array. Required for `exec`, rejected for the others. |

A timeout names the exact record, the value that never appeared, and the
resolver that never saw it:

> DNS-01 propagation timed out after 300s: the TXT record
> `_acme-challenge.myapp.com` still does not carry `LPJNul-w…` at resolver
> `1.1.1.1:53` (the name does not exist yet). Check that the record was written
> to the zone that actually serves this name and that its NS delegation is live,
> then raise `[server.tls.acme.dns] propagation_timeout_secs` if the provider is
> simply slow.

Challenge records are removed after the order finishes — **including when it
fails**, so a retrying deployment does not fill the zone with dead
`_acme-challenge` entries.

### Two records, one name

An order for `myapp.com` **and** `*.myapp.com` produces two authorizations whose
TXT records share the name `_acme-challenge.myapp.com` but carry different
values. Both must be live at validation time. Every provider here appends a
value and deletes by `(name, value)`, never "replace the record set" — worth
knowing if you write your own `exec` hook, because `nsupdate delete <name> TXT`
without a value would remove its sibling and fail the order.

Route 53 is the exception that proves the rule: its API has no "append", only
`ChangeResourceRecordSets`, which replaces the whole set. Autumn therefore
read-modify-writes — and applies **both** values in a *single* change, because
Route 53 may keep serving the pre-change values until the first change reaches
`INSYNC`, so a second read-modify-write could read the old set and write back
only its own value, dropping the first. If your `exec` hook talks to an API
shaped like that, batch the same way or make the hook idempotent under a stale
read.

### What DNS-01 does *not* need

- **Inbound port 80.** The CA never connects to your host for DNS-01. Autumn
  still *tries* to bind `http_challenge_port` for the HTTP→HTTPS redirect, but a
  bind failure is only a warning under DNS-01 — the app starts and serves HTTPS
  without it, so a container with no `CAP_NET_BIND_SERVICE` is a supported
  deployment. `autumn doctor` grades an unreachable `:80` the same way.
- **A record for the wildcard.** `*.myapp.com` has no address record of its own;
  the tenant subdomains point at your host (a wildcard `A`/`AAAA` record, or one
  per tenant). Creating those is your one-time job — autumn only writes the
  ephemeral challenge TXT records.

### `autumn doctor` for DNS-01

Three checks cover the failure classes DNS-01 adds:

| Check | Grades |
|---|---|
| `acme_dns_credential` | The provider credential is readable and carries the fields the provider needs. **Fail** when missing — without it every issuance and renewal fails. |
| `acme_dns_propagation` | (`--online`) Public DNS can answer for `_acme-challenge.<domain>` at all. **Fail** on `SERVFAIL`/timeout — a broken delegation defeats a correctly-written record. **Warn** on leftover records from an interrupted run. |
| `acme_tenancy_domain` | `[tenancy] base_domain`'s subdomains are actually covered by `[server.tls.acme] domains`. **Fail** otherwise — every tenant host would serve a name mismatch. |

### Failure surfaces through health and alerts

A failed DNS-01 issuance or renewal — an expired provider token, a propagation
timeout, a provider outage — is recorded on the `acme` health indicator (with
`challenge: "dns-01"` and the provider name in its details, never a credential)
and raises the operator-alert
[`scheduled_task_failure`](./operator-alerts.md) condition for
`acme-renewal`. Because renewal starts `renew_before_days` (default 30) ahead of
expiry, that alert fires with weeks of validity left, and clears automatically
on the first successful renewal.

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
