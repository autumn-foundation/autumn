# ADR 0025: Constela Charges Every Retained Byte at One Seam

- Status: Accepted
- Date: 2026-10-25
- Deciders: Autumn maintainers
- Tags: constela, security, dos, memory
- Issue: #2626

## Context

`autumn_web::constela` renders documents that a language model writes. We
must treat each document as hostile. Before this ADR, the module had several
separate bounds: parse `Limits`, `RenderLimits`, and an allocation budget for
values that `concat`, `array`, `obj` and string `+` build.

Each bound was added for one path. Review of #2623 found four paths that kept
document-controlled bytes and did not pass a bound:

1. A `state`, `var` or `param` read copied its value and charged nothing.
2. Each rendered portal kept a copy of its `target` string.
3. Validation kept one diagnostic per fault, each with a copy of the name.
4. A component with a literal `id`, used two times, rendered the same id two
   times.

Items 1 to 3 are memory exhaustion. Item 4 is an accessibility bug.

## Decision

### One seam for retained bytes

The private module `constela::budget` holds the only budget type. A render
makes one `Budget`. A dispatch makes one `Budget`. Each copy and each
retained value charges the budget **before** it allocates:

- an evaluator copy of a value it reads,
- a value that the evaluator builds,
- route metadata, handler payloads, portal targets, and rendered ids.

Output bytes use one meter for the full render. The meter counts the body and
all portals. Each write to an output buffer goes through it.

### Read without copy

The evaluator returns `Cow<Value>`. A read of `lit`, `state`, `var` or
`param` borrows. It copies only when the caller must own the value, and that
copy is charged. A text node that shows a state field does not copy it.

The lexical environment holds values behind `Rc`. A loop iteration or a
component call does not copy the outer bindings.

### Bounded diagnostics

`Diagnostic::new` cuts `path` and `message` to
`MAX_DIAGNOSTIC_FIELD_BYTES`. Validation keeps at most `MAX_DIAGNOSTICS`
diagnostics, then adds one `constela.limit` diagnostic that tells the
generator to fix these first.

### Portal targets

Upstream lets a target be `body`, `head`, or any CSS selector. Thus we do not
use an allowlist. Validation rejects a target longer than
`MAX_PORTAL_TARGET_BYTES` (256). Each rendered portal also charges its target
and a fixed overhead to the budget.

### Duplicate ids are a render error

The renderer records each rendered `id`. A second element with the same id
stops the render with `constela.duplicate`.

We compared these options:

| Option | Result |
| --- | --- |
| Scope ids per component call or loop pass | Rejected. Ids change when the document changes. A host that targets `#c-email` with htmx or CSS breaks. A `for` that points out of the scope breaks. |
| Reject at validation | Rejected. Validation cannot see computed ids or loop counts. |
| Reject at render | **Accepted.** HTML requires unique ids. The error tells the generator to pass the id as a param or derive it from the loop index. The id to host mapping stays `prefix + id`. |

## Consequences

- A new path that copies or keeps a value must call the budget. The memory
  property test (`autumn/tests/constela_memory.rs`) and the `constela` fuzz
  target fail when a path does not.
- Reads now count against the budget. An action that copies a state value
  larger than 4 MiB fails. Before, the copy was free.
- A document that rendered duplicate ids now fails. The migration guide
  (`docs/migrations/next.md`) gives the fix.
- Paths are still built as strings for each node. A very long key makes each
  path long. This costs CPU, not retained memory. A follow-up issue tracks it.
