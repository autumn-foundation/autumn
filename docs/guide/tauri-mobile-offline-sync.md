# Offline Sync for Tauri Mobile: Local SQLite + Background Sync (`autumn generate tauri-mobile --offline-sync`)

`autumn generate tauri-mobile --offline-sync` layers **local-first storage**
onto the in-process mobile scaffold from
[docs/guide/tauri-mobile-in-process.md](tauri-mobile-in-process.md): app data
lives in a SQLite file inside the app sandbox (`autumn_web::sync::SyncStore`),
and a background `SyncEngine` pushes and pulls changes to your **remote
Autumn deployment's `/sync` endpoints** whenever the network allows. The app
functions fully offline — reads, writes, deletes — and converges with the
remote PostgreSQL database in the background when connection is restored
(issue #1508, "Option C").

This is a different network model than the plain `tauri-mobile` scaffold:

| | `tauri-mobile` (default) | `tauri-mobile --offline-sync` |
| --- | --- | --- |
| Device data | remote Postgres, per query | local SQLite (`SyncStore`) |
| Network needed | for every DB-backed request | only to sync, in the background |
| Device DB credentials | Postgres URL in the shell | **none** — HTTPS to `/sync` |
| Remote deployment | any Postgres | the same app, serving `/sync` |

This page covers:

1. the architecture (what runs where),
2. the data model and change tracking (write-through journal),
3. sync semantics (server-authoritative versions, push/pull, idempotency),
4. conflict resolution (default last-write-wins, custom `ConflictResolver`),
5. tombstones, garbage collection, and full resync,
6. what the generator emits (drift-checked against the real templates),
7. the offline showcase walkthrough (airplane-mode checklist),
8. failure modes and limitations.

The sync engine itself lives in the `autumn-web` crate behind the
**`offline-sync`** cargo feature (`autumn_web::sync`); everything below also
applies to non-Tauri occasionally-connected clients.

## 1. Architecture

```text
┌─────────────────── mobile app process ───────────────────┐
│  webview ⇄ http://127.0.0.1:<port>                        │
│     │                                                     │
│  in-process Axum server (your routes)                     │
│     │  reads/writes                                       │
│  SyncStore  ──  sync.db (SQLite, app sandbox, WAL)        │
│     ▲                                                     │
│     │ push pending / pull since cursor                    │
│  SyncEngine (background task, 30 s + backoff)             │
└─────┼─────────────────────────────────────────────────────┘
      │  HTTPS  AUTUMN_SYNC__REMOTE_URL = https://…/sync
      ▼
┌──────────── remote Autumn deployment (same app) ──────────┐
│  AppBuilder::nest("/sync", sync::server::router(…))       │
│     POST /sync/push      GET /sync/pull?cursor=N          │
│     │                                                     │
│  PgSyncBackend → PostgreSQL shadow tables                 │
│     autumn_sync_rows / autumn_sync_applied /              │
│     autumn_sync_meta  (+ sequence autumn_sync_version_seq)│
└───────────────────────────────────────────────────────────┘
```

The **same generated app codebase** plays both roles. Deployed on a server
with `AUTUMN_DATABASE__URL` set, `serve()` mounts the `/sync` router backed
by Postgres shadow tables. Running in-process on a device with no database
URL, the mounting is skipped and the app is a sync **client**: routes talk
to the local `SyncStore`, and the shell's background engine reconciles it
with the remote.

**Ordering is server-authoritative.** Every accepted change is assigned a
monotonically increasing version — a change sequence number (CSN) — from one
global Postgres sequence. Clients pull "rows with `version > my cursor`";
device wall clocks never order the change feed (they are consulted only
inside the conflict resolver, between the two conflicting writes — see §4).

## 2. Data model and change tracking

`SyncStore` is a document-flavored store: rows are JSON payloads keyed by
`(collection, pk)`. Any `serde::Serialize`/`DeserializeOwned` type works:

```rust
use autumn_web::sync::SyncStore;

let store = SyncStore::open(std::env::var("AUTUMN_SYNC__DB_PATH")?)?;

store.put("notes", "6b3f2c1e-…", &note)?;           // insert or update
let note: Option<Note> = store.get("notes", "6b3f2c1e-…")?;
let all: Vec<(String, Note)> = store.list("notes")?; // pk-ordered, no tombstones
store.delete("notes", "6b3f2c1e-…")?;                // tombstone + journal
let pending = store.pending_count()?;                // journaled, unsynced changes
```

Two rules make the model sync-safe:

- **Client-generated primary keys** — always UUIDs (or similarly unique
  strings), never serial integers. Two offline devices must be able to
  create rows concurrently without colliding.
- **Additive schema evolution** — payloads are JSON; give new fields
  `#[serde(default)]`-compatible semantics so old rows (and rows written by
  not-yet-updated devices) still deserialize.

**Change tracking is write-through, not trigger-based.** Every `put`/`delete`
writes the row *and* appends an entry to a pending-change **journal in the
same SQLite transaction** — a crash can never lose a journal entry or record
a change that didn't happen. Journal entries per `(collection, pk)` are
coalesced (the latest state wins) but keep the **original** `base_version`,
so a conflict with a remote write is still detected even after ten local
edits. The store also persists a stable per-install `device_id` (UUID v4)
and the pull `cursor`.

The SQLite file uses WAL mode with a busy timeout, and all writes go through
one serialized connection — concurrent in-process writers are safe.

## 3. Sync semantics: push, pull, idempotency

One `SyncEngine::sync_once()` pass does:

1. **Push** — send journaled changes in batches
   (`POST /sync/push`, body `{device_id, changes: [...]}`). The server
   applies each batch **atomically** (one Postgres transaction) and answers
   per change: `applied {version}`, `already_applied`, or `resolved {row}`
   (a conflict was settled — see §4). Confirmed entries are cleared from the
   journal; a resolved row is applied locally so the device converges
   immediately.
2. **Pull** — page through `GET /sync/pull?cursor=N&limit=M` and apply every
   row newer than the local cursor, then advance the cursor. Rows with a
   *pending local change* are skipped — local edits win locally until the
   push settles them (the server remains the authority on the final state).

Delivery is **at-least-once**: every journal entry carries a
client-generated `change_id`, and the server dedups per
`(device_id, change_id)` in `autumn_sync_applied`. A retry after a lost
response returns `already_applied` and never double-applies.

The background loop (`spawn_background(interval)`) runs `sync_once` every
30 s (as generated), backing off exponentially — 1 s doubling to a 5 min
ceiling — while the server is unreachable. A transport error leaves local
state and the journal untouched; the app keeps working offline and the next
successful pass converges. The generated shell additionally triggers an
immediate pass when the app returns to the foreground (§6).

## 4. Conflict resolution

A conflict is detected at push time by **base-version mismatch**: the change
says "I was based on version 7" but the server row is at version 9. The
resolver runs **server-side** — one authority, no distributed convergence
protocol:

```rust
pub trait ConflictResolver: Send + Sync {
    fn resolve(&self, client_device_id: &str, client: &Change, server: &RemoteRow) -> Resolution;
}

pub enum Resolution {
    KeepServer,                  // client's write loses
    TakeClient,                  // client's write wins
    Merge(serde_json::Value),    // synthesize a merged payload
}
```

The default `LwwResolver` is **last-write-wins** on the two writes'
`updated_at`, with the device id as a deterministic tiebreak. The clock
caveat is confined and explicit: wall clocks compare only the *two
conflicting writes* (a device with a wrong clock can win one conflict, not
reorder the world), and you can replace the policy entirely. A field-merge
example:

```rust
use autumn_web::sync::{Change, ConflictResolver, RemoteRow, Resolution};

/// Field-level merge: keep the server row, overlay the client's fields.
struct FieldMergeResolver;

impl ConflictResolver for FieldMergeResolver {
    fn resolve(&self, _device: &str, client: &Change, server: &RemoteRow) -> Resolution {
        let (Some(client_payload), Some(server_payload)) = (&client.payload, &server.payload)
        else {
            return Resolution::KeepServer; // a delete is involved
        };
        let mut merged = server_payload.clone();
        if let (Some(merged_map), Some(client_map)) =
            (merged.as_object_mut(), client_payload.as_object())
        {
            for (key, value) in client_map {
                merged_map.insert(key.clone(), value.clone());
            }
        }
        Resolution::Merge(merged)
    }
}
```

Pass it where the generated `serve()` builds the router:
`server::router(backend, Arc::new(FieldMergeResolver))`.

**Convergence guarantee:** every resolution — even `KeepServer` — assigns
the row a **new** version, so all devices (including the conflict loser)
receive the settled state on their next pull.

## 5. Tombstones, GC, and full resync

Deletes never physically remove a synced row on the server: they write a
**tombstone** (`deleted = true`), which is just another versioned row in the
change feed — that is how a delete on one device propagates to every other
device instead of silently resurrecting on the next push.

Tombstones accumulate. `SyncBackend::gc_tombstones(up_to)` physically drops
tombstones with `version <= up_to` and records that version as the
**`tombstone_horizon`** in `autumn_sync_meta`. GC is an explicit server-side
operation (a job or admin task you schedule); it is **off by default**.

The horizon exists to keep long-offline clients correct: a client whose
cursor is *behind* the horizon might have missed a tombstone that no longer
exists, so the server answers its pull with **`FullResyncRequired`** instead
of a page of rows. The engine handles this transparently: it clears synced
local state (**pending local changes are preserved**), re-pulls everything
from cursor 0, then replays the preserved journal. Pick a GC cadence that
makes this rare — e.g. GC tombstones older than 30 days if your fleet syncs
at least monthly.

## 6. What the generator emits

Run it on an app that already has (or alongside) the mobile scaffold:

```bash
autumn generate tauri-mobile --offline-sync    # or --dry-run to preview
```

On top of the base scaffold (see
[tauri-mobile-in-process.md](tauri-mobile-in-process.md)) the flag makes
four template changes. Every snippet below is drift-checked against the real
generator output by `autumn-cli`'s test suite.

### Environment variables

| Variable | Set by | Meaning |
| --- | --- | --- |
| `AUTUMN_SYNC__DB_PATH` | the shell, in `setup()` | absolute path of the local SQLite sync database (app sandbox); your routes read it to open the same `SyncStore` |
| `AUTUMN_SYNC__REMOTE_URL` | **you**, in `src-tauri/src/lib.rs` | base URL of the remote `/sync` mount (e.g. `https://app.example.com/sync`, no trailing slash); if unset the app runs offline-only |
| `AUTUMN_DATABASE__URL` | your **server** deployment only | presence makes `serve()` mount `/sync`; leave it **unset on the device** |

These are template/deployment conventions — the engine itself takes plain
constructor arguments, so non-Tauri clients can wire it however they like.

### The app crate: feature + server-side `/sync` mounting

`Cargo.toml` gains an `offline-sync` feature
(`offline-sync = ["autumn-web/offline-sync"]`), included in `default` so a
plain `cargo run` server deployment serves `/sync`. The extracted
`src/lib.rs::serve()` mounts the router just before `.run()`:

<!-- drift:src/lib.rs -->
```rust
    #[cfg(feature = "offline-sync")]
    let app = mount_offline_sync(app).await;
```

backed by this generated helper — note the two load-bearing decisions: the
**database guard** (no `AUTUMN_DATABASE__URL` → sync client, `/sync` not
mounted) and **startup tolerance** (an unreachable database logs a warning
instead of aborting the boot):

<!-- drift:src/lib.rs -->
```rust
#[cfg(feature = "offline-sync")]
async fn mount_offline_sync(app: autumn_web::app::AppBuilder) -> autumn_web::app::AppBuilder {
    use std::sync::Arc;

    use autumn_web::reexports::tokio;
    use autumn_web::sync::{LwwResolver, PgSyncBackend, server};

    // Diagnostics below use stderr: this helper runs BEFORE AppBuilder::run()
    // installs the tracing subscriber, so tracing events here would be lost.
    let Ok(database_url) = std::env::var("AUTUMN_DATABASE__URL") else {
        eprintln!(
            "offline-sync: AUTUMN_DATABASE__URL is not set — running as a \
             sync client only; the remote deployment serves /sync"
        );
        return app;
    };
    let backend = Arc::new(PgSyncBackend::new(database_url));
    // Idempotent DDL for the sync shadow tables. A temporarily unreachable
    // database must not prevent the app from starting: log and continue —
    // /sync requests fail until the schema exists (restart once the database
    // is reachable, or run the DDL from a deploy step).
    let schema_backend = Arc::clone(&backend);
    match tokio::task::spawn_blocking(move || schema_backend.ensure_schema()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("offline-sync: could not ensure the sync schema (/sync will fail): {e}");
        }
        Err(e) => eprintln!("offline-sync: sync schema task failed: {e}"),
    }
    app.nest("/sync", server::router(backend, Arc::new(LwwResolver)))
}
```

`ensure_schema()` creates the shadow tables (`autumn_sync_rows`,
`autumn_sync_applied`, `autumn_sync_meta`) idempotently. They are
deliberately **not** part of autumn's framework migrations — apps without
offline sync see zero schema churn.

> **Authentication is on you.** The `/sync` endpoints trust `device_id` as
> sent and are generated without auth, exactly like every other route in a
> fresh scaffold. Before shipping, put them behind your app's session/token
> middleware (the router mounts through `AppBuilder::nest`, so app-level
> auth middleware applies) and scope data per user server-side.

### The shell: local store + background engine

`src-tauri/Cargo.toml` gains a direct `autumn-web` dependency with the
`offline-sync` feature (version-matched to the app's own requirement so
cargo unifies them). `setup()` places the sync database in the app sandbox
and exports its path for your routes:

<!-- drift:src-tauri/src/lib.rs -->
```rust
    let sync_db = data_root.join("sync.db");
    std::env::set_var("AUTUMN_SYNC__DB_PATH", sync_db.to_string_lossy().as_ref());
```

The server thread starts the engine before parking on `serve()`:

<!-- drift:src-tauri/src/lib.rs -->
```rust
fn start_background_sync(runtime: &tokio::runtime::Runtime, sync_db: std::path::PathBuf) {
    let store = match autumn_web::sync::SyncStore::open(&sync_db) {
        Ok(store) => store,
        Err(e) => {
            // ... log and return — the app still runs, without sync ...
            return;
        }
    };
    let Ok(remote_url) = std::env::var("AUTUMN_SYNC__REMOTE_URL") else {
        // ... log: offline-only mode (local SyncStore, no background sync) ...
        return;
    };
    let engine =
        autumn_web::sync::SyncEngine::new(store, autumn_web::sync::SyncConfig::new(remote_url));
    // spawn_background must be entered from inside the runtime; the returned
    // JoinHandle detaches on drop (dropping never cancels the task).
    let _sync_task =
        runtime.block_on(async { engine.spawn_background(std::time::Duration::from_secs(30)) });
    let _ = SYNC_KICK.set((runtime.handle().clone(), engine));
}
```

And the tauri run loop gains a **connectivity-regain trigger**: mobile OSes
freeze the process (and its timers) in the background, and connectivity
usually returns together with the foreground — so an app resume kicks one
immediate sync pass instead of waiting out the interval/backoff:

<!-- drift:src-tauri/src/lib.rs -->
```rust
            if let tauri::RunEvent::Resumed = event {
                if let Some((handle, engine)) = SYNC_KICK.get() {
                    let engine = engine.clone();
                    handle.spawn(async move {
                        if let Err(e) = engine.sync_once().await {
                            // ... log; the background loop retries anyway ...
                        }
                    });
                }
            }
```

### Offline startup, by construction

The offline requirement — *"the app functions fully offline"* — is met by
**not giving the device a database at all**. With `AUTUMN_DATABASE__URL`
unset, autumn's boot takes the "Database not configured" path: no pool, no
startup migrations, nothing to time out. Every piece of the sync wiring
degrades instead of aborting: a missing remote URL means offline-only mode,
an unreachable remote is a retried transport error, and the (server-side)
schema DDL failure logs and continues. Contrast this with the default
`tauri-mobile` model, where a dev-profile build with an unreachable
database exits during startup migrations — under `--offline-sync` that path
is never armed on the device. If you *do* set a database URL on the device
(hybrid: some direct-Postgres routes plus offline collections), you have
reintroduced that startup dependency knowingly.

## 7. Offline showcase: a notes flow, verified in airplane mode

The scaffold wires the plumbing; here is the complete pattern for an
offline-capable feature. Routes talk to the `SyncStore` — reads and writes
work with the radio off:

```rust
use autumn_web::prelude::*;
use autumn_web::sync::SyncStore;

#[derive(serde::Serialize, serde::Deserialize)]
struct Note {
    title: String,
    body: String,
}

/// Open the store at the path the mobile shell exported. Cheap: the
/// underlying connection is shared per path within the process.
fn notes_store() -> SyncStore {
    let path =
        std::env::var("AUTUMN_SYNC__DB_PATH").unwrap_or_else(|_| "tmp/sync.db".to_owned());
    SyncStore::open(path).expect("failed to open the offline sync store")
}

#[get("/notes")]
async fn notes_index() -> maud::Markup {
    let notes: Vec<(String, Note)> = notes_store().list("notes").unwrap_or_default();
    maud::html! {
        h1 { "Notes (" (notes.len()) ")" }
        ul {
            @for (pk, note) in &notes {
                li { b { (note.title) } " — " (note.body) " [" (pk) "]" }
            }
        }
    }
}

#[post("/notes")]
async fn notes_create(form: Form<Note>) -> Redirect {
    // Client-generated pk — NEVER a serial id: offline devices must be able
    // to create rows concurrently without colliding.
    let pk = format!("{:x}", md5_like_unique_id()); // e.g. uuid::Uuid::new_v4()
    notes_store().put("notes", &pk, &*form).expect("local write failed");
    Redirect::to("/notes")
}
```

(Register both in `routes![...]`; use the `uuid` crate — or any
collision-free scheme — for `pk`.)

**Airplane-mode checklist** (simulator or device):

1. Deploy the app to a server with `AUTUMN_DATABASE__URL` set; confirm
   `GET https://your-host/sync/pull?cursor=0` answers JSON.
2. Set `AUTUMN_SYNC__REMOTE_URL` in `src-tauri/src/lib.rs`, then
   `cd src-tauri && cargo tauri ios dev` (or `android dev`).
3. Create a few notes — they appear instantly (local SQLite writes).
4. **Enable airplane mode.** Create, edit, and delete notes: everything
   keeps working — reads and writes never touch the network. Relaunch the
   app in airplane mode: the data is still there (it is on disk, not in a
   cache). The console shows the engine backing off with transport errors.
5. **Disable airplane mode.** Within the sync interval (or immediately, on
   an app resume) the journal drains: verify on the server with
   `SELECT collection, pk, payload, deleted FROM autumn_sync_rows` — your
   offline creations are rows, your offline deletes are tombstones.
6. Run the app on a second device/simulator: it pulls the first device's
   notes; edits converge both ways, conflicts settle per §4.

In-repo, the same end-to-end behavior is pinned by
`autumn`'s `offline_writes_replay_to_server_on_sync` integration test
(offline writes → server starts → one sync → converged backend, drained
journal) — run
`cargo test -p autumn-web --features offline-sync --test integration_tests offline_sync`.

## 8. Failure modes and limitations

- **Only `SyncStore` data is offline.** Existing diesel `#[repository]`
  repositories and `Db`-extractor queries still need the remote database and
  will fail without it — the honest scope of this feature is "data you put
  in the store", not transparent offline for the whole ORM. Design the
  offline surface of your app around collections.
- **At-least-once, not exactly-once side effects.** Change *application* is
  deduplicated, but if you attach server-side hooks to sync data, make them
  idempotent.
- **Schema evolution is your contract.** Payloads are JSON: evolve models
  additively with serde defaults. There is no payload migration machinery.
- **Long-offline clients** past the GC horizon get a transparent full
  resync (§5) — correct, but bandwidth-shaped like a first sync.
- **Auth**: the `/sync` endpoints ship unauthenticated, like every scaffold
  route; they trust `device_id` as sent. Mount them behind your session or
  token middleware and enforce per-user scoping server-side before
  shipping. Never expose them without TLS.
- **Clock skew** only influences the default LWW resolver's choice between
  two conflicting writes; feed ordering is immune. If that is still too
  much trust, ship a custom resolver.
- **Storage**: sync.db lives in the app sandbox and is not size-managed by
  the framework; GC on the server plus `delete()`d tombstones locally keep
  it proportional to live data.
- `autumn destroy tauri-mobile --offline-sync` removes the shell; the app
  crate's `offline-sync` feature and the sync code in `src/lib.rs` are left
  in place (like the `serve()` extraction, they remain valid app code).
