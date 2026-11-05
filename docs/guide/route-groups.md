# Route groups and per-role mounting

One image can serve different routes on different pods. Put routes in
groups. Then give each role a `mount` list. A role serves only the groups in
its list. You can then scale each role on its own, and a failure in one role
does not stop the other. The compiler still checks every boundary, because
it is still one build.

See [ADR 0036](../adr/0036-route-groups.md) for the decision.

## Put routes in groups

A route gets its group from the first match in this list:

1. The route attribute: `#[get("/x", group = "marketing")]`, or
   `Route::in_group("marketing")`. Only the HTTP method attributes
   (`#[get]`, `#[post]`, `#[put]`, `#[patch]`, `#[delete]`) take `group`. For
   `#[static_get]`, `#[ws]` and repository API routes, use `routes_in` or
   `Route::in_group`.
2. `AppBuilder::routes_in("marketing", routes![...])`.
3. The plugin that adds the route: `Plugin::route_group()`. The default is
   `Plugin::name()`.
4. The default group, `app`.

```rust
use autumn_web::prelude::*;

#[get("/", group = "marketing")]
async fn landing() -> &'static str { "Welcome" }

#[get("/tickets")]
async fn tickets() -> &'static str { "Tickets" }

#[get("/health/deep")]
async fn deep_health() -> &'static str { "ok" }

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![landing])            // group "marketing" (attribute)
        .routes_in("service", routes![tickets])
        .routes(routes![deep_health])        // group "app"
        .run()
        .await;
}
```

A plugin can set a short group name:

```rust
use std::borrow::Cow;
use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;

struct LeadFormsPlugin;

impl Plugin for LeadFormsPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("lead-forms")
    }

    fn route_group(&self) -> Cow<'static, str> {
        Cow::Borrowed("marketing")
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        app
    }
}
```

- A plugin that does not set `route_group()` uses its name, for example
  `autumn-admin-plugin` or `my_crate::CrmPlugin`. A role with `mount` does
  not serve the routes of a plugin whose group is not in its list.
- `merge` and `nest` routers get the group of the code that adds them: the
  plugin group, or `app`. To put one in another group, add it from a plugin
  whose `route_group()` names that group.
- For `scoped` routes, set the group on each route.
- A group name is case-sensitive, and must not be blank.
- `autumn routes --format json` shows the `group` of each route. It lists
  every group, for every role.

## Mount groups on a role

Set `mount` on a `[roles.<name>]` preset:

```toml
[roles.marketing]
base = "web"
mount = ["marketing"]

[roles.service]
base = "web"
mount = ["service", "app"]
```

- If you do not set `mount`, the role serves every group. This is the same
  as before.
- `mount` needs a base that serves HTTP (`web` or `combined`). A `worker`
  base with `mount` stops the load.
- `mount = []` and a blank name stop the load.
- A `mount` entry that names no group of the app stops the boot. The error
  lists the groups of the app.
- When a preset sets `mount`, the app logs a warning at boot for each group
  that no preset mounts.
- `GET /actuator/info` shows the list as `role.mount`.

A request for a route in a group that the role does not mount gets `404 Not
Found`. Autumn does not send `421 Misdirected Request`: a browser retries a
421 on a new connection, and behind one ingress the retry goes to the same
pods.

A path helper (`__autumn_path_<name>()`, `paths![...]`) for a route in an
unmounted group still compiles and returns the path. The URL is valid for the
deployment.

## Shared routes

The framework mounts these routes on every role that serves HTTP. A `mount`
list does not remove them:

| Route | Why it is shared |
| --- | --- |
| `/live`, `/ready`, `/startup`, `/health` | The orchestrator probes each pod. |
| `/actuator/*` | Operators read every pod. |
| `/static/*`, `/static/_plugins/*` | A page in any group loads these files. |
| htmx, flash and widget assets | Same as `/static/*`. |
| `/_autumn/*` (mail previews, unsubscribe, job status, dev inspector) | Framework status routes. |
| `/__autumn/*` (dev reload) | Development only. |
| `/_stories/*` (when `stories.enabled`) | The widget story gallery. |
| `/openapi.json`, `/swagger-ui` | Each role documents the routes it serves. |
| The MCP endpoint | Its tools are the routes of the role. |
| `/sitemap.xml`, `/robots.txt`, storage and inbound-mail webhooks | The framework mounts them from config. |

A pre-rendered page (`#[static_get]`, `autumn build`) follows the group of
its route. A role does not serve the page from `dist/` and does not list it
in `/sitemap.xml` when it does not mount that group.

An auth callback (for example `#[oauth2_callback]`) is an app route. It is
not shared. Put the callback in a group that each role which starts a login
mounts.

The test `framework_routes_are_shared_by_every_role` in
`autumn/src/app.rs` checks that a narrow role keeps each framework route of a
full role, with the default config. The integration tests in
`autumn/tests/integration/route_groups.rs` send requests to the probes and
the actuator on each role.

## Graph, OpenAPI and doctor

- `autumn graph` and `/actuator/graph` show the routes of the role. Set the
  role with `AUTUMN_ROLE`.
- `completeness.unmounted_routes` lists a route only when no declared
  `[roles.<name>]` preset mounts it. A route that another preset mounts is in
  `completeness.mounted_elsewhere`.
- `autumn openapi export` and `/openapi.json` list only the routes of the
  role.
- `autumn doctor --strict` fails when a declared group has no preset that
  mounts it. Declare the groups in `[route_groups]`:

```toml
[route_groups]
manifest = "routes.json"   # the output of autumn routes --format json
```

or write them by hand:

```toml
[route_groups]
declared = ["marketing", "service", "app"]
```

- If you set `manifest`, doctor ignores `declared`. Doctor reads the path
  from the directory where it runs.
- A `manifest` that is not there, or that names no group, is a warning.
- When a preset serves HTTP, doctor does not count the built-in roles (`web`,
  `combined`). Declare each HTTP tier as a `[roles.<name>]` preset.

## Worked example: one image, two roles

The app has a `marketing` group and a `service` group. It runs the
`marketing` role and the `service` role from one image. The image that
`autumn release init` sets up listens on port 3000 and runs
`/usr/local/bin/my-app`.

`autumn.toml`:

```toml
[jobs]
backend = "postgres"

[roles.marketing]
base = "web"
mount = ["marketing"]

[roles.service]
base = "combined"
mount = ["service", "app"]

[route_groups]
declared = ["marketing", "service", "app"]
```

### Fly.io

Each Fly process group runs one role. The process command replaces the
image `CMD`, and `env` sets the role:

```toml
# fly.toml
app = "my-app"

[env]
AUTUMN_ENV = "production"

[processes]
marketing = "env AUTUMN_ROLE=marketing /usr/local/bin/my-app"
service = "env AUTUMN_ROLE=service /usr/local/bin/my-app"

[[services]]
processes = ["marketing"]
internal_port = 3000
protocol = "tcp"

  [[services.ports]]
  port = 443
  handlers = ["tls", "http"]

  [[services.http_checks]]
  path = "/ready"
  interval = "10s"
  timeout = "2s"

[[services]]
processes = ["service"]
internal_port = 3000
protocol = "tcp"

  [[services.ports]]
  port = 8443
  handlers = ["tls", "http"]

  [[services.http_checks]]
  path = "/ready"
  interval = "10s"
  timeout = "2s"
```

Fly routes by port, not by path. Here the marketing role answers on port
443 and the service role on port 8443. To serve both from one host name and
port, put a proxy in front that sends each path to the right process group,
or run two Fly apps from the image.

Scale each group on its own:

```sh
fly scale count marketing=2 service=4
```

### Kubernetes

Two Deployments use the same image. Each one sets `AUTUMN_ROLE`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app-marketing
spec:
  replicas: 2
  selector:
    matchLabels: { app: my-app, role: marketing }
  template:
    metadata:
      labels: { app: my-app, role: marketing }
    spec:
      containers:
        - name: app
          image: my-app:1.4.0
          env:
            - name: AUTUMN_ROLE
              value: marketing
          ports:
            - name: http
              containerPort: 3000
          startupProbe:
            httpGet: { path: /startup, port: http }
            periodSeconds: 2
            failureThreshold: 60
          readinessProbe:
            httpGet: { path: /ready, port: http }
          livenessProbe:
            httpGet: { path: /live, port: http }
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-app-service
spec:
  replicas: 4
  selector:
    matchLabels: { app: my-app, role: service }
  template:
    metadata:
      labels: { app: my-app, role: service }
    spec:
      containers:
        - name: app
          image: my-app:1.4.0
          env:
            - name: AUTUMN_ROLE
              value: service
          ports:
            - name: http
              containerPort: 3000
          startupProbe:
            httpGet: { path: /startup, port: http }
            periodSeconds: 2
            failureThreshold: 60
          readinessProbe:
            httpGet: { path: /ready, port: http }
          livenessProbe:
            httpGet: { path: /live, port: http }
```

Each Deployment has its own Service. The Ingress sends the marketing paths
to `my-app-marketing` and the rest to `my-app-service`:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: my-app-marketing
spec:
  selector: { app: my-app, role: marketing }
  ports:
    - { name: http, port: 80, targetPort: http }
---
apiVersion: v1
kind: Service
metadata:
  name: my-app-service
spec:
  selector: { app: my-app, role: service }
  ports:
    - { name: http, port: 80, targetPort: http }
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: my-app
spec:
  rules:
    - host: my-app.example.com
      http:
        paths:
          - path: /tickets
            pathType: Prefix
            backend:
              service: { name: my-app-service, port: { name: http } }
          - path: /
            pathType: Prefix
            backend:
              service: { name: my-app-marketing, port: { name: http } }
```

A path that the Ingress sends to the wrong Service gets `404`. Put
`/static/*` and other shared paths on either Service: both roles serve them.

### Check the split

```sh
AUTUMN_ROLE=marketing autumn graph show --json
AUTUMN_ROLE=marketing autumn openapi export --out openapi-marketing.json
autumn routes --format json > routes.json
autumn doctor --strict
```

## See also

- [Named roles](jobs.md#named-roles): every `[roles.<name>]` key.
- [Split web/worker topology](cloud-native.md#split-webworker-topology).
- [Architecture graph](architecture-graph.md).
