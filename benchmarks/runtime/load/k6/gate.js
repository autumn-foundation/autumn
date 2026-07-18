// k6 gate profile — runtime latency CI gate for Autumn.
//
// Exercises the two gated read paths:
//   GET /api/posts   (JSON list — 50 most-recent posts)
//   GET /posts       (server-rendered HTML list)
//
// Pinned defaults (do not change without re-baselining budgets.toml):
//   VUS      = 20
//   DURATION = 30s
//
// Produces gate-summary.json keyed by the exact budget path strings so that
// bench-runtime-gate can compare against budgets.toml without any mapping.
//
// Flake policy (documented in benchmarks/runtime/README.md):
//   1. Run once as a discarded warmup (the CI workflow does this).
//   2. Run R=3 measured times; bench-runtime-gate takes the median p99.
//   3. Budgets include 1.20x headroom over the baseline median p99.
//   4. A single re-run is allowed on a suspected flake.
//
// Usage:
//   BASE_URL=http://localhost:8080 k6 run load/k6/gate.js
//   BASE_URL=http://localhost:8080 k6 run --summary-export run-1/gate-summary.json load/k6/gate.js

import http from "k6/http";
import { check, sleep } from "k6";
import { Trend } from "k6/metrics";

const BASE_URL = __ENV.BASE_URL || "http://localhost:8080";

// Pinned: do not override VUS/DURATION without re-baselining.
export const options = {
  vus: 20,
  duration: "30s",
  summaryTrendStats: ["avg", "p(95)", "p(99)", "max"],
  thresholds: {
    http_req_failed: ["rate<0.01"],
  },
};

// One Trend per gated path — named without spaces/slashes for k6 compat.
const trendApiPosts = new Trend("gate_api_posts", true);
const trendHtmlPosts = new Trend("gate_html_posts", true);

export default function () {
  // GET /api/posts — JSON list
  const apiRes = http.get(`${BASE_URL}/api/posts`, {
    headers: { Accept: "application/json" },
  });
  trendApiPosts.add(apiRes.timings.duration);
  check(apiRes, { "GET /api/posts 200": (r) => r.status === 200 });

  sleep(0.05);

  // GET /posts — HTML list
  const htmlRes = http.get(`${BASE_URL}/posts`);
  trendHtmlPosts.add(htmlRes.timings.duration);
  check(htmlRes, {
    "GET /posts 200": (r) => r.status === 200,
    "GET /posts contains h1": (r) => (r.body || "").includes("<h1>"),
  });

  sleep(0.05);
}

// Emit gate-summary.json keyed by the exact budget path strings.
// bench-runtime-gate reads this file to get p99_ms per path.
export function handleSummary(data) {
  // Use a large sentinel when a metric is absent (app down, no responses recorded)
  // so bench-runtime-gate fails the gate rather than silently passing with 0ms.
  // Optional chaining guards against metrics or values being undefined/null,
  // which would otherwise throw a TypeError and crash handleSummary.
  const MISSING = 999_999;
  const apiP99 = data.metrics["gate_api_posts"]?.values?.["p(99)"] ?? MISSING;
  const htmlP99 = data.metrics["gate_html_posts"]?.values?.["p(99)"] ?? MISSING;

  const summary = {
    "GET /api/posts": { p99_ms: apiP99 },
    "GET /posts": { p99_ms: htmlP99 },
  };

  return {
    "gate-summary.json": JSON.stringify(summary, null, 2),
    stdout: "\n",
  };
}
