# Cookie Consent

Under ePrivacy and GDPR Art. 7, a site may set a non-essential cookie only
after the visitor agrees, and withdrawing that agreement must be as easy as
giving it. Autumn ships the parts that are easy to get subtly wrong — the
cookie codec, the consent gate, the banner, and the re-prompt on a policy
change — and leaves you the part only you can decide: which of your cookies are
non-essential.

`autumn new` wires all of this into a generated app. This guide explains what
it wired, how to gate your own code on it, and how to add the withdraw flow.

---

## The gate is the compliance, not the banner

The most common mistake is shipping the banner and setting the cookies anyway.
That is non-compliant theater and it is worse than no banner, because it looks
like consent management to an auditor.

The rule is simple: **every non-essential cookie and every third-party script
must be behind a check.**

```rust,ignore
use autumn_web::consent::Consent;

const CONSENT_POLICY_VERSION: u32 = 1;

#[get("/")]
async fn index(consent: Consent) -> Markup {
    html! {
        h1 { "Welcome" }
        @if consent.allows("analytics", CONSENT_POLICY_VERSION) {
            (analytics_snippet())
        }
    }
}
```

`Consent` is an ordinary extractor — it reads the request's `Cookie` header
directly, so it needs no middleware, no layer ordering, and no application
state. With nothing recorded, `allows` returns `false` for every category
except `"necessary"`.

---

## Which cookies are exempt

Session (`autumn.sid`) and CSRF (`autumn-csrf`) cookies are **strictly
necessary**: `SessionLayer` and `CsrfLayer` set them unconditionally,
regardless of the visitor's choice. Consent is not required for them, and
gating them would break login.

`Consent::allows("necessary", _)` always returns `true`, so you can route your
own necessary-cookie call sites through the same check and keep one code path:

```rust,ignore
use autumn_web::consent::{Consent, NECESSARY};

if consent.allows(NECESSARY, CONSENT_POLICY_VERSION) { /* always true */ }
```

Anything that is not required to deliver the page the visitor asked for —
analytics, marketing, A/B assignment persisted across sessions, embedded
third-party players — is not necessary, whatever the vendor's documentation
calls it.

---

## Recording a choice

Two functions build the `Set-Cookie` value:

| Function | Records |
|---|---|
| `accept_all_cookie(&["analytics"], VERSION)` | consent to the listed categories |
| `reject_non_essential_cookie(VERSION)` | an explicit rejection |
| `expire_consent_cookie()` | withdrawal — clears the decision entirely |

A generated app's routes look like this:

```rust,ignore
#[post("/consent/accept")]
async fn consent_accept(headers: HeaderMap) -> impl IntoResponse {
    let cookie = autumn_web::consent::accept_all_cookie(&["analytics"], CONSENT_POLICY_VERSION);
    let target = autumn_web::consent::redirect_target_from_referer(
        headers.get(header::REFERER).and_then(|v| v.to_str().ok()),
    );
    ([(header::SET_COOKIE, cookie)], Redirect::to(&target))
}
```

Two details in there are load-bearing:

- **They are `POST` routes.** A `GET` that changes consent can be triggered by
  a link prefetcher, a browser extension, or a cross-site top-level
  navigation — none of which is the visitor deciding anything. `POST` keeps the
  change behind CSRF protection.
- **`redirect_target_from_referer` is same-origin-clamped.** It returns the
  visitor to the page they were on instead of bouncing them to the homepage,
  and `safe_redirect_target` refuses an off-site destination, so the header
  cannot become an open redirect.

The cookie payload carries the policy version, the accepted categories, and an
RFC 3339 timestamp, and is capped at 180 days — within the usual regulatory
guidance for a consent-decision cookie.

---

## The banner

`inject_consent_banner` is a response-body-splice middleware: it detects an HTML
response and, when the visitor needs prompting, inserts the banner right before
`</body>` — with no per-handler wiring and no change to your `layout()`
signature.

**It deliberately does not inject in three cases**, so do not treat "the layer
is registered" as "every page prompts":

| Case | Why | Consequence |
|---|---|---|
| An htmx **fragment** response (`HX-Request` without `HX-Boosted`) | a fragment has no `</body>`, so the banner would be appended and swapped in beside the one already on the page | the enclosing page prompts; the fragment does not. `Vary: Cookie` is still applied |
| A **static cache hit** with CSRF enforced and no token available | on a pre-rendered `#[static_get]` page `CsrfLayer` never runs, so the banner's buttons would `403` | that visitor is unprompted on that page, and prompted on the first dynamic route they reach |
| A response body over 2 MiB | splicing would mean buffering arbitrarily more | the page is served unmodified |

The static case is the one to plan for: if your landing page is pre-rendered, a
first-time visitor's *first* page will not carry the banner. That is safe —
`Consent::allows` still returns `false`, so nothing non-essential runs — but if
you need the prompt on that exact page, make it dynamic rather than assuming the
layer covered it.

```rust,ignore
const CONSENT_POLICY_VERSION: u32 = 1;

autumn_web::app()
    .routes(routes![index, consent_accept, consent_reject, consent_manage])
    .layer(axum::middleware::from_fn(move |req, next| async move {
        autumn_web::consent::inject_consent_banner(
            req,
            next,
            CONSENT_POLICY_VERSION,
            // `Some(..)` = this app enforces CSRF. Pass `None` only if you
            // have disabled CSRF entirely: the distinction decides what an
            // absent token means, and with `Some` the banner is skipped
            // rather than rendered with buttons that would 403.
            Some(autumn_web::consent::DEFAULT_CSRF_COOKIE_NAME),
            autumn_web::consent::DEFAULT_CSRF_FORM_FIELD,
        )
        .await
    }))
```

Pass your configured names if you customized `security.csrf.cookie_name` or
`security.csrf.form_field`. The middleware reads both cookies off the incoming
request rather than from request extensions, so it does not care where
`CsrfLayer` sits relative to it.

Style it through the framework stylesheet — link
`autumn_web::ui::WIDGETS_CSS_PATH` and override the
`.autumn-consent-banner*` classes if you want your own look.

### What the banner does not do

It renders **two equally-weighted submit buttons** — "Reject non-essential" and
"Accept all" — sharing one CSS class, because rejecting must be no harder than
accepting. It uses plain HTML forms, so it works with JavaScript disabled, and
carries `role="region"` with an `aria-label` so it is reachable and announced.
There is no cookie wall, no pre-ticked box, and no "continue browsing means you
agree".

### Interactions worth knowing about

The middleware is careful in four places, and each one is a bug it is
preventing:

| Behaviour | Why |
|---|---|
| Static/ISR build renders pass through untouched | otherwise a banner and a build-time CSRF token get frozen into `dist/` and served to every visitor forever |
| `If-None-Match` / `If-Modified-Since` are stripped while a prompt is due | otherwise a `304` replays a cached banner-less page and silently skips the prompt after a policy bump |
| An injected response gets `Cache-Control: private, no-store` and `Vary: Cookie` | it now contains a per-visitor CSRF token, which must never reach a shared cache |
| A response already containing the banner marker is left alone | so a "manage preferences" page that renders the banner itself does not get a second copy |

Bodies over 2 MiB are served unmodified rather than buffered — a large page
without the banner beats a dropped one.

---

## Re-prompting after a policy change

The recorded cookie carries the version it was made under. Bump the constant
when your cookie policy changes:

```rust,ignore
const CONSENT_POLICY_VERSION: u32 = 2;   // was 1
```

A decision recorded under an older version is treated as undecided:
`needs_prompt` becomes `true`, the banner reappears, and `allows` returns
`false` for every non-necessary category until the visitor decides again. The
gate closes first and reopens only on a fresh decision — there is no window in
which the old consent is still honored for a policy it did not cover.

Bump it when you add a category, add a vendor, or change what an existing
category does. Do not bump it for unrelated releases; a re-prompt the visitor
cannot account for is its own dark pattern.

---

## The withdraw flow

Art. 7(3) requires withdrawal to be as easy as consent. That means a link on
every page — a footer link is the convention — leading to a page where the
choice can be changed.

Make the *page* a `GET` and the *change* a `POST`. A generated app ships
`GET /consent/manage`, which re-renders the same banner widget:

```rust,ignore
#[get("/consent/manage")]
async fn consent_manage(csrf: Option<CsrfToken>) -> impl IntoResponse {
    let markup = layout("Manage cookie preferences", html! {
        (autumn_web::consent::consent_banner_markup(
            csrf.as_ref().map(CsrfToken::token),
            autumn_web::consent::DEFAULT_CSRF_FORM_FIELD,
        ))
    });
    (
        [
            (header::CACHE_CONTROL, "private, no-store"),
            (header::VARY, "Cookie"),
        ],
        markup,
    )
}
```

The two headers are not optional: the page embeds the visitor's own live CSRF
token, so a shared cache serving it to someone else would leak that token and
break every other visitor's consent form.

`DEFAULT_CSRF_FORM_FIELD` is shown here because it is what most apps run. It is
the literal default, not a lookup — if you set `security.csrf.form_field`, pass
the `CsrfFormField` extractor's value instead, to both `consent_banner_markup`
and any hidden input you write yourself. `CsrfLayer` scans a URL-encoded body
for the configured name only, so a widget named from the constant renders fine
and `403`s on every button. `examples/reddit-clone`'s preferences page takes
that extractor for exactly this reason.

Reuse `consent_banner_markup` rather than writing a second set of buttons. The
injector detects the marker and skips its own injection, so the page shows one
banner, and the accept/reject semantics stay identical to the prompt.

To offer a full withdrawal — back to undecided, banner returns on the next
page — set `expire_consent_cookie()` from a `POST` route.

---

## Testing it

```rust,ignore
// An undecided visitor is prompted.
app.get("/").send().await.assert_body_contains("autumn-consent-banner");

// A rejecting visitor is not, and analytics stays off.
let res = app.post("/consent/reject").form("").send().await;
let cookie = res.header("set-cookie").expect("consent cookie");
app.get("/").header("cookie", cookie).send().await
   .assert_body_contains("Welcome")
   .assert_no_selector(".autumn-consent-banner");
```

The empty `form("")` works because `TestApp::new()` disables CSRF by default,
the way Spring Security's test support does — so the POST reaches
`consent_reject` and comes back with the consent cookie. If you re-enable CSRF
in a test (`config.security.csrf.enabled = true`), that same POST is rejected
with a `403` *before* the handler runs, and the rejection carries no consent
cookie, so the `set-cookie` lookup fails rather than the assertion. Fetch a page
first, read the token out of it, and submit it in the configured form field.

The assertion worth writing is the *negative* one: that the analytics snippet
is absent for a rejecting visitor. The banner being present is easy to get
right; the gate being honored is the thing that regresses.

---

## Checklist

- [ ] Every non-essential cookie and third-party script is behind
      `consent.allows(category, VERSION)`.
- [ ] Categories in `accept_all_cookie` match the categories you gate on.
- [ ] Accept and reject are `POST` routes; only the preferences *page* is a `GET`.
- [ ] A withdraw link is reachable from every page.
- [ ] `CONSENT_POLICY_VERSION` is bumped when the policy changes.
- [ ] Pages that embed the banner are not publicly cacheable.

---

## See also

- [Getting started](./getting-started.md) — what `autumn new` scaffolds
- [Middleware](./middleware.md) — where `inject_consent_banner` sits in the stack
- [Security posture manifest](./security-posture-manifest.md) — proving the
  posture in CI
- [Audit logging](./audit-logging.md) and [retention sweeps](./retention-sweeps.md)
  — the other halves of a privacy posture
- [Accessibility](./accessibility.md) — the banner's keyboard and screen-reader
  contract
