# Security Posture Manifest — Provenance Classes

`autumn routes audit` emits a machine-readable **security posture manifest**: a
stable-ordered JSON document describing what the framework can say about your
application's security posture at build time, and — crucially — *how it knows*.

Every dimension in the manifest is tagged with a **provenance class**. The class
is a promise about the strength of the claim: whether the manifest **proves** it
from your code, merely **reports** what you configured, or has to defer to
runtime. Reading the manifest starts with reading these tags.

```json
{
  "schema_version": 3,
  "dimensions": {
    "routes":           { "provenance": "provable",  "source": "macro:#[secured]/#[authorize]/#[public]", "entries": [ … ] },
    "csrf":             { "provenance": "declared",  "source": "config:security.csrf",     "entries": [ … ] },
    "security_headers": { "provenance": "declared",  "source": "config:security.headers",  "entries": [ … ] },
    "authorization_policies": {
      "provenance": "provable",
      "source": "macro:#[authorize]",
      "runtime_caveat": "the route→action→resource binding is proven from macro-expanded code; which impl Policy<Resource> serves it is resolved from the PolicyRegistry at boot (AppBuilder::policy::<R, _>(...)). A missing registration is not visible at build time — see the policy_registration entry in `excluded`.",
      "entries": [
        { "path": "/posts/{id}/edit", "method": "GET", "name": "edit_post", "action": "update", "resource": "Post", "provenance": "provable" }
      ]
    }
  },
  "excluded": [
    { "dimension": "repository_policy_bindings", "eventual_provenance": "provable",     "reason": "… their presence still shows as routes.entries[].policy" },
    { "dimension": "policy_registration",        "eventual_provenance": "runtime-only", "reason": "boot-time fact; excluded from a build-time manifest by design — the authorization_policies dimension proves bindings and carries this as its runtime_caveat" },
    …
  ]
}
```

There are exactly three provenance classes — `provable`, `declared`, and
`runtime-only`. Which one a dimension carries is a rule, not a judgement call.

## Choosing a provenance class

Ask these three questions **in order** and take the first `yes`:

1. **Can the fact be recovered from macro-expanded code alone** — no config
   file read, no process started? → `provable`.
2. **Is the fact read from typed configuration that the runtime then applies?**
   → `declared`.
3. **Does the fact only come into existence after boot, or per request?** →
   `runtime-only`, and the dimension is named in `excluded` rather than
   emitted.

The order matters because the classes are strictly decreasing in strength, and
a dimension may only claim the class it can defend. When two of the answers
apply to **the same fact**, the later — weaker — one wins.

### The tie-breaker: `provable` with a `runtime_caveat`

When two answers apply to **different steps** of one dimension, the dimension
is not demoted. Most real dimensions are not pure, and
`authorization_policies` is the worked case: the route→action→resource binding
is fully recoverable from the expansion (question 1 is `yes` for the entries),
while the `impl Policy<Post>` that actually answers the check is chosen from
the `PolicyRegistry` at boot (question 3 is `yes` for that adjacent step).

Such a dimension ships as **`provable` with a `runtime_caveat`**. Demoting it
to `declared` would understate what the build genuinely proves, and quietly
shipping it as `provable` with the boot dependency unmentioned would overstate
it. The caveat is a required, non-empty string
carried *on the dimension itself*, so a reader hits it in the same object as
the claim it qualifies, and it cannot be dropped by a later refactor without
failing the manifest's own tests.

Note what the caveat is **not** licensed to do: it qualifies a step *adjacent*
to the proven fact. It cannot be used to hand-wave the proven fact itself. If
the entries would be wrong without a runtime assumption, the dimension is not
`provable`.

### Worked example: outbound HTTP is `declared`

`[http.client.base_urls]` gives each upstream an alias, and handlers call it by
name:

```toml
[http.client.base_urls]
stripe   = "https://api.stripe.com"
sendgrid = "https://api.sendgrid.com"
```

Run the rubric on "which outbound hosts can this app reach?":

1. *Provable?* **No.** The alias table is a convenience, not a chokepoint —
   nothing at build time proves that every outbound call goes through a named
   client. A handler can build its own `Client` and pass an absolute URL, and
   the macros never see it.
2. *Declared?* **Yes.** The allowlist is typed configuration the runtime
   applies when a call resolves an alias.

So `outbound_http` is `declared`, and its `excluded` entry says so —
`eventual_provenance: "declared"`, with the reason naming exactly the gap
("named-client `base_urls` allowlist is config, nothing proves every outbound
call routes through a named client"). See [Outbound HTTP
Client](outbound-http.md) for the alias mechanism itself.

That entry is the shape every future dimension follows. **This rubric is the
honest-labeling gate: no dimension may ship claiming more than it knows.** A
change that adds or promotes a dimension has to answer the three questions,
and if the answer is "provable, with a boot- or request-time step", it has to
name that step in a `runtime_caveat` rather than leave the reader to discover
it.

## `provable`

The claim is **derived from macro-expanded code**. The manifest does not take
your word for it — the fact is a consequence of what you wrote, checked at build
time. A `provable` dimension can back a CI gate that fails the build.

**Example — a `#[secured]` route in the `routes` dimension.** When you guard a
handler:

```rust
#[secured("admin")]
async fn delete_widget(/* … */) -> impl IntoResponse { /* … */ }
```

the route surfaces in the manifest as a proven `gated` classification:

```json
{
  "path": "/widgets/{id}",
  "method": "DELETE",
  "classification": "gated",
  "roles": ["admin"],
  "policy": false,
  "provenance": "provable"
}
```

Nothing about this can drift: the classification *is* the expansion of the
macro. An unannotated mutating handler classifies `unclassified`, and the audit
gate fails and names it.

**Example — an `#[authorize]` binding in the `authorization_policies`
dimension.** Where `routes` answers "is this endpoint guarded at all?", this
dimension answers "guarded *to do what, to which resource?*". A record-level
guard:

```rust
#[get("/posts/{id}/edit")]
#[authorize("update", resource = Post)]
async fn edit_post(post: Post) -> AutumnResult<Markup> { /* … */ }
```

contributes one entry per binding, keyed by the route it sits on:

```json
{
  "path": "/posts/{id}/edit",
  "method": "GET",
  "name": "edit_post",
  "action": "update",
  "resource": "Post",
  "provenance": "provable"
}
```

The same route also appears in the `routes` dimension with `"policy": true` —
the boolean says *a* record-level check runs, this dimension says *which* one.

Three properties of these entries are load-bearing when you read them:

- **It is the resource identifier exactly as written at the use site** — the
  `Post` in `resource = Post`. It is deliberately *not* the `Policy`
  implementation that serves the check: `#[authorize]` never names one. The
  concrete `impl Policy<Post>` is looked up from the `PolicyRegistry` at boot,
  which is precisely what the dimension's `runtime_caveat` discloses.
- **It is an identifier, not a resolved path** — so two same-named types in
  different modules (`billing::Invoice` and `reporting::Invoice`) are
  indistinguishable in the manifest. That is a known limitation of proving
  from an expansion: name resolution is the compiler's job, and it happens
  after the macro has run.
- **Attribute detection is by name, not by resolved import** — the same
  no-name-resolution boundary, on the attribute side. When `#[authorize]` sits
  *below* the route attribute it is recognized textually (`authorize`, or a
  path ending in it, e.g. `#[autumn_web::authorize]`). Import the macro under
  an alias — `use autumn_web::authorize as policy;` — and a `#[policy(...)]`
  written below the route attribute still guards the route at runtime but
  records no binding (and no `policy: true` either; every name-based attribute
  check in the route macros — `#[secured]`, `#[public]`, `#[throttle]` — has
  always shared this boundary). Written *above* the route attribute the alias
  is harmless: the guard expands first and the route macro reads the marker
  its expansion emits, whatever name invoked it. If you alias the guard
  macros, put them above the route attribute — or don't alias them.

One boundary applies to every marker-read fact in the manifest, not just this
dimension: the marker consts (`__AUTUMN_AUTHORIZE_BINDINGS`,
`__AUTUMN_SECURED_ROLES`, `__AUTUMN_PUBLIC`, …) are framework-internal
declarations the macros emit, and the extractors take them at their word.
Hand-writing one in a handler body forges the corresponding claim — a forged
public marker even turns the coverage gate green for an unguarded route. The
manifest's threat model is **drift detection, not an adversarial author**: it
proves what your code declares, so an author lying to their own manifest is an
author lying in their own code, and that is what code review of the
application is for. The audit deliberately does not try to out-verify a
developer with commit access to the code it audits.

Entries are sorted by `(path, method, action, resource)`, and each route's own
bindings are sorted and deduplicated before they reach the manifest, so a
rebuild of unchanged code is byte-identical and a diff shows only what actually
moved. Stacking several `#[authorize]` attributes on one handler yields several
entries; framework-owned routes never contribute any.

The falsifiability that makes this a `provable` dimension is direct: delete one
of a handler's `#[authorize]` attributes and exactly that entry disappears from
the next manifest, while the `routes`, `csrf`, and `security_headers`
dimensions stay byte-identical. (Delete a handler's *only* guard and the
`routes` dimension moves too — reclassifying it to `unclassified` and failing
the gate. That is the other dimension doing its own job, not this one leaking.)

## `declared`

The claim is **read from configuration**. The manifest reports what you
*configured*, not that the runtime honors it. This is weaker than `provable` on
purpose: config is the source of truth for intent, but the manifest is not
re-deriving the middleware behaviour from the request path.

**Example — the CSP string in the `security_headers` dimension.** Your
`[security.headers]` config resolves to an effective
`Content-Security-Policy` template, and the manifest records it verbatim:

```json
{
  "header": "content_security_policy",
  "value": "default-src 'self'; img-src 'self' data:; script-src 'self'; …",
  "emitted": true
}
```

Weakening the policy — say emptying it, which stops the header being sent —
shows up as `"emitted": false` on exactly that entry. The `csrf` dimension is
`declared` for the same reason: it mirrors the `CsrfLayer` predicate against
your configured `enabled` flag and `exempt_paths`, but it is your configuration
speaking, not a proof that every mutating request is checked.

## `runtime-only`

The claim is a **boot-time or per-request fact that a build-time manifest cannot
observe**. Rather than fabricate it, the manifest lists the dimension in
`excluded` with the provenance class it will *eventually* carry once (and if) it
becomes observable.

**Example — policy registration at boot.** `#[authorize("update", resource =
Post)]` proves the binding, but not that anyone ever called
`.policy::<Post, _>(PostPolicy)` on the app builder. The registry is populated
when the app boots, so an application can carry a perfectly proven binding and
still have no policy behind it — a fact no build can see.

That fact is stated twice, on purpose, and the two statements say different
things. On the dimension, as its `runtime_caveat`, where it qualifies the
claim at the point of use:

```json
"runtime_caveat": "the route→action→resource binding is proven from macro-expanded code; which impl Policy<Resource> serves it is resolved from the PolicyRegistry at boot (AppBuilder::policy::<R, _>(...)). A missing registration is not visible at build time — see the policy_registration entry in `excluded`."
```

and in `excluded`, where it stands as its own deferred dimension — because
enumerating *the registry's contents* is a separate, still-unsolved problem.
`PolicyRegistry` exposes only per-type lookups (`policy::<R>()`,
`has_policy::<R>()`) over `TypeId`-keyed, type-erased entries, and the route
dump never runs the registrations at all — it exits with the route listing
before the builder applies them:

```json
{
  "dimension": "policy_registration",
  "eventual_provenance": "runtime-only",
  "reason": "boot-time fact; excluded from a build-time manifest by design — the authorization_policies dimension proves bindings and carries this as its runtime_caveat"
}
```

The `excluded` list is how the manifest stays honest about its own boundaries:
every dimension that is not yet emitted is named, with the class it will carry
and why it is deferred — so a reader can tell the difference between "proven
absent" and "not yet measured".

### Another boundary: serve-path routers

That honesty also covers a runtime boundary: the `serve_path_routers` entry
records that opt-in serve-path HTTP surfaces (MCP, inbound-mail, storage, SEO)
are injected only when the server actually starts — after the route-dump
early-exit — so `autumn routes` does not enumerate them at build time. Their
mutating endpoints (MCP, inbound-mail) run outside the framework CSRF layer and
rely on their own protections; enumerating them is deferred to a dump-plumbing
follow-up.

## Reading `policy` against the proven bindings

The `routes` dimension's `policy` boolean and the `authorization_policies`
entries do not measure the same thing, and the manifest never pretends they do:

```
routes.entries[].policy  ⊇  authorization_policies.entries
```

For every route the macros generate, a binding implies `policy: true` — but not
every route with `policy: true` has a binding. Two guards set the boolean while
leaving nothing a macro can recover:

- **An inline `__check_policy` / `__check_policy_scoped` call** in the handler
  body. That is the shape `#[authorize]` itself expands to, and the route macro
  recognizes the call by name — so a body carrying one is `gated` with
  `policy: true` whether or not the attribute is still visible. What the macro
  cannot do is read an `(action, resource)` pair back out of a hand-written
  call, where both are ordinary expression arguments rather than attribute
  syntax.
- **A `#[repository(api = ..., policy = ...)]` auto-API**, whose generated CRUD
  handlers enforce fixed `show`/`create`/`update`/`delete` actions but whose
  policy type the macro discards at expansion, leaving only a type-erased
  registry probe behind. This gap is disclosed in `excluded` as
  `repository_policy_bindings` with `eventual_provenance: "provable"` —
  recoverable in principle, simply not plumbed yet.

So a route with `"policy": true` and no matching `authorization_policies` entry
is **normal, not a defect**. Read the boolean as "a record-level check runs
here" and the entries as "and here is the part of it we can prove". The
manifest deliberately keeps the wider claim rather than redefining `policy` as
"has a recoverable binding": narrowing it would throw away the only signal
those two guards leave behind.

## See also

- [Route Auth Coverage — the Default-Deny Posture Model](route-auth-coverage.md)
  — the default-deny classification model behind the `routes` dimension, and
  how to classify the three route kinds (`gated`, `public`, `framework`).
- [Record-Level Authorization](authorization.md) — the `Policy` trait,
  `#[authorize]`, and the `#[repository(policy = ...)]` argument behind the
  `authorization_policies` dimension.
- [`autumn routes` — Route Inspection CLI](routes-cli.md) — the `audit`
  subcommand that emits this manifest.
