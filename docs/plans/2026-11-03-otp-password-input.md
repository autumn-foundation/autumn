# Plan: OTP input and password reveal / strength (issue #3412)

Part of #3393, Phase 2. Both helpers stay in core: they are small, and the
auth generator emits them.

## Goal

Add `otp_input`: one real input that looks like separate boxes. Give
`password_input` an `autocomplete` option, a reveal button and a strength
hint. Make the auth generator use both.

## Brainstorming

1. `otp_input` is one `<input type="text" autocomplete="one-time-code"
   inputmode="numeric">`. One input keeps SMS autofill and paste working.
   A screen reader reads one field, not six.
2. The boxes are `aria-hidden` `<span>` cells behind a transparent input.
   The input uses a monospace font and a letter spacing equal to the cell
   pitch, so each digit sits in its box. CSS only.
3. `OtpFormat::digits(n)` sets the length (4 to 10, default 6).
   `.group(g)` groups the digits (2 to 5).
4. The server sends `pattern="[0-9]{n}"`, so a browser without JS still
   checks the code on submit.
5. The JS keeps digits only and cuts the value to `n` digits on each
   `input` event. A paste of `123-456` or `Code: 123456` gives `123456`.
   The server also sends `maxlength` for a page with no JS; JS removes it.
6. Grouping needs JS. The runtime puts a space between groups, shows the
   separator cells and changes `pattern`. The `formdata` event removes the
   spaces, so the server gets digits only. Without JS, the boxes do not
   group.
7. The OTP value is never pre-filled. A code is used once.
8. `FieldOptions::current_password()` and `new_password()` set
   `autocomplete`. They are typed names for the two tokens.
9. `FieldOptions::reveal()` adds a `<button type="button" aria-pressed>`
   after the input. It has `hidden` in the server HTML. The runtime shows
   it and toggles `type` between `password` and `text`.
10. `FieldOptions::strength_hint()` adds `<p id="{field}-strength"
    aria-live="polite">`. Its id goes in `aria-describedby`. The runtime
    writes a level (Weak, Fair, Good, Strong) as the user types.
11. `reveal_label` and `strength_labels` set the text for translation.
12. `FieldOptions::minlength` and `maxlength`. The generated signup form
    sends `minlength`, so the helper must too.
13. `FieldControl::Otp(OtpFormat)`, so `form_for` can render a code field.
14. A "Sign-in inputs" story. Its keyboard test pastes into the OTP field
    and toggles the reveal button.
15. The auth generator uses `password_input_with` for each password field,
    and `otp_input_with` for the two 6-digit fields on `/account/2fa`.

## Reverse brainstorming

Question: "How can these helpers hurt a user or leak a secret?" Each answer
gives a guard.

| Way to fail | Guard |
|---|---|
| A revealed password stays visible after Back (bfcache). | Hide on `pagehide`. Browser test. |
| A revealed password goes into the browser form history as text. | Hide on `submit` (capture phase). Browser test. |
| An htmx history snapshot keeps `type="text"`. | Hide on `htmx:beforeHistorySave`. |
| The reveal button submits the form. | `type="button"`. Unit test. |
| The reveal button shows without JS and does nothing. | `hidden` in the server HTML. Unit and browser test. |
| The button label changes on toggle, so a screen reader hears a new control. | The label stays. `aria-pressed` gives the state (APG). |
| `maxlength` cuts a paste of `123 456` to `123 45`. | The server sends `maxlength` for no-JS pages. JS removes it, then keeps digits. |
| A pasted code with a dash or text fills the wrong digits. | JS removes each non-digit. Browser test. |
| A grouped value `123 456` fails `pattern` or reaches the server. | JS changes `pattern`. `formdata` removes spaces. Browser test. |
| A recovery code (`a1b2c-3d4e5`) goes into a digits-only field. | The generator keeps the text input on `/login/verify` and `/reauth`. |
| An app sets `inputmode` or `autocomplete` on the OTP field: a duplicate attribute. | The app option replaces the default, one time only. Unit test. |
| The OTP value shows again after a 422. | The OTP field never renders `value`. Unit test. |
| A strength hint reads as a policy. | Docs: it is a hint. The server checks the policy. |
| `aaaaaaaaaaaaaaaa` scores Strong. | One kind of character, or three in a row, removes a point. |
| An IME writes digits in steps, so the format runs too early. | Skip `isComposing`; format on `compositionend`. |
| A revealed password goes to a cloud spellcheck. | `spellcheck="false"` when the reveal button is on. |
| The boxes make a 320 px page scroll sideways. | The box size follows the viewport width. |
| An htmx request with no form sends `123 456`. | Strip the spaces on `htmx:configRequest` too. |
| The live region speaks on each key. | The text changes only when the level changes. |
| A long label breaks the strength JSON. | `serde_json` writes the labels; the writer escapes the attribute. |
| A digit is not in its box (font, RTL). | Monospace font, `direction: ltr`. A manual Chromium screenshot in the light and the dark scheme. |
| Chromium autofill paints a box over the cells. | A long `background-color` transition on `:autofill`. |
| The default `password_input` output changes and breaks apps or the alloc gate. | No option set: the markup is the same bytes. Existing tests. |
| A new class has no CSS rule. | `widget_css_coverage`. |
| The JS budget grows past its limit. | Raise it with a reason in the test doc. |

## Six thinking hats

- **White (facts).** `render_field` writes every core field. The generated
  layout loads `autumn-widgets.js` and `widgets.css`. The generated auth
  forms use raw `input` tags and no changeset. `verify_totp_code` only trims
  the code. The JS bundle is 46,140 of 47,104 budget bytes. htmx 2 builds a
  request with `new FormData(form)`, which fires `formdata`.
- **Red (feeling).** An app author wants one call for a code field, with
  the same shape as `range_input`. A user wants paste and autofill to work.
- **Black (risk).** Glyph alignment depends on the font. The generator
  change touches many tests. Grouping depends on JS. A reveal toggle can
  leak a password on a shared screen.
- **Yellow (value).** OTP inputs ship in 4 suites. The auth scaffold gets a
  reveal button, a strength hint and correct autocomplete tokens.
- **Green (alternatives).** Six split inputs (rejected: breaks autofill and
  paste, and gives six fields to a screen reader). A JS-drawn overlay
  (rejected: more JS, no-JS fallback is worse). Boxes as a background
  gradient (rejected: groups need per-size stops). A server strength score
  (rejected: the hint must update as the user types).
- **Blue (process).** SPEC: a Verus spec (`verification/otp_format.rs`)
  proves the `OtpFormat` bounds and the cell count. A proptest checks the
  runtime against the same rules. RED: unit tests for each option, the story,
  the browser test, the generator tests. GREEN: the smallest code that
  passes. REFACTOR: share the option writer. Then a review from several
  angles, and an acceptance check.

## API

| Item | Notes |
|---|---|
| `otp_input(cs, field, label)` | 6 digits |
| `required_otp_input(cs, field, label)` | |
| `otp_input_with(cs, field, label, format, options)` | `OtpFormat::digits(n).group(g)` |
| `FieldControl::Otp(OtpFormat)` | `form_for` arm |
| `FieldOptions::current_password()` / `new_password()` | `autocomplete` |
| `FieldOptions::reveal()` / `reveal_label(text)` | password only |
| `FieldOptions::strength_hint()` / `strength_labels([..; 4])` | password only |
| `FieldOptions::minlength(n)` / `maxlength(n)` | text, password, textarea, email, tel, url |
