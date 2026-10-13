# AEO: Agent Readiness by Default — Design

**Date:** 2026-10-09
**Status:** Validated (brainstorm, reverse brainstorm, six hats)
**Scanner:** <https://isitagentready.com> (22 checks, 5 categories)

## Goal

An Autumn app is AEO-ready (Agent Engine Optimization) with no code.
Agents find the site, read it as Markdown, know the content rules, and find
the MCP server, the APIs, the skills, and the auth rules.

Baseline scan of `autumn-web.app`: level 3. It passes discoverability,
Markdown, and bot rules through Cloudflare features. The framework itself
gives none of these. All protocol-discovery checks fail.

## Acceptance criteria

| # | Category | Criterion |
|---|---|---|
| AC1 | Discoverability | `/robots.txt` is served by default (from the router fallback when no `[seo]` route serves it), with `User-agent` groups. |
| AC2 | Discoverability | With `[seo] base_url` set, `/sitemap.xml` is served and `robots.txt` names it. |
| AC3 | Discoverability | The homepage sends `Link` headers with agent relations (`api-catalog`, `service-desc`, `service-doc`, `describedby`). |
| AC4 | Discoverability | DNS-AID: the app cannot publish DNS. Autumn generates the SVCB records to publish. |
| AC5 | Content | `Accept: text/markdown` on an HTML page returns `Content-Type: text/markdown`. HTML stays the default. |
| AC6 | Bot access | `robots.txt` has explicit groups for AI search and AI training crawlers. |
| AC7 | Bot access | `robots.txt` has a `Content-Signal` line (`search`, `ai-input`, `ai-train`). |
| AC8 | Bot access | Web Bot Auth: `/.well-known/http-message-signatures-directory` serves a JWKS when a key is set. Outbound requests can be signed. |
| AC9 | Protocol | MCP Server Card at `/.well-known/mcp/server-card.json` when MCP is mounted. |
| AC10 | Protocol | Agent Skills index at `/.well-known/agent-skills/index.json`, with SHA-256 digests. |
| AC11 | Protocol | WebMCP: pages can register the MCP tools with `document.modelContext.registerTool()`. |
| AC12 | Protocol | API Catalog (RFC 9727) at `/.well-known/api-catalog` when an API exists. |
| AC13 | Protocol | OAuth discovery at `/.well-known/oauth-authorization-server` when configured. |
| AC14 | Protocol | OAuth Protected Resource (RFC 9728) when authorization servers are configured. |
| AC15 | Protocol | `/auth.md` when the app has an API or auth config. |
| AC16 | Protocol | ARD manifest at `/.well-known/ai-catalog.json`. |
| AC17 | Commerce | x402: priced routes answer `402` with x402 payment headers and settle through a facilitator. |
| AC18 | Commerce | MPP: priced routes carry `x-payment-info` in `/openapi.json`. |
| AC19 | Commerce | UCP profile at `/.well-known/ucp` when configured. |
| AC20 | Commerce | ACP document at `/.well-known/acp.json` when configured. |
| AC21 | All | One `[aeo]` config section turns each part off. A user route at the same path wins. |
| AC22 | All | Docs, a changelog fragment, and the scaffold use the defaults. |

## Brainstorming

Ideas, no filter:

- Serve every well-known document from data Autumn already has: route
  registry, `#[api_doc(mcp)]` tools, OpenAPI config, health path, SEO titles.
- Convert HTML to Markdown in a middleware. No handler change.
- Put title and description from `<head>` in Markdown front matter.
- Make `llms.txt` from routes that declare `seo(title, description)`.
- Make one default skill: "how an agent uses this site".
- Project MCP tools into WebMCP with one generated script.
- Use the MCP tool list as the server card tool list.
- Use the existing DNS-01 providers to publish DNS-AID records.
- Sign outbound `http_client` requests with Web Bot Auth.
- Priced routes: one declaration drives x402 and MPP.
- A local `autumn aeo check` that runs the scanner checks.

## Reverse brainstorming

"How do we make this harmful?" and the guard for each:

| Harm | Guard |
|---|---|
| A CDN caches Markdown and serves it to browsers. | `Vary: Accept` on every negotiable HTML response. |
| The Markdown keeps the HTML `ETag`; a `304` replays the wrong body. | Rewrite the `ETag` to a weak, Markdown-only value. |
| Conversion buffers a huge or endless body. | Size limit; past it, stream the HTML unchanged. |
| Deep nesting overflows the stack. | Depth limit in the converter. |
| Markdown runs on errors, downloads, ranges, or compressed bodies. | Convert only `200` + `text/html`, no `Content-Encoding`, no attachment, no range. |
| A server card or WebMCP script leaks a tool list the app gated with `secure_mcp`. | List tools only when the MCP endpoint has no auth layer. |
| An explicit `User-agent: GPTBot` group drops the app's `Disallow` rules for that bot. | AI bots share the `*` group, so they get the same rules. |
| A generated route hides a user route. | Every generated route yields to a user `GET` at the same path. |
| `/robots.txt` in dev invites indexing. | Dev and test keep `Disallow: /`. |
| A published OAuth document claims an issuer the app is not. | Publish only values the operator configures. |
| x402 settles a payment for a failed request. | Settle only after a `2xx` handler response. |
| x402 trusts the client. | The facilitator verifies; no local trust. |
| WebMCP calls bypass CSRF. | The script sends the page CSRF token; the route pipeline checks it. |
| A private key appears in logs. | The key loads from the env var that `private_key_env` names; `Debug` shows only the key id. |

## Six thinking hats

- **White (facts):** The scanner rules are public (`llms-full.txt`). Levels
  1–3 need robots, sitemap, Link headers, bot rules, signals, Markdown.
  Level 4 needs one of: server card, A2A card, skills, API catalog.
  Commerce checks are neutral for non-commerce sites. DNS-AID needs DNSSEC,
  which is a zone setting.
- **Red (feelings):** Users want "it just works". A framework that changes
  `robots.txt` content by surprise feels bad. So: a clear default, a clear
  switch, docs in one page.
- **Black (risks):** Body rewrite middleware is the riskiest part (caching,
  ETags, compression order, memory). Commerce moves money. The specs are
  drafts and change. Keep wire formats in one module with tests.
- **Yellow (benefits):** Most checks pass with zero code. MCP apps reach
  level 4–5. The same data feeds OpenAPI, MCP, WebMCP, and ARD, so nothing
  drifts.
- **Green (alternatives):** Inject the WebMCP script into every page
  (rejected: second body rewrite; use a head helper and the scaffold).
  A2A card (rejected: Autumn does not speak A2A; out of scope). Publish
  DNS-AID through ACME DNS providers (deferred: needs SVCB support in the
  providers, and DNSSEC stays manual).
- **Blue (process):** TDD per slice: red test, green code, refactor. Order:
  robots → Markdown → Link headers → well-known documents → WebMCP →
  Web Bot Auth → commerce → DNS-AID → docs. Review with agents at the end.

## Design

### Module

`autumn/src/aeo/` — no new crates in the default build.

| File | Content |
|---|---|
| `mod.rs` | Config, site assembly, the router of generated documents. |
| `markdown.rs` | HTML-to-Markdown converter. |
| `negotiate.rs` | The response layer: Markdown negotiation, `Vary`, homepage `Link`. |
| `documents.rs` | JSON/Markdown bodies: API catalog, server card, skills, ARD, OAuth, auth.md, llms.txt. |
| `webmcp.rs` | WebMCP script and head helper. |
| `web_bot_auth.rs` | JWKS directory and RFC 9421 request signing. |
| `commerce.rs` | x402 layer, MPP OpenAPI extension, UCP/ACP documents. |
| `dns_aid.rs` | DNS-AID record lines. |

### Where it mounts

- With `[seo]` settings, `robots.txt` and `sitemap.xml` stay normal routes
  in `seo.rs`. Without them, the AEO fallback serves a default `robots.txt`.
- `build_router_pre_state` takes the site snapshot after MCP and OpenAPI
  are known. The router fallback (set in `apply_middleware`) serves the
  documents, so `TestApp` gets them too.
- x402 sits innermost, after rate limits, CSRF, the trusted-host check, the
  timeout, and CORS. The SSG/ISR path adds a second copy outside the
  static-first layer; the copies share one used-payment cache.
- The response layer sits inside compression, outside the exception filter,
  on both the dynamic and the static (SSG/ISR) paths.

### Defaults

| Item | Default |
|---|---|
| Content signals | `search=yes, ai-input=yes, ai-train=no` |
| AI crawlers | Listed in the `*` group (same rules as all crawlers) |
| Markdown negotiation | On |
| Homepage `Link` headers | On |
| Generated agent skill | On |
| OAuth, Web Bot Auth, commerce | Off until configured |
