### Added

- **aeo:** agent readiness by default (`autumn_web::aeo`, `[aeo]` in
  `autumn.toml`). An HTML page requested with `Accept: text/markdown` returns
  Markdown, with `Vary: Accept`, `x-markdown-tokens`, and `Content-Signal`.
  The homepage sends agent `Link` headers. See
  [the AEO guide](docs/guide/aeo.md).
- **aeo:** generated agent documents, served from the router fallback so an
  app route at the same path wins: `/llms.txt`, an agent skills index with a
  `site-guide` skill, an ARD manifest (`/.well-known/ai-catalog.json` and
  `/.well-known/ard.json`), an RFC 9727 API catalog, the MCP server card
  (`/.well-known/mcp/server-card.json` and `<mcp>/server-card`), RFC 9728 and
  RFC 8414 OAuth metadata, and `/auth.md`.
- **aeo:** `AppBuilder::agent_skill`, `ucp_profile`, and `acp_discovery`.
- **aeo:** WebMCP. `/_autumn/webmcp.js` registers the public MCP tools with
  `document.modelContext.registerTool()`; add
  `aeo::webmcp::head_tags` to the layout.
- **aeo:** Web Bot Auth. With `[aeo.web_bot_auth] private_key_env`, Autumn
  serves a signed JWKS directory, and `http_client::RequestBuilder::sign_web_bot_auth`
  signs outbound requests (RFC 9421).
- **aeo:** x402 payments for `[[aeo.paid_routes]]` (verify and settle through
  a facilitator; settle only after a `2xx`), MPP `x-payment-info` in
  `/openapi.json`, and `aeo::dns_aid::records` for DNS-AID zone lines.
- **test:** `TestClient::head`.

### Changed

- **seo:** `/robots.txt` and `/sitemap.xml` now mount by default (when
  `[aeo] enabled`, the default), not only when `[seo]` is set. In
  production, `robots.txt` lists the AI crawlers in the `*` group and adds
  `Content-Signal: search=yes, ai-input=yes, ai-train=no`. Set
  `[aeo.content_signals]` to change the signals, or `[aeo] enabled = false`
  for the old output.
