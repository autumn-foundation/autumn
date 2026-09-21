# Autumn Guide Index

Every guide page, grouped by what you came here to do. Each line says which
question the page answers — open the one that matches yours.

This page is an index only: it carries no answers of its own, so it can never
disagree with the page it points at. `scripts/check-docs-guide-index.sh` fails
the build when a guide page is missing from it, listed twice, or points
nowhere.

New to Autumn? Start with the first two entries under "Start here" below.

Looking for the API reference instead? That is
[docs.rs/autumn-web](https://docs.rs/autumn-web).

## Start here

- [Getting Started](getting-started.md) — install the CLI, generate an app, and serve your first route
- [Todo Tutorial](tutorial/index.md) — twelve chapters from empty directory to a deployed CRUD app
- [Application Starters](starters.md) — the prebuilt app shapes `autumn new` can generate from
- [Code Generators](generators.md) — `autumn generate model | migration | scaffold`, and what each one writes
- [Coming From Other Frameworks](coming-from-other-frameworks.md) — the Rails, Django, Spring and Laravel word for each Autumn concept
- [What Happens When...](what-happens-when.md) — the request path end to end, in order, with the code that runs at each step
- [Platform Support](platform-support.md) — what runs natively on Windows, what needs WSL2, and what is Linux-only

## Routing, requests and responses

- [Middleware](middleware.md) — the built-in stack, and writing your own layer
- [Extractors](extractors.md) — the full catalog, the two ordering rules, and decoding nested or repeated query parameters
- [Typed Path Helpers](path-helpers.md) — building a URL to a route without hand-writing the string
- [Route Inspection CLI](routes-cli.md) — `autumn routes`, for seeing what is actually mounted
- [Content Negotiation](content-negotiation.md) — serving HTML to a browser and JSON to an API client from one handler
- [Conditional GET and ETags](conditional-get.md) — returning `304 Not Modified` instead of the body
- [Response Compression](compression.md) — gzip and brotli, and when Autumn skips them
- [File Downloads and Range Requests](downloads.md) — sending a file, resumable downloads, and `Content-Disposition`
- [Generating PDFs](pdf-downloads.md) — rendering a PDF and returning it as a download
- [API Versioning, Deprecations and Sunsets](api-versioning.md) — running two versions of an endpoint and retiring the old one
- [Idempotency Keys](idempotency.md) — making a retried POST safe to replay
- [One-Time Submit Tokens](submit-tokens.md) — stopping a double-clicked form from submitting twice
- [Rate Limiting](rate-limiting.md) — capping how often a caller can hit a route
- [Outbound HTTP Client](outbound-http.md) — calling another service from a handler
- [Resilience and Circuit Breakers](resilience.md) — timeouts, retries and breakers on outbound calls

## Templates, forms and the browser UI

- [Forms, Validation and Normalization](forms.md) — re-rendering a rejected submission with the user's input and inline errors
- [Nested `has_many` Forms](nested-forms.md) — editing a parent and its children in one form
- [Multi-Step Form Wizards](wizards.md) — a form split across several pages, with state carried between them
- [Flash Messages](flash.md) — showing "Saved!" after a redirect
- [View Formatting Helpers](format-helpers.md) — `number_to_currency`, `pluralize`, `truncate`, `time_ago_in_words` and friends
- [Maud Fragment Caching](fragment-caching.md) — caching part of a rendered page
- [Tabs](tabs.md) — tabbed panels without writing JavaScript
- [Widget Styling](widget-styling.md) — restyling the shipped widgets to match your design
- [Widget Stories](stories.md) — the `/_stories` gallery for previewing widgets in isolation
- [Rich Text](rich-text.md) — accepting and safely rendering user-authored markup
- [Accessibility](accessibility.md) — the accessibility guarantees of the shipped widgets, and `autumn a11y verify`
- [Generated UI (Constela)](constela.md) — server-rendering an interface a language model produced
- [WASM Islands (Yew CSR)](wasm-islands.md) — dropping a client-side interactive component into a server-rendered page

## Database, models and queries

- [Declarative Schema](declarative-schema.md) — describing tables on the model and letting Autumn derive the DDL
- [Migrations](migrations.md) — writing, running, and rolling back schema changes
- [Database Seeding](seeding.md) — populating a fresh database with development data
- [Repositories and Bulk Operations](repositories.md) — querying, inserting and updating records
- [Transactions](transactions.md) — running several writes as one unit
- [Hooks and Transactions](hooks-and-transactions.md) — running code before or after a save, inside the same transaction
- [Aggregate Queries](aggregates.md) — counts, sums and group-bys without dropping to raw SQL
- [Pagination](pagination.md) — paging a long list, by offset or by cursor
- [Soft Delete](soft-delete.md) — hiding a record instead of destroying it
- [Record Version History](version-history.md) — keeping every past version of a row
- [Ledgered Entities](ledgered-entities.md) — tamper-evident history and as-of queries
- [Counter Caches](counter-cache.md) — keeping `posts.comment_count` correct without a query per row
- [Maintained Derived Read Models](derivations.md) — a filtered count or sum on the parent, updated in the same transaction
- [Declarative State Machines](state-machines.md) — allowed status transitions, enforced on the model
- [Transition Effects](transition-effects.md) — running a side effect when a state machine moves between two states
- [Typed Lifecycles](lifecycle.md) — modelling an entity's stages in the type system
- [Votes, Likes and Reactions](votable.md) — race-safe reaction counts on any model
- [Threaded Comments on Anything](commentable.md) — attaching a comment thread to any record
- [Horizontal Sharding](sharding.md) — splitting one logical database across several physical ones
- [SQLite in Production](sqlite-in-production.md) — when SQLite is a reasonable production database, and how to run it

## Files, media and storage

- [File Storage](storage.md) — uploading a file and storing it locally or in S3
- [Image Variants](storage-variants.md) — generating and serving resized versions of an uploaded image
- [Live Media](media.md) — broadcast and multi-participant rooms

## Search

- [Search: keyword and vector](search.md) — `autumn-search`, and choosing between keyword and embedding search
- [Full-Text Search](full-text-search.md) — Postgres FTS over your own models
- [Active Search and Autocomplete](active-search-and-autocomplete.md) — results as the user types, and typeahead pickers that store the chosen record's ID

## Authentication and authorization

- [Authentication](authentication.md) — sessions, login and logout, password policy, lockout, and remember-me
- [Sign in with OAuth2 / OIDC](oauth.md) — letting users sign in with Google, GitHub and other providers
- [Record-Level Authorization](authorization.md) — deciding who may see or change a particular record
- [Step-Up Authentication](step-up-authentication.md) — re-asking for a password before a dangerous action
- [Route Auth Coverage](route-auth-coverage.md) — proving no route was left unauthenticated by accident
- [Encrypted Credentials](credentials.md) — keeping secrets in the repository without keeping them in plaintext
- [Signing Secrets](signing-secrets.md) — where cookie and token signing keys come from, and rotating them

## Security, privacy and compliance

- [Attribute Encryption](attribute-encryption.md) — encrypting a column at rest
- [Confidential Fields](confidential-fields.md) — sealing a column under a key the server never holds
- [Data Classification](data-classification.md) — marking personal-data columns so the compiler catches leaks
- [Logging and PII](logging-pii.md) — keeping personal data out of logs
- [Audit Logging](audit-logging.md) — recording who changed what, and when
- [Cookie Consent](cookie-consent.md) — the consent gate, the banner, and the withdraw flow
- [Bot Protection and CAPTCHA](bot-protection.md) — keeping automated traffic off a form or route
- [TLS and HTTPS](tls.md) — certificates, ACME, and terminating TLS
- [Data Retention for Framework-Owned Data](data-retention.md) — bounding the tables Autumn itself creates
- [Data-Retention Sweeps](retention-sweeps.md) — auto-purging your own tables on a schedule
- [Data Scrubbing](data-scrubbing.md) — turning a production backup into an anonymized staging copy
- [The Security Posture Gate](posture-gate.md) — failing the build on an unsafe production configuration
- [Security Posture Manifest](security-posture-manifest.md) — the diffable record of what each posture claim rests on
- [Verify What You're Running](supply-chain.md) — provenance and supply-chain verification for a release

## Background work and scheduling

- [Background Jobs](jobs.md) — `#[job]`: running work outside the request
- [Operating Background Jobs](operating-background-jobs.md) — the dashboard, retries, and recovering a stuck queue
- [One-Off Tasks](tasks.md) — `#[task]` and `autumn task`, for work you run by hand
- [Multi-Replica Scheduled Tasks](scheduled-multi-replica.md) — running a cron job exactly once across several replicas
- [Events and Listeners](events.md) — `#[event]` / `#[listener]`, for decoupling one action from its consequences
- [Distributed Locks](distributed-locks.md) — making sure only one process does a thing at a time

## Realtime and messaging

- [Realtime Channels, SSE and htmx Broadcasts](realtime.md) — pushing an update to a connected page
- [WebSockets](websockets.md) — `#[ws]` handlers and connection lifecycle
- [Distributed Presence](presence.md) — who is online, across replicas
- [Collaborative Fields](collaboration.md) — two people editing the same text at once, without losing a character
- [Web Push](web-push.md) — browser notifications to a user who has closed the tab
- [In-App Notifications](notifications.md) — a notification centre inside your own UI

## Mail

- [Mail](mail.md) — mailers, templates, previews, and delivery
- [Mail Compliance](mail-compliance.md) — `List-Unsubscribe` and the one-click unsubscribe route

## APIs, webhooks and integrations

- [OpenAPI Spec Generation](openapi.md) — the spec Autumn derives from your handlers, and Swagger UI
- [Exposing Your API as MCP Tools](mcp.md) — projecting typed endpoints into a Model Context Protocol server
- [The Agent Authority Envelope](agent-authority.md) — bounding what an agent-callable handler is allowed to do
- [Wire Contracts](wire-contracts.md) — failing a caller's build when a request or response field stops matching
- [Signed Webhook Intake](signed-webhooks.md) — verifying a sender's signature on webhooks arriving in
- [Outbound Signed Webhooks](outbound-webhooks.md) — sending signed webhooks out, with retries and a dead-letter queue
- [Edge Capsules](edge.md) — compiling read-path routes into a WASM artifact a CDN can run

## Product features

- [Billing](billing.md) — Stripe checkout and portal, entitlement gating, and dunning retries
- [Money and the Ledger](money.md) — holding amounts without rounding them away, and moving money without double-charging or losing it
- [A/B Experiments](experiments.md) — assigning users to variants and reading the results
- [Feature Flags](feature-flags.md) — turning a feature on for some users and not others
- [Admin Panel](admin.md) — the generated CRUD backoffice, and restricting who reaches it

## Content, SEO and localization

- [SEO](seo.md) — canonical URLs, `robots` directives, and the auto-mounted `sitemap.xml` and `robots.txt`
- [Atom and RSS Feeds](feeds.md) — publishing a feed from your own models
- [Internationalization (i18n)](i18n.md) — translating an app and serving it per locale
- [Per-User Time Zones](time-zones.md) — rendering every timestamp in the reader's own zone

## Performance and caching

- [Cache Coherence](cache-coherence.md) — `autumn cache audit`: failing the build when a write can leave a cached read stale
- [Cache Stampede Protection](cache-stampede.md) — stopping a cache miss from becoming a thundering herd
- [Compile-Time Query Budgets](query-budgets.md) — `#[query_budget(N)]`: catching N+1 regressions at build time
- [Capacity Contracts](capacity-contracts.md) — declaring what a route is allowed to consume
- [Server-Timing](observability/server-timing.md) — per-phase request timings the browser shows in devtools
- [Dev-Loop Latency Budget](dev-loop-latency.md) — the p50/p95 budgets for `autumn dev`, and how they are measured

## Testing

- [Integration Testing](testing.md) — driving your own routes and models from a test
- [System Tests](system-tests.md) — browser tests against a running app
- [Simulation Testing](simulation-testing.md) — deterministic, seed-replayable tests over virtual time
- [Failure Capsules](failure-capsules.md) — recording a failing request and replaying it offline
- [Macro Transparency](macro-transparency.md) — seeing exactly what each Autumn macro expands to

## Developer tools

- [The Data Playground](console.md) — `autumn console`, the pre-wired edit-and-run REPL
- [Dev Request Inspector](dev-inspector.md) — the in-browser view of recent requests
- [Dev Error Overlay](dev-error-overlay.md) — the error page that shows the failing source line
- [The Architecture Graph](architecture-graph.md) — `autumn graph impact Post`: what a change touches, as a query

## Deploying and operating

- [Deploying an Autumn App](deployment.md) — getting a build onto a server and serving it
- [Staged and Zero-Downtime Deploys](staged-deploys.md) — shipping without dropping requests
- [Fleet Deploys](fleet-deploys.md) — rolling a release across several hosts, one at a time
- [In-Place Upgrades](hot-upgrades.md) — replacing the running binary without losing connections
- [Daemon Mode](daemon.md) — `autumn serve` as a long-running service
- [Maintenance Mode](maintenance-mode.md) — taking the app offline deliberately, with probes still answering
- [Cloud-Native Autumn](cloud-native.md) — containers, Kubernetes, and the twelve-factor surface
- [Embedded Clustering](clustering.md) — two-node clustering with no external dependency
- [Per-Tenant Memory Cells](tenant-cells.md) — bounding per-tenant memory, with deterministic eviction
- [Runtime Configuration](runtime-config.md) — `autumn.toml`, profiles, and environment overrides
- [Health Indicators](health-indicators.md) — what `/health` and `/ready` report, and adding your own check
- [App Metrics](metrics.md) — the metrics Autumn exports, and recording your own
- [Plugin Metrics Sources](metrics-sources.md) — exporting metrics from a plugin
- [Error Reporting](error-reporting.md) — sending exceptions to an external tracker
- [Operator Alerts](operator-alerts.md) — getting paged when the framework detects trouble

## Desktop and mobile apps

- [Desktop Apps with Tauri](tauri.md) — `autumn generate tauri`
- [Mobile Apps with Tauri: In-Process Backend](tauri-mobile-in-process.md) — the backend compiled into the app, against a remote database
- [Mobile Thin-Client Apps with Tauri](tauri-mobile-thin-client.md) — a mobile shell over your hosted server
- [Offline Sync for Tauri Mobile](tauri-mobile-offline-sync.md) — local SQLite with background sync

## Extending Autumn

- [Extensibility](extensibility.md) — the extension points, and which one fits your case
- [Replacing Autumn Subsystems](custom-subsystems.md) — swapping a built-in implementation for your own
- [Sandboxed Plugins](sandboxed-plugins.md) — running an unaudited third-party plugin under a capability sandbox

## Releasing and upgrading

- [Upgrading with `autumn upgrade`](upgrading.md) — applying a release's mechanical API migrations to your own code
- [Docs Smoke Procedure](docs-smoke.md) — the release gate that certifies the first-run path
