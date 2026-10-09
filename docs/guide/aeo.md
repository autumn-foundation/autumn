# AEO — agent readiness by default

AI agents find, read, and use web sites. Agent Engine Optimization (AEO)
helps them. Autumn does most AEO tasks automatically. The checks follow
[isitagentready.com](https://isitagentready.com).

You do one thing: set the site base URL.

```toml
[seo]
base_url = "https://example.com"
```

Autumn then serves the items in the table below.

| Category | Item | Default |
|---|---|---|
| Discoverability | `/robots.txt` | On |
| Discoverability | `/sitemap.xml` | On (`[seo]` gives the full one) |
| Discoverability | Agent `Link` headers on `/` | On |
| Discoverability | DNS-AID records | You publish them (see below) |
| Content | `Accept: text/markdown` returns Markdown | On |
| Content | `/llms.txt` | On |
| Bot access | AI crawler groups and `Content-Signal` in `robots.txt` | On |
| Bot access | Web Bot Auth key directory | When a key is set |
| Protocols | MCP server card | When MCP is mounted |
| Protocols | Agent skills index with a `site-guide` skill | On |
| Protocols | WebMCP script | On (add the head tags) |
| Protocols | API catalog (RFC 9727) | When OpenAPI or MCP is mounted |
| Protocols | ARD manifest | On |
| Protocols | OAuth metadata and `/auth.md` | When configured |
| Commerce | x402 payments, MPP discovery | When `[[aeo.paid_routes]]` is set |
| Commerce | UCP and ACP documents | When registered |

The router fallback serves each generated document. The fallback runs only
when no route matches, so an application route at the same path always
wins. With `[seo]` settings or a `SitemapSource`, `/robots.txt` and
`/sitemap.xml` are normal routes; see [SEO](seo.md). Without them, the
fallback `/sitemap.xml` lists `/` and the pages that declare a
`seo(title)`, so the sitemap link in `llms.txt` always resolves.

---

## Turn parts off

```toml
[aeo]
enabled = true          # false turns every AEO part off
markdown = true         # Accept: text/markdown negotiation
markdown_max_bytes = 2097152
link_headers = true
llms_txt = true
site_guide_skill = true
publish_tools = true    # false when a proxy guards /mcp
site_name = "Example Shop"   # default: the OpenAPI title, else the host name
site_description = "Hand-made things."
```

---

## `robots.txt` rules for AI

The default `robots.txt` in production:

```text
# As a condition of accessing this website, you agree to
# ... (the contentsignals.org explanation block) ...

User-agent: *
User-agent: OAI-SearchBot
User-agent: Claude-SearchBot
...
User-agent: GPTBot
User-agent: ClaudeBot
...
Allow: /
Content-Signal: search=yes, ai-input=yes, ai-train=no

Sitemap: https://example.com/sitemap.xml
Agentmap: https://example.com/.well-known/ai-catalog.json
```

The AI crawlers are in the `*` group. The `[seo.robots] additional_rules`
also apply to them. A bot that `additional_rules` names (for example
`User-agent: GPTBot`) keeps only your group.

- `search`: use the content for a search index.
- `ai-input`: use the content as input to AI answers.
- `ai-train`: use the content to train AI models.

Change the signals:

```toml
[aeo.content_signals]
search = true
ai_input = true
ai_train = false
```

Block a class of AI crawler:

```toml
[aeo.ai_crawlers]
search = "allow"      # OAI-SearchBot, Claude-SearchBot, PerplexityBot
user_fetch = "allow"  # ChatGPT-User, Claude-User, Perplexity-User
training = "disallow" # GPTBot, ClaudeBot, Google-Extended, CCBot, ...
```

A disallowed class gets its own group with `Disallow: /` and the same
`Content-Signal` line.

The default `robots.txt` (no `[seo]` settings) says `Disallow: /` only for
the `dev` and `test` profiles, and when no profile is set. With `[seo]`
settings, every profile except `prod` and `production` disallows, unless
`[seo.robots] allow_all` is set.

---

## Markdown for agents

Send `Accept: text/markdown` to any HTML page:

```sh
curl -H 'Accept: text/markdown' https://example.com/about
```

Autumn converts the page to Markdown:

- It converts `<main>`, else `<body>`.
- It drops scripts, styles, navigation, form controls, and hidden elements.
- It puts `<title>` and the description meta tag in YAML front matter.

The response carries these headers:

| Header | Value |
|---|---|
| `Content-Type` | `text/markdown; charset=utf-8` |
| `Vary` | `Accept` |
| `x-markdown-tokens` | Token estimate |
| `Content-Signal` | The `robots.txt` signals |
| `ETag` | A weak tag for the Markdown form, when the HTML has an `ETag` |
| `Cache-Control` | `private`, when the app set no cache policy |

`HEAD` gets the same headers as `GET`, with no body. A streamed page has no
known length, and its `GET` may outgrow `markdown_max_bytes`, so its `HEAD`
keeps the HTML headers. Browsers get HTML. Each
HTML page that Autumn can convert sends `Vary: Accept`. A cache then keeps
the HTML and Markdown copies apart. Some CDNs ignore `Vary` for HTML; for
those, add `Accept` to the cache key.

Autumn does not convert:

- a page larger than `markdown_max_bytes` (it stays HTML),
- a status other than `200`,
- a compressed body, a range, or a download.

Call the converter yourself with `autumn_web::aeo::markdown::html_to_markdown`.

---

## Generated documents

| Path | Content |
|---|---|
| `/llms.txt` | Site name, summary, pages with `seo(title)`, agent entry points |
| `/.well-known/agent-skills/index.json` | Skills index (v0.2.0) with SHA-256 digests |
| `/.well-known/agent-skills/<name>/SKILL.md` | One skill |
| `/.well-known/ai-catalog.json`, `/.well-known/ard.json` | ARD manifest |
| `/.well-known/api-catalog` | RFC 9727 linkset for OpenAPI and MCP |
| `/.well-known/mcp/server-card.json`, `<mcp>/server-card` | MCP server card |
| `/.well-known/oauth-protected-resource` | RFC 9728 metadata |
| `/.well-known/oauth-authorization-server` | RFC 8414 metadata |
| `/auth.md` | How agents get and use a credential |
| `/.well-known/http-message-signatures-directory` | Web Bot Auth JWKS |
| `/_autumn/webmcp.js` | WebMCP tool registration |
| `/.well-known/ucp`, `/.well-known/acp.json` | Commerce documents |

Each document has a strong `ETag` and answers `304` to a matching
`If-None-Match`. A document never carries a `Set-Cookie`.

Set `[seo] base_url` in production. Without it, Autumn builds absolute URLs
from the request `Host` header and sends `Cache-Control: no-store`, so no
shared cache keeps a forged host. Autumn logs a warning in production when
`base_url` is not set.

### Static builds

`autumn build` writes `llms.txt`, the skills, the ARD manifest, `auth.md`,
the OAuth metadata, and the commerce documents into `dist/` when
`[seo] base_url` is set. A file that a static route wrote stays. A static
build has no MCP server or API, so it has no server card or API catalog.
The build does not write the Web Bot Auth directory, because its signature
expires. Markdown negotiation and x402 need the server.

### Agent skills

Autumn publishes a `site-guide` skill. It tells an agent how to read the
site, which tools exist, and where the auth rules are. Add your own skills:

```rust
use autumn_web::aeo::AgentSkill;

let skill = AgentSkill::parse(include_str!("../skills/refunds/SKILL.md"))
    .expect("valid SKILL.md");
autumn_web::app().agent_skill(skill);
```

A skill named `site-guide` replaces the generated one.

### MCP server card

When you call `mount_mcp`, Autumn serves the server card at two paths. The
card lists the tools. When `secure_mcp` gates the endpoint, the card does not
list the tools and says that auth is required. When a proxy or an app-wide
layer guards `/mcp`, set `[aeo] publish_tools = false`.

### OAuth and `auth.md`

Autumn does not run an authorization server. It publishes the values you set:

```toml
[aeo.oauth]
authorization_servers = ["https://auth.example.com"]
scopes_supported = ["read", "write"]

[aeo.auth_md]
registration_url = "https://example.com/settings/tokens"
```

When the app itself is the issuer, add the RFC 8414 fields:

```toml
[aeo.oauth.authorization_server]
issuer = "https://example.com"
authorization_endpoint = "https://example.com/oauth/authorize"
token_endpoint = "https://example.com/oauth/token"
agent_identity_endpoint = "https://example.com/agent/identity"
agent_identity_types = ["anonymous"]
agent_credential_types = ["api_key"]
```

The issuer must be this site's origin with no path. Autumn does not publish
metadata for another issuer.

Autumn serves `/auth.md` when `[aeo.auth_md] registration_url` or OAuth
settings give an agent a way to get a credential. `[aeo.auth_md] enabled =
false` turns it off.

---

## WebMCP

WebMCP lets an AI agent in the browser call your MCP tools. Add the head
tags to your layout:

```rust
html! {
    head {
        (autumn_web::aeo::webmcp::head_tags(None, csrf.as_ref().map(CsrfToken::token)))
    }
}
```

Pass the CSP nonce when your policy uses nonces, and the request's CSRF
token (from the `Option<CsrfToken>` extractor). The token is written as
`<meta name="csrf-token">`, which the htmx CSRF helper reads too. Without a
token the script registers only the read-only tools. On page load, the script
calls `document.modelContext.registerTool()` for each public tool that is
marked safe: `readOnlyHint: true` (a `GET` tool), or `destructiveHint:
false`. A write tool gets `consequentialHint: true`, so the browser can ask the
visitor first. The script sends each call to
the MCP endpoint with the page cookies and the CSRF token. Auth, CSRF, and
rate limits apply to the call.

For a plain HTML form, add the declarative attributes:

```rust
html! {
    form action="/search" toolname="search" tooldescription="Search the catalog" {
        input name="q" toolparamdescription="Search words";
    }
}
```

---

## Web Bot Auth

Web Bot Auth proves that requests from your app's bots come from you.

1. Make an Ed25519 seed:
   `openssl rand 32 | base64 | tr '+/' '-_' | tr -d '='`.
2. Put it in an environment variable.
3. Name the variable in `autumn.toml`:

```toml
[aeo.web_bot_auth]
private_key_env = "WEB_BOT_AUTH_KEY"
signature_agent = "https://example.com"   # default: [seo] base_url
```

Autumn then serves the public key at
`/.well-known/http-message-signatures-directory`. With `[seo] base_url` set,
Autumn signs that response (`@authority` and `content-digest`).
Sign an outbound request:

```rust
use autumn_web::aeo::web_bot_auth::WebBotAuthSigner;

let signer = WebBotAuthSigner::from_state(&state).expect("key set");
client.get("https://example.org/").sign_web_bot_auth(&signer).send().await?;
```

With `[aeo] enabled = false`, Autumn loads no key and `from_state` returns
`None`. The signature covers the first request only. Autumn does not sign a
redirect again. `Signature-Agent` uses the quoted-string form that
Cloudflare verifies, not the draft's dictionary form.

---

## Commerce

### x402

Price a route. A request without payment gets `402` and a
`PAYMENT-REQUIRED` header. The facilitator verifies a retry that has
`PAYMENT-SIGNATURE`. For `GET` and `HEAD`, the handler runs, then Autumn
settles only a `2xx` answer (a redirect is not paid). For other methods,
Autumn settles first, then runs the handler. A priced route never runs free: when
`[aeo.x402]` is incomplete, the route's `amount` is invalid, or the build has
no `http-client` feature, it answers `503`. A facilitator error answer is a
`502`. An entry that can never match a request (a method that is not an HTTP
method, or a `path` without a leading `/`, or with a space, a `?` query, or a
`#` fragment) stops the app at startup.

```toml
[aeo.x402]
facilitator_url = "https://x402.org/facilitator"
pay_to = "0xYourWallet"
network = "eip155:8453"    # Base
asset = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"  # USDC on Base

[[aeo.paid_routes]]
method = "GET"
path = "/api/reports/{id}"
amount = "10000"           # smallest unit: 0.01 USDC
description = "One report"
```

If settlement fails, Autumn sends `402` and discards the handler body. If
the facilitator does not answer, Autumn sends `502`. A paid answer carries
`Cache-Control: private, no-store`. One payment header works once per
process; a second use gets `402`. With several replicas, a client can use a
header once on each replica before settlement fails. Make priced handlers
idempotent (see [idempotency](idempotency.md)).

The route match ignores one trailing `/`, a locale prefix (`/en/...`), and
capture names (`{rid}` matches a `{id}` route), and treats `HEAD` as `GET`. `{name}` matches one segment, `{*name}` the rest.
The facilitator URL must use `https` (`http` only for `localhost`). Scanners probe `GET /api`
and `GET /api/v1`, so price one of these to show x402 support.

A priced route that is also an MCP tool is paid the same way: send
`PAYMENT-SIGNATURE` on the `POST /mcp` request. An unpaid call is a tool error
that holds the `402` body, and the `/mcp` response carries `PAYMENT-REQUIRED`;
a paid one carries `PAYMENT-RESPONSE`.

### MPP

Add `mpp_method` to a priced route. `/openapi.json` then carries
`x-payment-info` and a `402` response for that operation. x402 skips that
route: your handler (or `autumn-billing`) runs the MPP payment.

```toml
[[aeo.paid_routes]]
method = "POST"
path = "/api/render"
amount = "500"
mpp_method = "stripe"
mpp_currency = "usd"
```

### UCP and ACP

Register the documents. `UcpProfile::new` and `AcpDiscovery::new` check the
required fields.

```rust
use autumn_web::aeo::commerce::{AcpDiscovery, UcpProfile};

autumn_web::app()
    .ucp_profile(UcpProfile::new(ucp_json).expect("valid UCP profile"))
    .acp_discovery(AcpDiscovery::new(acp_json).expect("valid ACP document"));
```

---

## DNS-AID

An app cannot publish DNS records. Autumn writes the records for you to
publish. Run the CLI in the project directory:

```sh
autumn aeo dns --mcp-path /mcp
```

It reads `[seo] base_url` from `autumn.toml`, with the profile overlay.
Pass `--base-url` to set it on the command line. The same lines come from
the library:

```rust
use autumn_web::aeo::dns_aid::{DnsAidInput, records};

for line in records(&DnsAidInput::new("https://example.com", Some("/mcp"))) {
    println!("{line}");
}
```

```text
_mcp._agents.example.com. 3600 IN SVCB 1 example.com. alpn="mcp,h2" port=443 mandatory=alpn,port key65400="https://example.com/mcp/server-card"
_index._agents.example.com. 3600 IN SVCB 1 example.com. alpn="h2" port=443
_catalog._agents.example.com. 3600 IN TXT "url=https://example.com/.well-known/ai-catalog.json"
```

Sign the zone with DNSSEC. Validating resolvers then mark the answers as
authenticated.

---

## Check your site

Run the scanner against your deployed site:

```sh
curl -s -X POST https://isitagentready.com/api/scan \
  -H 'content-type: application/json' -d '{"url":"https://example.com"}'
```
