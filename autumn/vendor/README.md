# Vendored Dependencies

## htmx

- **Version:** 2.0.4
- **Source:** <https://unpkg.com/htmx.org@2.0.4/dist/htmx.min.js>
- **License:** BSD 2-Clause (see <https://github.com/bigskysoftware/htmx/blob/master/LICENSE>)

### How to update

1. Choose the new version at <https://unpkg.com/htmx.org/>.
2. Download the minified build:

   ```sh
   curl -sL https://unpkg.com/htmx.org@<VERSION>/dist/htmx.min.js \
        -o autumn/vendor/htmx.min.js
   ```

3. Update `HTMX_VERSION` in `autumn/src/htmx.rs` to match.
4. Run `cargo test --workspace` to verify the embed still works.
5. Commit both the JS file and the version bump together.

## Autumn WebMCP shim

- **File:** `autumn-webmcp.js`. Autumn's own code, not a third-party file.
- **Tracks:** the W3C WebMCP community-group draft of 2026-04-23
  (<https://webmachinelearning.github.io/webmcp/>).
- **Served at:** `/static/js/autumn-webmcp.js` and its hashed URL, with the
  `webmcp` feature.

### How to update

1. Read the draft change log. Change the shim when the API changes.
2. Change the draft date in each file that names it
   (`grep -rn 2026-04-23 autumn docs STABILITY.md`).
3. Run `node autumn/tests/fixtures/webmcp/shim_harness.js autumn/vendor/autumn-webmcp.js`.
4. Run `cargo test -p autumn-web --features webmcp --test integration_tests webmcp`.

## Swagger UI

- **Version:** 5.32.4
- **Source:** <https://unpkg.com/swagger-ui-dist@5.32.4/swagger-ui.css>
- **Source:** <https://unpkg.com/swagger-ui-dist@5.32.4/swagger-ui-bundle.js>
- **License:** Apache-2.0 (see <https://github.com/swagger-api/swagger-ui/blob/master/LICENSE>)

### How to update

1. Download the pinned distribution assets:

   ```sh
   curl -sL https://unpkg.com/swagger-ui-dist@<VERSION>/swagger-ui.css \
        -o autumn/vendor/swagger-ui/swagger-ui.css
   curl -sL https://unpkg.com/swagger-ui-dist@<VERSION>/swagger-ui-bundle.js \
        -o autumn/vendor/swagger-ui/swagger-ui-bundle.js
   ```

2. Update `SWAGGER_UI_VERSION` in `autumn/src/openapi.rs` to match.
3. Run the OpenAPI and security tests to verify the same-origin UI still works under the default CSP.

## axe-core (test only)

- **Version:** 4.14.0
- **Source:** <https://registry.npmjs.org/axe-core/-/axe-core-4.14.0.tgz>
  (`package/axe.min.js`, `package/LICENSE`)
- **Integrity (npm):** `sha512-9WTZxEjsZ7b13TH8JPmbV2z8CHbl80/2hm3XPEG4JgNdQLK81IBRXmSxHfMAOkSqQeRxT/0dwNDz2GOm3zzpcQ==`
- **SHA-256 of `axe.min.js`:** `20c09fe157a8a34a30e241aaa1fcdade657734f08ab379ecfbeb7d45cc46e878`
- **License:** MPL-2.0 (see `autumn/tests/fixtures/axe-core/LICENSE`)
- **Location:** `autumn/tests/fixtures/axe-core/`. It is not served, not
  embedded and not published (`exclude` in `autumn/Cargo.toml`). Only the
  `stories_browser` tests load it (issue #3398).

### How to update

1. Download the tarball. Compare
   `echo "sha512-$(openssl dgst -sha512 -binary axe-core-<VERSION>.tgz | base64 -w0)"`
   with `npm view axe-core@<VERSION> dist.integrity`.
2. Copy `package/axe.min.js` and `package/LICENSE` to
   `autumn/tests/fixtures/axe-core/`.
3. Update the version, the integrity and the SHA-256 above
   (`sha256sum autumn/tests/fixtures/axe-core/axe.min.js`).
4. Run the `stories_browser` suite (see CONTRIBUTING.md "Widget browser gate").
   Fix each new violation in the widget. Do not disable a rule.

