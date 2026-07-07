# Mobile Thin-Client Apps with Tauri (`autumn generate tauri --remote-url`)

`autumn generate tauri --remote-url <URL>` scaffolds a `src-tauri/` sub-project
for the **mobile thin-client** architecture: the app you install on a phone is a
native Tauri shell whose webview loads your **cloud-hosted Autumn server**
directly over HTTPS. There is no local sidecar binary, no bundled database, and
no build-time staging step — your existing routes, Maud/htmx templates, and
sessions run unmodified on the server, exactly as they do for browser users.

```
┌─────────────────────────────┐          ┌──────────────────────────┐
│  Phone (Android / iOS)      │          │  Cloud                   │
│  ┌───────────────────────┐  │  HTTPS   │  ┌────────────────────┐  │
│  │ Tauri shell           │◄─┼──────────┼─►│ Autumn server      │  │
│  │  webview → remote URL │  │          │  │ (routes, sessions, │  │
│  │  + native plugins:    │  │          │  │  Postgres, …)      │  │
│  │  notification         │  │          │  └────────────────────┘  │
│  │  biometric            │  │          └──────────────────────────┘
│  │  store                │  │
│  └───────────────────────┘  │
└─────────────────────────────┘
```

Choose the thin client when your app is online-first and you already run it as
a normal web service: one deployment serves browsers, PWAs, and the mobile app,
and shipping a server fix updates every installed app instantly. Choose the
[desktop sidecar model](tauri.md) instead when the app must be fully
self-contained on the user's machine.

**Trust model, up front:** the generated capability file grants pages served
from your remote origin the right to invoke native device APIs on the phone.
That origin is *fully trusted* — a compromised server can call every permitted
plugin command. Keep the grant scoped to exactly one origin you control, keep
the permission list minimal, and never widen `urls` to a wildcard like
`https://*`.

## Scaffolding

Run the generator from your project root:

```bash
autumn generate tauri --remote-url https://app.example.com
```

The URL must be `https://`; plain `http://` is accepted only for
`localhost` / `127.0.0.1` / `::1` dev servers, and URLs with embedded userinfo
(`https://user:pass@…`) are rejected outright. `--dry-run` prints the file plan
without writing, `--force` overwrites collisions, and
`autumn destroy tauri --remote-url <URL>` reverts the scaffold.

```
src-tauri/
  tauri.conf.json              — productName, identifier, version, bundle icons
  Cargo.toml                   — "{app}-mobile" crate; staticlib/cdylib for android/ios init
  build.rs                     — calls tauri_build::build()
  Info.ios.plist               — NSFaceIDUsageDescription for Face ID
  capabilities/
    remote-app.json            — grants the remote origin the plugin permissions
  src/
    main.rs                    — calls {app}_mobile::run()
    lib.rs                     — plugin registration + webview → remote URL
  icons/                       — same placeholder set as the desktop scaffold
  .gitignore                   — /target /binaries /configs /gen
```

Compared with the desktop scaffold, note what is *absent*: no
`stage-sidecar.sh`/`.ps1`, no per-OS `tauri.*.conf.json` overlays, no
`externalBin`, and no bundled `autumn.toml` resources. All of those exist
solely to build and supervise a local sidecar binary — a thin client has
nothing to stage, and its configuration lives on the server.

After scaffolding, initialise the mobile projects (output goes to
`src-tauri/gen/`, which is `.gitignore`d):

```bash
cargo install tauri-cli --version "^2"
cd src-tauri
cargo tauri android init && cargo tauri android dev
cargo tauri ios init && cargo tauri ios dev
```

## Routing the webview to a remote HTTPS domain

The generated `src/lib.rs` opens the main window directly on your server using
`WebviewUrl::External`:

```rust
tauri::WebviewWindowBuilder::new(
    app,
    "main",
    tauri::WebviewUrl::External(
        "https://app.example.com"
            .parse()
            .expect("remote URL was validated at generate time"),
    ),
)
.title("My App")
.build()?;
```

This mirrors the mechanism the desktop scaffold already uses (there it points
at a loopback port), so the codebase has one webview-creation idiom.

**Alternative: `frontendDist` as a remote URL.** Tauri also accepts a URL in
`tauri.conf.json`'s `build.frontendDist` field, in which case you don't create
the window in Rust at all:

```json
{
  "build": { "frontendDist": "https://app.example.com" }
}
```

The generator deliberately does not use this form: `tauri dev` has a known bug
with URL-form `frontendDist`
([tauri-apps/tauri#12333](https://github.com/tauri-apps/tauri/issues/12333)),
and the Rust-side builder keeps the URL next to the plugin registration it
belongs with. The config form remains valid if you prefer it for a
hand-maintained project.

**Server-side prerequisites** for the remote origin:

- A valid, publicly trusted TLS certificate — mobile webviews do not show
  certificate-error interstitials; a bad cert is just a blank screen.
- `session.secure = true` (or `AUTUMN_SESSION__SECURE=true`) in production so
  session cookies carry the `Secure` attribute (see
  [Sessions & auth](#sessions--auth-handoff) below).
- If you restrict trusted hosts (`AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS`),
  include the domain the app loads.

## Native device capabilities (App Store Guideline 4.2)

Apple's [Guideline 4.2 — Minimum Functionality](https://developer.apple.com/app-store/review/guidelines/#minimum-functionality)
rejects apps that are "not particularly useful, unique, or app-like" — in
practice, bare webview wrappers around a website. The scaffold pre-registers
three official Tauri plugins so your remote pages can use genuinely native
device features. Be honest with yourself here: these integrations *reduce*
rejection risk by making the app app-like, but nothing guarantees approval —
review is holistic, and the strongest defence is UX that feels like an app
(native-feature use, offline handling, no browser chrome metaphors).

### The capability file, field by field

`src-tauri/capabilities/remote-app.json` is what lets JavaScript served by your
*remote* server call into the native plugins (by default, Tauri only trusts
bundled local pages):

```json
{
  "identifier": "remote-autumn-app",
  "description": "Allow pages served by the remote Autumn server to use the native device plugins (notifications, biometric authentication, key-value storage).",
  "windows": ["main"],
  "remote": {
    "urls": ["https://app.example.com"]
  },
  "permissions": [
    "core:default",
    "notification:default",
    "biometric:default",
    "store:default"
  ]
}
```

- `identifier` — a unique name for this capability; tauri-build auto-discovers
  every JSON file under `src-tauri/capabilities/`.
- `windows` — which webview windows the grant applies to (only `main` exists
  in this scaffold).
- `remote.urls` — the origins whose pages may invoke the permitted commands.
  The generator emits exactly the origin you passed — never widen this to a
  wildcard: every origin listed here can drive the device APIs.
- `permissions` — the default permission set of each plugin plus Tauri's core
  APIs (`core:default`, `notification:default`, `biometric:default`,
  `store:default`).

> **Caveat:** on Linux and Android, Tauri cannot distinguish an embedded
> iframe from the window itself — a page your app embeds from another origin
> could be treated as the remote origin. Avoid third-party iframes on pages
> served to the mobile shell.

Your server pages call the plugins through the `@tauri-apps/api` npm packages
— or, simplest for a server-rendered Maud/htmx app, via the global that Tauri
injects when `app.withGlobalTauri` is enabled, or a small bundled script. The
samples below use the plugin packages; they run *on your remote Autumn pages*,
which is exactly what the `remote.urls` grant enables.

Detect the shell first, so the same pages degrade gracefully in an ordinary
browser:

```js
import { isTauri } from '@tauri-apps/api/core';

if (isTauri()) {
  // running inside the mobile shell — native plugins are available
} else {
  // plain browser — fall back to web APIs or hide native-only UI
}
```

### Notifications

```js
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from '@tauri-apps/plugin-notification';

let granted = await isPermissionGranted();
if (!granted) {
  granted = (await requestPermission()) === 'granted';
}
if (granted) {
  sendNotification({ title: 'Order shipped', body: 'Your order #1042 is on its way.' });
}
```

### Biometric authentication

The biometric plugin exists only on Android and iOS — its Cargo dependency is
target-gated and its registration in `lib.rs` is `#[cfg(mobile)]`-gated, so the
shell still builds on desktop for local smoke-testing. On iOS the generated
`Info.ios.plist` provides the required `NSFaceIDUsageDescription`; without it,
the first Face ID prompt kills the app.

```js
import { authenticate } from '@tauri-apps/plugin-biometric';

try {
  await authenticate('Unlock your account', {
    allowDeviceCredential: false,
  });
  // success — proceed (e.g. release a stored token, see below)
} catch (e) {
  // user cancelled or biometry unavailable — fall back to password login
}
```

### Device storage

```js
import { load } from '@tauri-apps/plugin-store';

const store = await load('app-data.json');
await store.set('draft', { body: 'unsent message…' });
const draft = await store.get('draft');
await store.save();
```

The store persists as plaintext JSON on the device. For secrets that warrant
encryption at rest, upgrade to
[`tauri-plugin-stronghold`](https://v2.tauri.app/plugin/stronghold/) — the
generator wires the lighter store plugin by default because Stronghold brings
heavy build dependencies.

## Sessions & auth handoff

Two patterns work between the webview and a remote Autumn server. Cookie-based
sessions are the default and require no app changes; the token pattern adds
native-storage control and composes with biometrics.

### Cookie-based sessions

Autumn's server-side sessions work in WKWebView (iOS) and Android WebView the
same way they do in a browser — the webview stores and replays the session
cookie. Requirements and caveats specific to a *remote HTTPS origin inside a
mobile shell*:

- **`Secure` is mandatory.** Set `session.secure = true`
  (`AUTUMN_SESSION__SECURE=true`) in production. Cookies without `Secure` over
  HTTPS are increasingly dropped by mobile webviews.
- **Set `SameSite` explicitly.** Since your pages, form posts, and htmx
  requests all originate from your own origin, `SameSite=Lax` (Autumn's
  sensible default) works for normal navigation. But iOS 18+ WKWebView treats
  cookies with *no* `SameSite` attribute inconsistently (historically `None`,
  now effectively `Lax`), so never rely on the unset default. If a request to
  your server originates from a *different* context (a plugin webview, an
  OAuth popup, an embedded page), the cookie must be `SameSite=None; Secure`
  to be attached cross-site.
- **Persistence is flaky on iOS.** WKWebView's cookie persistence interacts
  badly with Intelligent Tracking Prevention and has long-standing
  synchronization bugs (see WebKit bug 213510): a session cookie can survive a
  relaunch one day and vanish the next. Mitigate by using **server-side
  sessions keyed by a persistent, `HttpOnly`, `Secure` cookie with a long
  `Max-Age`** (not a browser-session cookie), and implement a silent
  re-authentication fallback — the token handoff below is a good one — so a
  dropped cookie degrades to a background token refresh instead of a login
  screen.
- Autumn knobs that matter here: `session.secure`, the signing secret
  (`AUTUMN_SECURITY__SIGNING_SECRET`), and trusted hosts
  (`AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS`).

### Authorization-header token handoff

Instead of (or as a fallback to) cookies, hand the webview a bearer token and
store it natively via the store plugin. On successful login your page stores
the token:

```js
import { load } from '@tauri-apps/plugin-store';

// after POST /login succeeds and returns { token }
const store = await load('auth.json');
await store.set('auth_token', token);
await store.save();
```

Subsequent requests from your remote pages attach it as an `Authorization`
header:

```js
const store = await load('auth.json');
const token = await store.get('auth_token');

const res = await fetch('https://app.example.com/api/orders', {
  headers: { Authorization: `Bearer ${token}` },
});
```

Issue short-lived access tokens and rotate them server-side (a refresh
endpoint the shell calls when a request comes back `401`); revoke on logout by
deleting the store entry *and* invalidating server-side.

**Biometric-gated token release** ties the two plugin integrations together —
require Face ID / fingerprint before the stored token is read:

```js
import { authenticate } from '@tauri-apps/plugin-biometric';
import { load } from '@tauri-apps/plugin-store';

async function unlockToken() {
  await authenticate('Unlock your account'); // throws if cancelled/unavailable
  const store = await load('auth.json');
  return store.get('auth_token');
}
```

Remember the store file is plaintext on disk — for high-sensitivity apps keep
tokens short-lived or move them to `tauri-plugin-stronghold`.

## Building & shipping

```bash
cd src-tauri
cargo tauri android build      # .aab / .apk under gen/android
cargo tauri ios build          # .ipa via Xcode under gen/apple
```

Signing, provisioning profiles, and store submission are covered by the
official Tauri distribution docs for
[Google Play](https://v2.tauri.app/distribute/google-play/) and the
[App Store](https://v2.tauri.app/distribute/app-store/). Replace the
placeholder icons before shipping (`cargo tauri icon static/icons/icon.svg`
from the app root).

**Offline UX:** a thin client is online-first, but "airplane mode shows a
white screen" is both a bad experience and App Store rejection bait. At
minimum, detect load failures (`window.addEventListener('offline', …)` on
page, or a page-load error handler in the shell) and show a friendly retry
view; a small local fallback page bundled with the shell is a worthwhile
manual addition.

## Relationship to other options

This page covers **Option A** of Autumn's Tauri mobile roadmap (issue #1506):
the pure thin client. Option B (in-process backend with a remote database,
issue #1507) and Option C (in-process backend with local SQLite and sync,
issue #1508) are separate roadmap items. For desktop packaging, see the
[Tauri desktop guide](tauri.md).
