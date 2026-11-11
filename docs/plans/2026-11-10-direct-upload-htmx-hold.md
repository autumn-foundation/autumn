# Plan: hold htmx requests during a direct upload

The direct upload controller holds a form while an upload runs. It listens
for `submit`. An `hx-post` button does not fire `submit`: htmx sends the form
on `click`. A click on Save during an upload then sends the form with no blob
key. The response can also replace the upload widget.

## Goal

Hold each htmx request that sends a form with a running upload. Tell the
user why. Keep all other htmx requests as they are.

## Brainstorming

1. Listen for `htmx:beforeRequest` on the document. Call `preventDefault()`.
   htmx then sends no request and releases its request lock.
2. Use the htmx 2 rule to find the forms that a request sends:
   - the requester, if it is a form;
   - the closest form, if the verb is not `GET`;
   - each `hx-include` target, and the fields in it.
3. Resolve `hx-include` with the resolver that `hx-indicator` uses now.
   Make it general: one attribute name in, the elements out.
4. Share one `hold()` function with the `submit` guard. It writes the alert
   line and moves focus to the file input.
5. Listen in the capture phase and stop the event. Then no `hx-on` handler
   starts a spinner for a request that does not occur.
6. Alternative: listen for `click` on submit buttons. Rejected: htmx can
   send on any trigger, and a `type="button"` button also sends the form.
7. Alternative: add the key to the request later. Rejected: the key does
   not exist until the upload ends.

## Reverse brainstorming (how to make this fail)

- Hold only `type="submit"` buttons. `ButtonConfig::new()` makes
  `type="button"`, so an `hx-post` Save button gets through. Fix: use the
  htmx rule, not the button type.
- Hold an `hx-get` button. A Cancel or Preview link stops for no reason.
  Fix: a `GET` request does not send the closest form. It sends only a
  requester form and the `hx-include` targets.
- Move focus on each held request. Live validation on `keyup` then moves
  focus away from the text field while the user types. Fix: show the alert
  and move focus only for a `click` or a `submit`. Hold other requests with
  no message.
- Forget `hx-include`. A button outside the form with `hx-include="#f"`
  gets through. Fix: resolve `hx-include`.
- Do not stop the event. An `hx-on::before-request` spinner then starts and
  never stops. Fix: `stopImmediatePropagation()`.
- Hold a request on `autumn:upload`. The widget fires it while it is still
  busy, so an autosave never sends the key. Fix: let a request on an
  `autumn:upload` event go.
- Wait for nothing. A held request is cancelled, not queued. Fix: say so in
  the docs. Tell autosave forms to add `autumn:upload` to the trigger.
- Go over the JS size budget. The bundle has 13 bytes free. Fix: reuse the
  `hx-indicator` resolver and the `submit` guard code. Raise the budget with
  a reason.

## Six hats

- White (facts): htmx 2.0.4 fires `htmx:beforeRequest` on the requester
  before `htmx:beforeSend`. The detail has `elt` and `requestConfig` with
  `verb` and `triggeringEvent`. A cancel ends the request lock.
- Red (feel): a user who clicks Save wants the same message as a plain form.
- Black (risk): a too-wide guard stops requests that do not send the upload.
  A too-narrow guard sends a form with no key. Tests cover both sides.
- Yellow (gain): the htmx path and the plain form path act the same.
- Green (new): the guard also covers `hx-include` from outside the form.
- Blue (process): RED browser tests first, then the guard, then docs,
  then review.

## Decisions

- No new attribute, event or config.
- The message is the same English text as the `submit` guard.
- A held request that no user action started gets no message and no focus
  move.

## Tests

RED first, in `widgets_direct_upload_browser.rs`:

1. An `hx-post` submit button during an upload: no request, the alert,
   focus on the file input. After the upload, the click sends the key.
2. A `type="button"` `hx-post` button from `ButtonConfig` is held.
3. A button outside the form with `hx-include` is held.
4. Live validation (`hx-post` on `keyup`) is held with no message and
   no focus move.
5. An `hx-get` button in the form is not held.
6. `data-hx-include="closest <main/>"` is held.
7. A button with no include, outside the form, is not held.
8. An autosave form is held on `change` and sends the key on
   `autumn:upload`.
9. A static test in `htmx.rs` checks for the `htmx:beforeRequest` guard.

## Files

- `autumn/vendor/autumn-widgets.js`: the guard.
- `autumn/src/htmx.rs`: the static test and the size budget.
- `autumn/tests/integration/widgets_direct_upload_browser.rs`: browser tests.
- `autumn/src/storage/form_helper.rs`, `docs/guide/storage.md`,
  `docs/guide/keyboard.md`: docs.
- `changelog.d/3601-direct-upload-htmx-hold.md`: the release note.
