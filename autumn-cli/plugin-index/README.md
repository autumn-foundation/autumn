# The Autumn plugin index

`index.toml` is the curated list of Autumn plugins. `autumn plugin list` and
`autumn plugin add` read it before they search crates.io. The CLI embeds the
file, so each CLI release shows the listings that were verified for it.

A listing tells a user four things before they install a plugin:

| Field | What it tells the user |
|---|---|
| `autumn_web` | The `autumn-web` versions the plugin supports. |
| `[plugin.conformance]` | The last `autumn plugin-check` result, and the release it ran on. |
| `trust` | `native` (full trust: native code), or `sandboxed` with its `capabilities`, `grants`, `quotas` and `artifact_sha256`. |
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
     --sensitive-route /<name>/admin:"Role: admin required" \
     --format json > autumn-plugin-<name>.json
   ```

   All checks must pass or skip. Give one `--sensitive-route` for each
   admin, debug, credential, operator, secret or metrics route. CI uses the
   same values from your listing.
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

7. Open a pull request. CI runs `autumn plugin-check` on your listing again.
   The result must agree with the recorded one. A report you edit by hand
   does not get past this step.

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
- Two listings name one crate. Case and `-`/`_` do not count, as on
  crates.io.
- `autumn_web` is not a Cargo version requirement, or it has no upper bound
  (`*`, `>=0.6`).
- The status is `listed` and the last conformance run did not pass.
- The tier does not agree with `experimental_surfaces`, or a surface is not a
  known experimental surface.
- A `sandboxed` listing has no `capabilities`, names an unknown one, or has
  no 64-hex `artifact_sha256`. A `native` listing has either field.
- A text field holds a control, bidi or zero-width character.
- The listing was not verified against the current release.

`record` also refuses a passing report with no `plugin-contract` check or no
declared range.

Only a first-party crate that is not a `Plugin` can be `exempt`. An example is
`autumn-storage-s3`, a `BlobStore`. The re-verification compiles its mount
instead.

## Re-verification

The `plugin_index_reverify_listings` test runs `autumn plugin-check` against
every live listing. CI runs it in the `plugin-install` job of
`.github/workflows/generator-conformance.yml`. It runs on each change to this
index, to a first-party plugin or to `autumn-web`, on each release bump, and
each week. It skips a sandboxed listing, because it cannot fetch the
artifact. For a sandboxed listing, run
`autumn plugin inspect <file>.autumn-plugin --format json > hello.json`, then
`autumn plugin index record --inspect hello.json`. This copies the
capabilities, the scoped grants (hosts, tables, job types, render slots), the
quotas and the artifact digest from the artifact. For a new version of a listed artifact,
run `inspect` with `--against <old>.autumn-plugin`. `record` refuses a new
artifact without that baseline. If the new version asks for more authority,
`record` flags the listing and keeps the old artifact.

The job uploads the `plugin-index-reports` artifact. It holds one report per
listing and `index.toml`: the index with the reports already recorded.

On each `autumn-web` release, the maintainer does these steps:

1. Get the artifact, or run the test locally with
   `PLUGIN_INDEX_REPORTS=<dir>`.
2. Copy its `index.toml` over `autumn-cli/plugin-index/index.toml`. To record
   by hand instead:

   ```bash
   autumn plugin index record --index autumn-cli/plugin-index/index.toml \
     --report <dir>/*.json --exempt autumn-storage-s3
   ```

3. Run `autumn plugin index check`, then commit the result with the release.

Until step 2, `autumn plugin index check` fails: each listing was verified on
an older release. The CLI unit tests run the same gate, so a release cannot
ship a stale index.

`record` changes the status by these rules:

| Result | Status before | Status after |
|---|---|---|
| pass | any | `listed` |
| fail | `listed` | `incompatible` |
| fail | `incompatible`, same release (a retry) | `incompatible` |
| fail | `incompatible`, a different release | `delisted` |
| fail | `delisted` | `delisted` |

`autumn plugin list` marks an `incompatible` listing. `autumn plugin add`
refuses it on the failed series and later series, and when it cannot read the
app's `autumn-web` version. A `delisted` listing is not shown. If crates.io
still has the crate, it shows as unlisted.

`autumn plugin add` pins a listed community crate to its verified version
with `=`. A later patch release is not verified, so Cargo does not take it.
