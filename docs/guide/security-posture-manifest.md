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
  "schema_version": 2,
  "dimensions": {
    "routes":           { "provenance": "provable",  "source": "macro:#[secured]/#[authorize]/#[public]", "entries": [ … ] },
    "csrf":             { "provenance": "declared",  "source": "config:security.csrf",     "entries": [ … ] },
    "security_headers": { "provenance": "declared",  "source": "config:security.headers",  "entries": [ … ] }
  },
  "excluded": [
    { "dimension": "policy_registration", "eventual_provenance": "runtime-only", "reason": "boot-time fact; excluded from a build-time manifest by design" }
  ]
}
```

There are exactly three provenance classes.

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

**Example — policy registration at boot.** Which authorization policies are
actually registered in the `PolicyRegistry` is decided when the app boots, not
when it compiles. It therefore appears only in the `excluded` block:

```json
{
  "dimension": "policy_registration",
  "eventual_provenance": "runtime-only",
  "reason": "boot-time fact; excluded from a build-time manifest by design"
}
```

The `excluded` list is how the manifest stays honest about its own boundaries:
every dimension that is not yet emitted is named, with the class it will carry
and why it is deferred — so a reader can tell the difference between "proven
absent" and "not yet measured".

That honesty also covers a runtime boundary: the `serve_path_routers` entry
records that opt-in serve-path HTTP surfaces (MCP, inbound-mail, storage, SEO)
are injected only when the server actually starts — after the route-dump
early-exit — so `autumn routes` does not enumerate them at build time. Their
mutating endpoints (MCP, inbound-mail) run outside the framework CSRF layer and
rely on their own protections; enumerating them is deferred to a dump-plumbing
follow-up.

## See also

- [`autumn routes` — Route Inspection CLI](routes-cli.md) — the `audit`
  subcommand that emits this manifest.
