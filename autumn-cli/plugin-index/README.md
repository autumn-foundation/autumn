# The Autumn plugin index

`index.toml` is the curated list of Autumn plugins. `autumn plugin list` and
`autumn plugin add` read it before they search crates.io. The CLI embeds the
file, so each CLI release shows the listings that were verified for it.

A listing tells a user four things before they install a plugin:

| Field | What it tells the user |
|---|---|
| `autumn_web` | The `autumn-web` versions the plugin supports. |
| `[plugin.conformance]` | The last `autumn plugin-check` result, and the release it ran on. |
| `trust` | `native` (full trust: native code) or `sandboxed` (with `capabilities`). |
| `tier` | `stable`, or `experimental` with the `experimental_surfaces` it uses. |

## Submit a plugin for listing

You need a published `autumn-plugin-<name>` crate. Then do these steps.

1. Declare the supported range. Implement `Plugin::contract` and return
   `PluginContract::new(env!("CARGO_PKG_NAME")).autumn_web("<range>")`.
   Declare each experimental surface with `uses_experimental`. See
   [The plugin API contract](../../docs/plugins.md#the-plugin-api-contract).
2. Make a small host app that mounts the plugin.
3. Run the conformance check on the current `autumn-web` release:

   ```bash
   autumn plugin-check --plugin-name autumn-plugin-<name> --prefix /<name> \
     --format json > autumn-plugin-<name>.json
   ```

   All checks must pass or skip.
4. Add a `[[plugin]]` table to `index.toml`. Copy the shape below.
5. Write the report into the listing:

   ```bash
   autumn plugin index record --index autumn-cli/plugin-index/index.toml \
     --report autumn-plugin-<name>.json
   ```

   This sets the result, the range, the version and the tier from the report.
   Do not type them by hand.
6. Run the gate. It must pass:

   ```bash
   autumn plugin index check --index autumn-cli/plugin-index/index.toml
   ```

7. Open a pull request. Attach the JSON report.

```toml
[[plugin]]
name = "autumn-plugin-<name>"
description = "One line. Same as the crate description."
origin = "community"
repository = "https://github.com/<you>/autumn-plugin-<name>"
version = "0.1.0"
autumn_web = "0.7"
tier = "stable"
trust = "native"
status = "listed"
prefix = "/<name>"
# sensitive_routes = ["/<name>/admin:Role: admin required"]

[plugin.conformance]
result = "pass"
autumn_web = "0.7.0"
checked = "2026-09-27"
```

## Admission rules

`autumn plugin index check` refuses a listing when:

- The name does not start with `autumn-plugin-` (community listings).
- `autumn_web` is not a Cargo version requirement.
- The status is `listed` and the last conformance run did not pass.
- The tier does not agree with `experimental_surfaces`, or a surface is not a
  known experimental surface.
- A `sandboxed` listing has no `capabilities`, or names an unknown one. A
  `native` listing names any.
- A text field holds a control character.
- The listing was not verified against the current release.

Only a first-party crate that is not a `Plugin` (for example
`autumn-storage-s3`, a `BlobStore`) can be `exempt`. The `plugin-install` CI
gate compiles its mount instead.

## Re-verification

The `plugin_index_reverify_listings` test runs `autumn plugin-check` against
every live listing. CI runs it in the `plugin-install` job of
`.github/workflows/generator-conformance.yml`: on each change to a plugin or
to `autumn-web`, on each release bump, and each week.

On each `autumn-web` release, the maintainer does these steps:

1. Run the test with `PLUGIN_INDEX_REPORTS=<dir>`. It writes one report per
   listing.
2. Record the reports:

   ```bash
   autumn plugin index record --index autumn-cli/plugin-index/index.toml \
     --report <dir>/*.json --exempt autumn-storage-s3
   ```

3. Commit the result with the release.

Until step 2, `autumn plugin index check` fails: each listing was verified on
an older release. The CLI unit tests run the same gate, so a release cannot
ship a stale index.

`record` changes the status by these rules:

| Result | Status before | Status after |
|---|---|---|
| pass | any | `listed` |
| fail | `listed` | `incompatible` |
| fail | `incompatible` or `delisted`, on a later release | `delisted` |

`autumn plugin list` marks an `incompatible` listing, and `autumn plugin add`
refuses it on the failed release and later. A `delisted` listing is not shown.
If crates.io still has the crate, it shows as unlisted.
