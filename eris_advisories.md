# [ERIS-NOTE] CSRF on HTMX Endpoints

The hypothesis that "I can bypass CSRF protection on htmx endpoints by omitting the HX-Request header and submitting a standard form POST" was extensively tested and found to be false. The `CsrfLayer` securely applies validation on all non-safe methods regardless of the presence of htmx-specific headers, and gracefully falls back to checking the URL-encoded body if the header token is absent.

# [ERIS-NOTE] HTMX has_oob_attribute Bypass Injection

The hypothesis that "An attacker can bypass `has_oob_attribute` using malformed HTML comments to inject `hx-swap-oob` attributes" was tested.
The `has_oob_attribute` function parses `<!--->` as a valid comment, skipping the check, and allowing `has_oob_attribute` to return `false` while the string actually contains an `hx-swap-oob="true"` attribute.
However, if `has_oob_attribute` returns `false`, `HtmxFragments::render_to` wraps the fragment in a server-generated `<template hx-swap-oob="...">` tag. This server-side logic is not exploitable directly via user-input because the `id` and strategy for the swap are determined by the server and are strictly escaped. If a user inputs an `hx-swap-oob` attribute via template, Maud's automatic HTML escaping mitigates the injection entirely.


# [ERIS-NOTE] Method Override bypasses CSRF checks?

The hypothesis that "A POST request with `_method=DELETE` might bypass CSRF layer if it acts differently" was tested.
The `MethodOverrideLayer` executes but the `CsrfLayer` sits on the outside. The `CsrfLayer` detects the original safe method as `POST`, forcing CSRF token validation. Testing verified that `POST` requests with an overridden `DELETE` method still get correctly rejected with `403 Forbidden` if they lack a valid CSRF token, ensuring no bypass exists.


# [ERIS-NOTE] Session Fixation and CSRF

The hypothesis that "Session tokens might be generated insecurely or susceptible to timing attacks" was investigated.
Maud templates strictly escape all outputs. Any fragments created with `HtmxFragments` have the strategy carefully mapped out server-side.
Therefore, HTML injections via Htmx `hx-swap-oob` templates can't affect the structure of the `<template>` wrapper due to strict parsing.

We were unable to find any actionable exploits across the top 3 layers: HTTP/Axum Foundation, Maud Templating, and HTMX Integration.
