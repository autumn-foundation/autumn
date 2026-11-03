# Plan: ship the direct upload controller (issue #3411)

Part of #3393. `direct_upload_input` renders a progress bar, but no script
moves it. Its input has an `aria-label` and no visible `<label>`.

## Goal

Ship the controller in `autumn-widgets.js`. Fix the markup. Keep a working
no-JS path.

## Brainstorming

1. Put the controller in the core of `autumn-widgets.js`. It is same-origin, so
   it works under `script-src 'self'`.
2. Add a new asset file. Rejected: a second request for one widget.
3. Use `fetch` for the presign call and `XMLHttpRequest` for the PUT. `fetch`
   has no upload progress.
4. Presign request: `POST` JSON `{filename, content_type, size}` with the CSRF
   header from `<meta name="csrf-token">`. Response: the envelope plus `key`.
5. The hidden field holds the `key`. The app form submits it. The app then
   calls `complete_direct_upload` (the bind step).
6. Markup: visible `<label for>`, dropzone, `role="progressbar"`, a polite
   status region, an assertive error region.
7. No JS: the input keeps `name`. The app form uses `multipart/form-data`.
   With JS the controller removes `name`, so the bytes go only to storage.
8. A `DirectUploadConfig` sets label, id, `accept` and `multiple`.

## Reverse brainstorming (how to make this fail)

- Send the file twice: once to storage, once with the form. Fix: drop `name`.
- Submit the form while a PUT runs. The key is missing. Fix: block submit and
  say why.
- Show progress that never ends after an error. Fix: hide the bar on error.
- Let a drop bypass `accept`. Fix: check `accept` on drop. The server still
  validates.
- Use a style attribute. The CSP blocks it. Fix: set a CSS variable with CSSOM.
- Announce every percent. Fix: announce only start, end and error.
- Init twice after an htmx swap. Fix: the `once` guard.
- Hide the input in the dropzone. Keyboard users lose it. Fix: keep the real
  input visible and focusable.

## Six hats

- White (facts): the PUT URL, headers and `key` come from the app. The local
  backend signs the content type. The PUT must send the same one.
- Red (feel): users want a bar that moves and a clear error.
- Black (risk): CSRF, CSP, double submit, a key the user did not upload. The
  bind step stays the guard. The controller adds no trust.
- Yellow (gain): one widget stops breaking the no-JS contract. FileField
  and htmx apps reuse the pattern.
- Green (new): `autumn:upload` and `autumn:upload-error` events let apps hook in.
- Blue (process): RED tests first, then the script, then docs and story, then
  review.

## Decisions

- No new feature flag. No new route.
- JS budget: 46 to 50 KiB raw, 14 to 15 KiB gzip.
- Messages are English constants. Localization is a follow-up.
- Tests use `LocalBlobStore` on a temp dir with its signed PUT route.

## Tests

RED first:

1. Markup unit tests: label, no `aria-label`, regions, hooks, `multiple`.
2. Browser tests (Chromium): upload to storage, hidden field, progress,
   drag and drop, presign error, submit block, no-JS multipart submit.
3. Story slug and CSS selector gates.
4. Size budget test.

## Files

- `autumn/src/storage/form_helper.rs`: markup and config.
- `autumn/vendor/autumn-widgets.js`: controller.
- `autumn/src/ui/widgets.css`: dropzone and states.
- `autumn/src/stories/builtin.rs`: story.
- `autumn/tests/integration/widgets_direct_upload_browser.rs`: browser tests.
- `docs/guide/storage.md`, `changelog.d/3411-direct-upload-controller.md`.
