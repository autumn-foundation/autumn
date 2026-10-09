### Added

- **aeo:** agent readiness by default (`autumn_web::aeo`, `[aeo]` in
  `autumn.toml`). An HTML page requested with `Accept: text/markdown` returns
  Markdown, with `Vary: Accept`, `x-markdown-tokens`, and `Content-Signal`.
  The homepage sends agent `Link` headers. See
  [the AEO guide](docs/guide/aeo.md).
- **aeo:** generated agent documents, served from the router fallback so an
  app route at the same path wins: `/robots.txt` (when no `[seo]` route
  serves it), `/llms.txt`, an agent skills index with a `site-guide` skill,
  an ARD manifest (`/.well-known/ai-catalog.json` and
  `/.well-known/ard.json`), an RFC 9727 API catalog, the MCP server card
  (`/.well-known/mcp/server-card.json` and `<mcp>/server-card`), RFC 9728 and
  RFC 8414 OAuth metadata, and `/auth.md`.
- **aeo:** `robots.txt` lists the AI search, user-fetch, and training
  crawlers and adds `Content-Signal: search=yes, ai-input=yes, ai-train=no`
  (`[aeo.content_signals]`, `[aeo.ai_crawlers]`).
- **aeo:** `autumn build` writes the agent documents into `dist/` when
  `[seo] base_url` is set.
- **aeo:** `AppBuilder::agent_skill`, `ucp_profile`, and `acp_discovery`.
- **aeo:** WebMCP. `/_autumn/webmcp.js` registers the public MCP tools with
  `document.modelContext.registerTool()`; add `aeo::webmcp::head_tags` to the
  layout.
- **aeo:** Web Bot Auth. With `[aeo.web_bot_auth] private_key_env`, Autumn
  serves a signed JWKS directory, and
  `http_client::RequestBuilder::sign_web_bot_auth` signs outbound requests
  (RFC 9421).
- **aeo:** x402 payments for `[[aeo.paid_routes]]` (facilitator verify and
  settle, one use per payment header), MPP `x-payment-info` in
  `/openapi.json`, and DNS-AID zone lines (`aeo::dns_aid::records`).
- **cli:** `autumn aeo dns` prints the DNS-AID records to publish.
- **scaffold:** new apps add `aeo::webmcp::head_tags` to the layout, and
  `autumn.toml` documents `[aeo]`.
- **test:** `TestClient::head`.

### Changed

- **seo:** an app with no `[seo]` settings now serves `/robots.txt` (from
  the router fallback, so an app route still wins). It says `Disallow: /`
  for the `dev` and `test` profiles. Set `[aeo] enabled = false` for the old
  `404`.
