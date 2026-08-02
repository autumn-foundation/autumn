# Scaffolded attachment uploads — issue #1236 plan

## Boundary and specification

The change is generator-only: newly generated HTML scaffolds with one or more
`Attachment` fields must accept a browser multipart submission, stream file
parts through `MultipartField::save_to_blob_store`, and bind the returned
`Blob`. Non-attachment scaffolds and the public storage/extractor APIs remain
unchanged.

Observable invariants:

1. Create binds each uploaded attachment, while an omitted optional file binds
   `NULL`.
2. Update binds a replacement or preserves the current blob when the file part
   is empty/absent.
3. Every saved blob is either committed with the row or best-effort deleted on
   a later parse, validation, authorization, or database failure.
4. Upload size failures retain the extractor's `413 Payload Too Large` mapping.
5. Generated forms need only multipart encoding and a named file input; no
   hidden storage key, presign endpoint, or JavaScript is part of the default.
6. Reloaded views visibly identify the bound blob through non-sensitive metadata.

This is generated Rust and does not introduce a new critical runtime algorithm
or state representation, so a separate Verus model would duplicate template
string tests without proving the generated integration boundary. Executable
code-generation, generated-project compilation, and real local-blob-store tests
are the appropriate proof boundary.

## Brainstorming

Candidate approaches considered:

- Generate a presign endpoint and JavaScript uploader.
- Buffer uploaded bytes in the route before writing storage.
- Stream `MultipartField` directly into the configured `BlobStore` and rebuild
  only the text fields for the existing form decoder.
- Add a new public framework abstraction around attachment forms.

The streaming approach reuses the shipped primitive, enforces configured size
limits, works for local and S3 stores, and keeps the public API unchanged. The
existing form decoder remains the single source of text-field semantics.

## Reverse brainstorming

Ways to make the scaffold fail or lose data deliberately:

- Keep emitting a file input but decode the body as URL-encoded bytes.
- Expect a hidden blob key that no generated JavaScript populates.
- Replace an existing attachment with `NULL` on an empty edit submission.
- Save the first of several files, then leak it when a later part is invalid.
- Swallow the extractor's size error and return a generic `500`.
- Generate code requiring undeclared `storage` or `multipart` features.
- Persist a blob but render only an indistinguishable generic marker after
  reload.

Each failure mode maps to a generator assertion, cleanup-path test, real-store
write-path test, or fresh-project compilation check.

## Six hats review

- **White (facts):** `save_to_blob_store` already streams and enforces the
  configured cap; attachment columns are nullable `Option<Blob>`; browsers do
  not repopulate file inputs.
- **Red (user experience):** the plain generated form must work immediately and
  an edit without a new upload must feel safe rather than destructive.
- **Black (risks):** multipart parsing can fail after an earlier blob was saved;
  authorization/validation/DB failures can orphan objects; extractor ordering
  can make generated axum handlers fail to compile.
- **Yellow (benefits):** zero handwritten upload code, backend-independent
  storage, progressive enhancement, and a clear opt-in path for large direct
  uploads.
- **Green (alternatives):** rebuild URL-encoded text pairs to reuse validation,
  generate unique per-field keys, and centralize cleanup around the whole
  fallible parse span.
- **Blue (process):** RED generator tests first; GREEN minimal template changes;
  REFACTOR shared rendering/cleanup helpers; then format, lint, focused tests,
  fresh-scaffold compilation, acceptance-criteria audit, and multi-angle review.

## TDD execution

### Red

Extend the attachment scaffold integration test to require index/show output to
identify the persisted blob. Confirm it fails against the generic
`"attachment"` marker.

### Green

Render the stored content type and byte size for present attachment cells and
an em dash for `NULL`, using maud's escaped value rendering.

### Refactor and verification

Keep the behavior in the shared `cell_value_expr` helper so index and show views
cannot drift. Run formatting, focused generator tests, the ignored fresh-project
compile test, clippy, and an affected-area stub scan. Review the final diff from
correctness, security/data-integrity, testing, and generated-code/DX angles.

## Review outcomes

Independent correctness, security/data-integrity, and testing/DX reviews found
that authorized updates previously stored multipart bytes before checking the
record policy. The refactor now loads and authorizes the record before parsing,
so a denied actor cannot consume blob storage and no authorization error can
orphan a staged upload. Duplicate file parts for one field are rejected inside
the centralized cleanup span, preventing overwrite-orphans. The generated note
was also corrected to match the current bounded-prefix CSRF scanner, and views
render non-sensitive blob metadata rather than internal storage keys.

Successful replacement does not automatically delete the prior blob: `Blob`
values can be intentionally shared and the framework has no ownership/refcount
contract that makes deletion safe. Cleanup remains limited to newly staged,
uncommitted objects; lifecycle reclamation requires an explicit ownership
policy and is outside this generator-only issue.
