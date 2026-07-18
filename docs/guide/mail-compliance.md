# Mail compliance: List-Unsubscribe

Since February 2024, Gmail and Yahoo require senders shipping **bulk** mail to
include [RFC 8058](https://www.rfc-editor.org/rfc/rfc8058) one-click
`List-Unsubscribe` headers. Mail without them gets reputation-downgraded,
throttled, and eventually dropped.

Autumn makes this a one-line opt-in on `#[mailer]`. Declare a list once and the
framework:

- emits both `List-Unsubscribe` and `List-Unsubscribe-Post:
  List-Unsubscribe=One-Click` headers on every send,
- signs a short-lived, stateless unsubscribe token with your app signing key,
- offers an opt-in default one-click unsubscribe endpoint (no end-user auth,
  rate limited, CSRF/CAPTCHA-exempt),
- records opt-outs in a suppression table and **skips** suppressed recipients on
  future sends,
- shows the resulting headers and signed link in the dev mail preview.

Apps that never touch the attribute are completely unaffected — no new required
config, no behavior change.

## Which mailers should set `list_unsubscribe`?

Set it for **bulk / marketing-style** mail:

- ✅ Newsletters
- ✅ Digests (daily/weekly summaries)
- ✅ Drip campaigns / onboarding sequences
- ✅ Batch product-update or announcement notifications

**Never** set it for transactional mail the user cannot opt out of, per Gmail's
bulk-sender guidance:

- ❌ Password reset emails
- ❌ MFA / one-time codes
- ❌ Security alerts (new sign-in, password changed)
- ❌ Receipts and other legally required notices

Adding `List-Unsubscribe` to a password reset invites users to "unsubscribe"
from mail they actually need — keep those mailers plain.

## 15-minute setup

### 1. Scaffold a list mailer

```bash
autumn generate mailer WeeklyDigest --list-unsubscribe weekly_digest
```

This writes a mailer with the attribute already set:

```rust
#[mailer(list_unsubscribe = "weekly_digest")]
impl WeeklyDigestMailer {
    pub fn digest(&self, to: String) -> Mail {
        Mail::builder()
            .to(to)
            .subject("Weekly Digest")
            .html(include_str!("../../templates/mailers/weekly_digest.html"))
            .text(include_str!("../../templates/mailers/weekly_digest.txt"))
            .build()
            .expect("valid mail")
    }
}
```

and a `migrations/<ts>_create_mail_unsubscribes/` migration provisioning the
suppression table (keyed by `subscriber`, `list_id`, `unsubscribed_at`).

> Already have a mailer? Just add the `list_unsubscribe = "..."` argument and run
> the generator once more (or add the migration by hand) — the migration step is
> idempotent and skips creation when a `*_create_mail_unsubscribes` migration
> already exists.

### 2. Configure where unsubscribe links point

```toml
[mail]
transport = "smtp"
from = "Acme <news@example.com>"
# At least one of these is required for list mailers:
unsubscribe_base_url = "https://app.example.com"
unsubscribe_mailto  = "unsubscribe@example.com"
# Optional: token validity window (days). Default: 30.
unsubscribe_token_ttl_days = 30
```

The header includes the HTTPS one-click URL and, when set, a `mailto:` fallback:

```
List-Unsubscribe: <https://app.example.com/_autumn/unsubscribe?token=...>, <mailto:unsubscribe@example.com?subject=unsubscribe>
List-Unsubscribe-Post: List-Unsubscribe=One-Click
```

> **Fail closed.** In production, startup (and `autumn doctor --strict`) fails
> when a `#[mailer]` declares `list_unsubscribe` but neither
> `unsubscribe_base_url` nor `unsubscribe_mailto` is configured — so you can't
> ship non-compliant bulk mail by accident.

To serve the one-click link, opt into the built-in endpoint on the builder
(see [The unsubscribe endpoint](#the-unsubscribe-endpoint)):

```rust
autumn_web::app().mount_unsubscribe_endpoint() // ...
```

### 3. Send

Use the generated helper as usual:

```rust
WeeklyDigestMailer.send_digest(&mailer, "ada@example.com".to_owned()).await?;
```

List mailers are delivered **one recipient per message** so each unsubscribe
link is personalized. Recipients already in the suppression table are skipped
with a structured log event (`target: "mail", outcome = "skipped_suppressed"`).

> **Deferred delivery.** Suppression filtering and header signing happen inside
> `Mailer::send`. The in-process `deliver_later` fallback re-enters `send`, so it
> is covered automatically. If you register a durable
> [`MailDeliveryQueue`](mail.md#deferred-delivery), make its worker deliver by
> calling `Mailer::send` (the standard pattern) rather than invoking a transport
> directly — otherwise queued list mail would bypass suppression and the
> List-Unsubscribe headers.

## How tokens work

Unsubscribe tokens are **stateless and short-lived**:

`base64url(subscriber).base64url(list_id).expiry.HMAC-SHA256`

The HMAC is computed over `subscriber.list_id.expiry` with your app signing key
([signing secrets](signing-secrets.md), rotation-aware). There is **no token
table** — verification is pure and replica-safe. The raw subscriber identifier
appears only inside the opaque, signed token and is revealed only on successful
verification, never as a plain URL parameter. Tokens expire after
`unsubscribe_token_ttl_days` (default 30).

## The unsubscribe endpoint

The default endpoint is **opt-in** — a plain JSON API never gets an HTML
endpoint it didn't ask for. Enable it on the builder (requires
`mail.unsubscribe_base_url`):

```rust
autumn_web::app()
    .mount_unsubscribe_endpoint()
    // ...
    .run()
    .await;
```

(equivalently `mail.mount_unsubscribe_endpoint = true` in `autumn.toml`). When
mounted, Autumn serves:

- `POST /_autumn/unsubscribe?token=...` — the RFC 8058 one-click flow. Verifies
  the token, requires the `List-Unsubscribe=One-Click` body, records the
  suppression, returns a confirmation page. No end-user auth; covered by the
  global rate-limit layer and automatically exempted from CSRF and CAPTCHA
  (mailbox-provider POSTs carry neither token).
- `GET /_autumn/unsubscribe?token=...` — a minimal confirmation page with a
  one-click form, for users who click through.

**Serve a custom page instead** by not calling `mount_unsubscribe_endpoint()`
and registering your own route at `/_autumn/unsubscribe` (e.g. a branded
preference center). Your handler keeps its own CSRF/CAPTCHA protections.

## Suppression storage

The suppression list lives in **your app database** — no external ESP
dependency. With the `db` feature and a configured pool, Autumn auto-wires a
Diesel-backed `DbSuppressionStore` over the `mail_unsubscribes` table. To plug a
custom backend (e.g. Redis), implement `SuppressionStore` and register it:

```rust
autumn_web::app()
    .with_suppression_store(MyRedisSuppressionStore::new(redis))
    // ...
    .run()
    .await;
```

The table:

```sql
CREATE TABLE mail_unsubscribes (
    id BIGSERIAL PRIMARY KEY,
    subscriber TEXT NOT NULL,
    list_id TEXT NOT NULL,
    unsubscribed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (subscriber, list_id)
);
```

## Verify before you deploy

Open the [dev mail preview](mail.md#previewing-emails-in-dev) at `/_autumn/mail`
and view your list mailer: the **Headers** section shows the rendered
`List-Unsubscribe` / `List-Unsubscribe-Post` headers and the signed unsubscribe
link, so you can confirm the wiring without sending a single message.

Run `autumn doctor --strict` as a pre-deploy gate — it fails when a list mailer
has no unsubscribe destination configured.

## Out of scope

List-Unsubscribe compliance is one slice of deliverability. Not handled here:
DKIM/SPF/DMARC (belongs to your MTA/DNS), bounce/complaint suppression,
Postmaster analytics, and marketing-grade preference centers.
