# [ERIS-NOTE] CSRF on HTMX Endpoints

The hypothesis that "I can bypass CSRF protection on htmx endpoints by omitting the HX-Request header and submitting a standard form POST" was extensively tested and found to be false. The `CsrfLayer` securely applies validation on all non-safe methods regardless of the presence of htmx-specific headers, and gracefully falls back to checking the URL-encoded body if the header token is absent.

# [ERIS-NOTE] HTMX has_oob_attribute Bypass Injection

The hypothesis that "An attacker can hide an `hx-swap-oob` attribute from `has_oob_attribute` with a malformed HTML comment (e.g. `<!--->`)" was tested and not reproduced.
`has_oob_attribute` consumes the whole comment and resumes scanning at the next tag, so an `hx-swap-oob` attribute after the comment is still detected; text inside the comment is comment content to the browser too, not an active attribute.
When `has_oob_attribute` returns `false`, `HtmxFragments::render_to` wraps the fragment in a server-generated carrier: a `<div hx-swap-oob="...">` for `innerHTML` and the positional strategies (`OobSwap::inserts_child_nodes()`), a `<template hx-swap-oob="...">` otherwise. In both cases the carrier's id and strategy are chosen by the server and escaped, and user text interpolated through Maud is HTML-escaped, so a user cannot inject an `hx-swap-oob` attribute of their own.

# [ERIS-NOTE] Method Override bypasses CSRF checks?

The hypothesis that "A POST request with `_method=DELETE` might bypass CSRF layer if it acts differently" was tested.
The `MethodOverrideLayer` executes but the `CsrfLayer` sits on the outside. The `CsrfLayer` detects the original safe method as `POST`, forcing CSRF token validation. Testing verified that `POST` requests with an overridden `DELETE` method still get correctly rejected with `403 Forbidden` if they lack a valid CSRF token, ensuring no bypass exists.
