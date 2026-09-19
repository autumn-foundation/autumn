# Plan — Prove classified data can't leak to a sink at compile time (#1654)

Status: implementation plan for the first slice.
Guide produced by this work: `docs/guide/data-classification.md`.

## 1. Problem restated

Autumn classifies sensitive data at runtime, by *name*: `log/filter.rs` matches a
key denylist, `http_client.rs` matches three header names, `gdpr.rs` keys erasure
off stringly-typed table names. Rename a column and the protection silently
evaporates. Nothing in the build says "this value may not be serialized here".

The first slice must make one classification tier (`personal data`) a property of
the **type**, gate one sink (the `Json` response), and emit a diffable manifest
of what can reach that sink.

## 2. Brainstorming — candidate mechanisms

1. **Name denylist v2.** Extend `DEFAULT_FILTER_KEYS` to the response serializer.
   Zero type safety; the exact failure mode the issue names. Rejected.
2. **Runtime taint tracking.** Wrap values, check a flag before serializing.
   Catches leaks in production, not in CI. Rejected (the issue asks for a build
   failure).
3. **Custom lint / dylint driver.** Real dataflow analysis, but needs a nightly
   driver and a second toolchain in CI. Rejected for slice 1.
4. **Taint the model type.** A `#[model]` with any classified field loses its
   `Serialize` impl. Cheap, but `let email = user.email;` hands you a plain
   `String` again — the Success Metric's exact leak still compiles. Insufficient
   alone.
5. **Taint the field type.** The `#[classified]` field becomes
   `Classified<String, Marker>`, a wrapper with **no** `Serialize` impl. Moving
   the value anywhere serializable is a type error; the model loses `Serialize`
   as a *consequence*, not as a special case. This is the issue's stated Vision
   ("becomes tainted at the type level"). **Chosen.**
6. **Sink gating by trait bound.** `Json<T>: IntoResponse` requires
   `T: classify::JsonSink`, blanket-implemented for `T: Serialize` and marked
   `#[diagnostic::do_not_recommend]` so the diagnostic is autumn's, not serde's.
   **Chosen** — it is also the seam the follow-up sinks (log, outbound HTTP,
   analytics) plug into.
7. **Declassification by move.** `Classified<T, F>::declassify(self, &Declassification<F>) -> T`
   consumes the taint exactly once and emits the audit record. **Chosen.**
8. **Manifest from `inventory`.** `#[model]` submits one descriptor per
   classified field; the `declassify!` macro submits one per declared boundary.
   The manifest joins them. Matches the house pattern from #1716
   (`autumn cache audit`) and #1604 (`autumn routes audit`). **Chosen.**
9. **Manifest by proc-macro file writes.** A macro writing JSON into `target/`
   during expansion. Literally "cargo build emits", but stale on incremental
   rebuilds, racy under parallel codegen units, and broken on read-only build
   sandboxes. Rejected in favour of (8).

## 3. Reverse brainstorming — how would we make this *fail*?

| Way to break it | Countermeasure in this slice |
|---|---|
| Give `Classified<T, F>` a `Serialize` impl "for convenience" | It has none, and a compile-fail fixture pins that. |
| Add `impl From<Classified<T, F>> for T` so Diesel can write the column | Diesel goes through an opaque `ClassifiedText` column wrapper instead. `ClassifiedText` has no accessor, no `Serialize`, no `Display`. |
| `Display`/`ToString` on the wrapper leaks into templates | Not implemented. `Debug` is redacted. |
| Let anyone construct a `Declassification` inline, bypassing the manifest | The constructor is `#[doc(hidden)]`; `declassify!` is the public path and it is what submits the descriptor. Threat model is drift detection, not an adversarial author (same posture as `docs/guide/security-posture-manifest.md`). |
| Reuse `User::email`'s boundary to release `Order::card_number` | `Declassification<F>` is generic over the field marker; the wrong boundary is a type error. |
| Model keeps `Serialize` because the derive is unconditional | The derive is dropped when any field is classified; a compile-fail fixture pins `Json(user)`. |
| The write structs (`NewUser`, `UpdateUser`) still serialize the plaintext | Classified columns get `#[serde(skip_serializing)]` there. |
| Diagnostic is serde's "`Classified<..>: Serialize` is not satisfied" and unreadable | `#[diagnostic::on_unimplemented]` on `JsonSink` + `#[diagnostic::do_not_recommend]` on its blanket impl; `.stderr` snapshots pin the wording. |
| The manifest silently drops a field when a crate is not linked | The manifest describes the binary that produced it, and says so, exactly like the cache-coherence manifest. |
| Existing name-based redaction regresses | `log/filter.rs` and `http_client.rs` are untouched; a regression test asserts both still behave. |

## 4. Six hats

**White (facts).** `#[model]` already carries per-field attributes and already
rewrites field types for `#[encrypted]` via `#[diesel(serialize_as/deserialize_as)]`.
`Json` is a single wrapper in `autumn/src/extract.rs` with one `IntoResponse`
impl. `inventory` is already a dependency and already backs two manifests.
`trybuild` compile-fail/compile-pass fixtures already exist with `.stderr`
snapshots. Rust 1.88 has `diagnostic::on_unimplemented` (1.78) and
`diagnostic::do_not_recommend` (1.85).

**Red (instinct).** The scary part is blast radius inside a 13k-line model macro.
Mitigation: the field-type rewrite fires only for fields that opt in, and no
existing model opts in, so the workspace cannot regress.

**Black (risks).**
- A non-`Serialize` model breaks `#[repository]` version history / ledger
  codegen, which calls `serde_json::to_value(self)`. Documented limitation;
  version history is itself a follow-up sink.
- `#[validate]` attributes ride on the read struct since #1778. Delegating
  `validator` impls for the wrapper keep them working.
- `OpenApiSchema` would advertise a property that can never be serialized.
  Classified columns are omitted from the read struct's schema.

**Yellow (upside).** One tier and one sink, but the seam generalises: a new sink
is a new marker trait plus a `Sink` variant, and a new tier is a new enum
variant. The manifest is diffable, so a reviewer sees a new release edge in the
PR diff.

**Green (creative).** The field marker type does double duty: it names the field
in the compiler diagnostic *and* keys the boundary so a manifest edge is a
build-time fact rather than a runtime observation.

**Blue (process).** Red → green → refactor, then a multi-angle agent review, then
the AC evidence table.

## 5. Design

### 5.1 `autumn/src/classify/` (new, unconditional module)

- `Classification::PersonalData` — the single tier.
- `Classified<T, F: ClassifiedField>` — the taint. `Clone`, `PartialEq`, `Eq`,
  `Hash`, redacted `Debug`, `Deserialize`, `From<T>`. **No `Serialize`, no
  `Display`, no `Deref`, no `into_inner`.**
- `ClassifiedField` — the macro-generated per-field marker trait carrying
  `MODEL`, `FIELD`, `CLASSIFICATION`.
- `ClassifiedText` — the opaque Diesel `serialize_as` / `deserialize_as` column
  wrapper (mirrors `encryption::RandomizedText`).
- `Sink::JsonResponse` — the one supported sink.
- `Declassification<F>` — a declared boundary (purpose, sink, reason).
- `declassify!` — declares a boundary **and** submits its manifest descriptor.
- `JsonSink` — the gate on `Json`'s `IntoResponse`.
- `manifest` — descriptors, `DataFlowManifest`, dump marker, JSON, summary.
- Release records: a `tracing` event on `autumn::declassification` plus an
  optional process-wide observer for apps that persist them.

### 5.2 `#[model]` changes

`#[classified]` (alias `#[classified(personal_data)]`) on a non-null `String`
field:

1. rewrites the read struct's field to `Classified<String, Marker>` with the
   Diesel column wrapper,
2. generates `Marker` + its `ClassifiedField` impl + the inventory descriptor,
3. drops `#[derive(Serialize)]` from the read struct,
4. stamps `#[serde(skip_serializing)]` on the column in `New*`/`Update*`/`Changeset`,
5. redacts the column in the generated `Debug`,
6. omits the column from the read struct's OpenAPI schema,
7. rejects `#[encrypted]`, `#[searchable]`, `#[normalize]`, `#[translatable]`,
   `#[id]`, `#[lock_version]`, `#[position]`, `#[state_machine]` and non-`String`
   types with a diagnostic that says why.

### 5.3 `Json` sink

```rust
impl<T> IntoResponse for Json<T> where T: crate::classify::JsonSink { ... }
```

### 5.4 Manifest

`autumn data-flow` builds and runs the app binary under
`AUTUMN_DUMP_DATA_FLOW=1`, writes `target/autumn/data-flow-manifest.json`, and
`--check <path>` fails on drift against a committed copy.

## 6. Test plan (red first)

1. `classify` unit tests — wrapper behaviour, boundary, records, manifest join,
   JSON round-trip, dump parsing.
2. `tests/integration/data_classification.rs` — end-to-end over a real
   `#[model]`: marker consts, inventory descriptors, declassify + `Json`, and a
   non-classified model still serializing.
3. trybuild compile-fail — model leak, field leak, wrong boundary, non-`String`,
   `#[encrypted]` combo; `.stderr` snapshots pin that the diagnostic names the
   field and the sink.
4. trybuild compile-pass — the declassified path.
5. AC5 regression — `log/filter.rs` and `http_client.rs` behaviour unchanged.
6. `autumn-cli` unit tests — report formatting and `--check` drift.
