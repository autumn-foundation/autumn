# Edge Node and TTFB Probe (issue #1790, second pass)

> **Status: executed.** Prior work: `2026-08-19-edge-capsule-first-slice.md`
> and PR #3130. This plan closes the gaps that stay after them.

## Gap audit

| AC / metric | State before this plan | Gap |
| --- | --- | --- |
| AC-1 one `autumn build` | Done. CI runs `autumn build --edge`. | None. |
| AC-2 byte-identical, in CI | Done. Tiers A to D. | None. |
| AC-3 fallthrough, no glue | `EdgeGateway` is a library with an in-process origin. No CDN speaks the NDJSON protocol. | To deploy, the author must write a shim. That is glue. |
| AC-4 mediated seam | Done. `EdgeKv` / `EdgeCache`. | None. |
| AC-5 actionable refusal | Done. Macro, `EdgeHandler`, doctor, build preflight. | None. |
| Metric: TTFB down >= 50% | Not measured. | No tool, no proof. |
| Metric: 10k requests, 0 divergence | Done. Tier D. | None. |

The issue puts "a single reference target" in scope. Today that target is a
library. The author cannot run it without code.

## Brainstorming

1. `autumn edge serve`: an HTTP edge node. It runs the capsule and sends each
   fallthrough to a remote origin over HTTP.
2. `autumn edge ttfb`: a probe. It measures TTFB at the edge and at the origin,
   and compares the bytes of each pair.
3. A vendor shim (Workers, Lambda).
4. A WASI Preview 2 component host (`wasmtime`).
5. A warm-instance pool to cut cold start.
6. A CI tier with a simulated distant origin.
7. Fallthrough counters per reason.

Selected: 1, 2, 6. Item 3 is out of scope (vendor binding). Items 4 and 5 are
ADR-0011 follow-on work. Item 7: the node writes the lane to its log line.

## Reverse brainstorming ("how do we make this fail?")

| Failure we could cause | Prevention |
| --- | --- |
| The node follows an origin redirect, so the client gets other bytes. | Redirect policy: none. |
| The node uses `HTTPS_PROXY` to reach the origin. | `no_proxy()`. |
| The node sends hop-by-hop headers through. | Strip them in both directions. |
| The origin sees every client as the node's IP. | Append `x-forwarded-for`. Set `x-forwarded-host`. |
| Credentials reach the capsule. | The gateway strips them. The node adds no other path. |
| A slow capsule blocks the runtime. | Run the capsule on a blocking thread. |
| Edge responses lack the origin's security headers. | Copy them from an origin probe at start. |
| An unreachable origin hangs or panics the node. | Connect timeout. Answer 502. |
| A large body is buffered in memory. | Stream bodies in both directions. |
| The TTFB test is flaky on slow runners. | Large margin: origin delay 150 ms, edge cost about 2 ms. |
| The probe reports a speed-up with wrong bytes. | Compare every pair. Any divergence fails the run. |
| The `node` feature leaks into the capsule. | Native-only feature. The CI `cargo tree` check stays. |

## Six thinking hats

- **White (facts):** `EdgeGateway` exists. `EdgeArtifact::run` costs about
  1 ms. reqwest 0.13 and tokio are in the graph. CI cannot place a client far
  from the origin.
- **Red (feeling):** A founder wants one command, not a protocol spec.
- **Black (risks):** ADR-0011 says "Autumn ships no reverse proxy". The node
  is a reverse proxy. A simulated delay is not real geography.
- **Yellow (value):** One command closes AC-3 for real deployments. The probe
  makes the metric measurable from any client.
- **Green (ideas):** Copy security headers by probe. Log the lane. Offer
  `--min-reduction` so CI or ops can gate on the metric.
- **Blue (process):** Amend ADR-0011. Red tests first, then green, then
  refactor. CI proof is Tier E in the existing `edge-conformance` job. State
  the simulation clearly in docs.

## Design

- `autumn-edge` feature `node` (native only): `node::HttpOrigin`,
  `node::EdgeNode`, `node::serve`, `node::origin_static_headers`,
  `node::ttfb`.
- `autumn edge serve --capsule <wasm> --origin <url>`.
- `autumn edge ttfb --edge <url> --origin <url> --path <p>`.
- Tier E: origin over real HTTP with a 150 ms delay. The node in front of it.
  The probe must show >= 50% TTFB reduction and zero divergence.

## TDD order

1. Red: `autumn-edge/tests/node.rs` (WAT guests, no wasm target).
2. Green: `node` module, then CLI, then Tier E.
3. Refactor: docs, ADR amendment, changelog fragment, plugin reference.
