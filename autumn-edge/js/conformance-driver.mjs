// Runs a capsule through the JS host for every case in a job file and
// prints one NDJSON line per case, in order:
//
//   {"sync": <guest frame>, "replay": <guest frame>, "kv_reads": ["key", ...]}
//
// `sync` is `Capsule.runSync` over a synchronous snapshot of the whole KV;
// `replay` is `Capsule.run` over an asynchronous source, which exercises
// replay-on-miss. Each frame is the outcome as a guest frame
// (`{"op":"response",...}` or `{"op":"fallthrough",...}`), so a Rust caller
// parses it with `from_line::<GuestFrame>` and compares it with the
// reference host's outcome.
//
// Usage: node conformance-driver.mjs <job.json>
//
// job.json:
//   {
//     "wasm": "path/to/edge-capsule.wasm",
//     "kv": { "key": "<base64 value>" },
//     "cases": [
//       { "method": "GET", "uri": "/x", "headers": [["accept", "text/html"]],
//         "capabilities": ["kv"], "identity": null }
//     ]
//   }
import { readFileSync } from "node:fs";
import { Capsule, decodeBase64, outcomeToFrame } from "./autumn-edge-host.mjs";

const job = JSON.parse(readFileSync(process.argv[2], "utf8"));
const capsule = await Capsule.load(readFileSync(job.wasm));
const entries = new Map(
  Object.entries(job.kv || {}).map(([key, value]) => [key, decodeBase64(value)]),
);

const lines = [];
for (const testCase of job.cases) {
  const request = {
    method: testCase.method,
    uri: testCase.uri,
    headers: testCase.headers || [],
    identity: testCase.identity || null,
  };
  const capabilities = testCase.capabilities || [];
  const sync = capsule.runSync(request, {
    capabilities,
    kv: (key) => (entries.has(key) ? entries.get(key) : null),
  });
  const reads = [];
  const replay = capabilities.includes("kv")
    ? await capsule.run(request, {
        capabilities,
        kv: async (key) => {
          reads.push(key);
          return entries.has(key) ? entries.get(key) : null;
        },
      })
    : await capsule.run(request, { capabilities });
  lines.push(
    JSON.stringify({
      sync: outcomeToFrame(sync),
      replay: outcomeToFrame(replay),
      kv_reads: reads,
    }),
  );
}
process.stdout.write(lines.length ? lines.join("\n") + "\n" : "");
