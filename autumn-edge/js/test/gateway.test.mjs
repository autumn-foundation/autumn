// The JS gateway's contract, under `node --test`.
//
// The capsules here are assembled byte by byte (see `respond`), so this file
// needs no wasm toolchain. `cargo test -p autumn-edge --features host --test
// js_host` runs it; so does `node --test autumn-edge/js/test/`.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { test } from "node:test";

import {
  Capsule,
  checkResponseHeader,
  createEdgeGateway,
  intoResponse,
  isSafePath,
} from "../autumn-edge-host.mjs";

// ── a tiny wasm assembler ────────────────────────────────────────────

const uleb = (value) => {
  const out = [];
  do {
    let byte = value & 0x7f;
    value >>>= 7;
    if (value !== 0) byte |= 0x80;
    out.push(byte);
  } while (value !== 0);
  return out;
};

const sleb = (value) => {
  const out = [];
  for (;;) {
    const byte = value & 0x7f;
    value >>= 7;
    if ((value === 0 && (byte & 0x40) === 0) || (value === -1 && (byte & 0x40) !== 0)) {
      out.push(byte);
      return out;
    }
    out.push(byte | 0x80);
  }
};

const name = (text) => {
  const bytes = [...new TextEncoder().encode(text)];
  return [...uleb(bytes.length), ...bytes];
};

const section = (id, body) => [id, ...uleb(body.length), ...body];
const vector = (items) => [...uleb(items.length), ...items.flat()];

/// A capsule that writes each of `lines` to stdout, in order, then returns
/// (or traps, with `trap`). It imports only `fd_write`.
function respond(lines, { trap = false } = {}) {
  const encoder = new TextEncoder();
  const DATA = 1024;
  const segments = [];
  const code = [];
  let at = DATA;
  for (const line of lines) {
    const bytes = [...encoder.encode(line)];
    segments.push([0x00, 0x41, ...sleb(at), 0x0b, ...uleb(bytes.length), ...bytes]);
    code.push(
      0x41, 0x00, 0x41, ...sleb(at), 0x36, 0x02, 0x00, // iovec.ptr = at
      0x41, 0x04, 0x41, ...sleb(bytes.length), 0x36, 0x02, 0x00, // iovec.len
      0x41, 0x01, 0x41, 0x00, 0x41, 0x01, 0x41, 0x08, 0x10, 0x00, 0x1a, // fd_write; drop
    );
    at += bytes.length + 8;
  }
  if (trap) code.push(0x00);
  code.push(0x0b);
  const body = [0x00, ...code]; // no locals
  const bytes = [
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
    ...section(1, vector([
      [0x60, 0x04, 0x7f, 0x7f, 0x7f, 0x7f, 0x01, 0x7f],
      [0x60, 0x00, 0x00],
    ])),
    ...section(2, vector([[...name("wasi_snapshot_preview1"), ...name("fd_write"), 0x00, 0x00]])),
    ...section(3, vector([[0x01]])),
    ...section(5, vector([[0x00, ...uleb(Math.ceil((at + 1) / 65536))]])),
    ...section(7, vector([
      [...name("memory"), 0x02, 0x00],
      [...name("_start"), 0x00, 0x01],
    ])),
    ...section(10, vector([[...uleb(body.length), ...body]])),
    ...section(11, vector(segments)),
  ];
  return new WebAssembly.Module(new Uint8Array(bytes));
}

const b64 = (text) => btoa(text);
const served = (status, headers = [], body = "") =>
  JSON.stringify({ op: "response", status, headers, body_b64: b64(body) }) + "\n";
const declined = (reason) =>
  JSON.stringify({ op: "fallthrough", reason, detail: "the edge says no" }) + "\n";

/// An in-process origin that records what reached it.
function recordingOrigin() {
  const seen = [];
  const origin = async (request) => {
    seen.push({
      method: request.method,
      url: request.url,
      headers: Object.fromEntries(request.headers),
      body: request.body ? await request.text() : "",
    });
    return new Response("origin answer", {
      status: 201,
      headers: { "x-origin": "yes", "set-cookie": "sid=1" },
    });
  };
  return { origin, seen };
}

// ── the edge lane ────────────────────────────────────────────────────

test("the assembler builds a capsule the host can run", () => {
  const capsule = new Capsule(respond([served(200)]));
  assert.deepEqual(capsule.imports(), ["wasi_snapshot_preview1::fd_write"]);
  const outcome = capsule.runSync({ method: "GET", uri: "/", headers: [] });
  assert.equal(outcome.kind, "served");
});

test("an edge answer is served and the origin is not asked", async () => {
  const { origin, seen } = recordingOrigin();
  const gateway = createEdgeGateway({
    capsule: respond([served(200, [["content-type", "text/plain"]], "edge answer")]),
    origin,
  });
  const { response, lane } = await gateway.handle(new Request("https://edge.test/greet/ada"));
  assert.equal(lane, "edge");
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("content-type"), "text/plain");
  assert.equal(await response.text(), "edge answer");
  assert.equal(seen.length, 0);
});

test("a HEAD answer has no body", async () => {
  const gateway = createEdgeGateway({
    capsule: respond([served(200, [["content-length", "11"]])]),
    origin: () => assert.fail("the origin is not asked"),
  });
  const { response, lane } = await gateway.handle(
    new Request("https://edge.test/x", { method: "HEAD" }),
  );
  assert.equal(lane, "edge");
  assert.equal(await response.text(), "");
});

test("responseHeaders replace the capsule's and are validated up front", async () => {
  const gateway = createEdgeGateway({
    capsule: respond([served(200, [["x-frame-options", "SAMEORIGIN"]], "x")]),
    origin: () => assert.fail("the origin is not asked"),
    responseHeaders: [
      ["X-Frame-Options", "DENY"],
      ["access-control-allow-origin", "*"],
    ],
  });
  const response = await gateway.fetch(new Request("https://edge.test/x"));
  assert.equal(response.headers.get("x-frame-options"), "DENY");
  assert.equal(response.headers.get("access-control-allow-origin"), "*");
  for (const bad of ["set-cookie", "Content-Length", "x-autumn-edge-fallthrough", "connection"]) {
    assert.throws(() => checkResponseHeader(bad));
    assert.throws(() =>
      createEdgeGateway({ capsule: respond([]), origin: "https://o.test", responseHeaders: [[bad, "1"]] }),
    );
  }
});

// ── what the gateway refuses from a capsule ──────────────────────────

test("an unsafe capsule answer becomes a capsule_error fallthrough", async () => {
  const unsafe = [
    served(200, [["set-cookie", "sid=1"]], "x"),
    served(200, [["x-autumn-edge-fallthrough", "unknown_route"]]),
    served(200, [["transfer-encoding", "chunked"]]),
    served(199),
    served(600),
    served(204, [], "body"),
    served(304, [], "body"),
    served(200, [["content-length", "3"]], "x"),
    served(200, [["content-length", "1"], ["content-length", "2"]], "x"),
    served(204, [["content-length", "0"]]),
    served(205, [["content-length", "1"]]),
    served(200, [["x-bad", "line\u0001"]], "x"),
    served(200, [["bad name", "x"]], "x"),
  ];
  for (const line of unsafe) {
    const { origin, seen } = recordingOrigin();
    const gateway = createEdgeGateway({ capsule: respond([line]), origin });
    const { response, lane, reason } = await gateway.handle(new Request("https://edge.test/x"));
    assert.equal(lane, "fallthrough", line);
    assert.equal(reason, "capsule_error", line);
    assert.equal(response.status, 201, line);
    assert.equal(seen.length, 1, line);
  }
});

test("intoResponse keeps the content-length rules of RFC 9110 8.6", () => {
  const response = (status, headers, body = "") => ({
    status,
    headers,
    body: new TextEncoder().encode(body),
  });
  assert.ok(intoResponse(response(200, [["content-length", "2"]], "hi"), false));
  assert.ok(intoResponse(response(304, [["content-length", "99"]]), false));
  assert.ok(intoResponse(response(205, [["content-length", "0"]]), false));
  assert.ok(intoResponse(response(200, [["content-length", "99"]]), true));
  assert.equal(intoResponse(response(200, [["content-length", "-1"]], ""), false), null);
});

// ── fallthrough ──────────────────────────────────────────────────────

test("every decline forwards the original request to the origin", async () => {
  for (const reason of ["unknown_route", "missing_capability", "capsule_error"]) {
    const { origin, seen } = recordingOrigin();
    const gateway = createEdgeGateway({ capsule: respond([declined(reason)]), origin });
    const { response, lane, reason: why } = await gateway.handle(
      new Request("https://edge.test/p?q=1", {
        headers: { cookie: "sid=secret", authorization: "Bearer t", accept: "text/html" },
      }),
    );
    assert.equal(lane, "fallthrough");
    assert.equal(why, reason);
    assert.equal(response.status, 201);
    assert.equal(response.headers.get("set-cookie"), "sid=1");
    assert.equal(await response.text(), "origin answer");
    // The origin gets the credentials the capsule never saw.
    assert.equal(seen[0].headers.cookie, "sid=secret");
    assert.equal(seen[0].headers.authorization, "Bearer t");
    assert.equal(seen[0].url, "https://edge.test/p?q=1");
  }
});

test("a trap and a silent capsule fall through", async () => {
  for (const capsule of [respond([], { trap: true }), respond([])]) {
    const { origin } = recordingOrigin();
    const gateway = createEdgeGateway({ capsule, origin });
    const { lane, reason } = await gateway.handle(new Request("https://edge.test/x"));
    assert.equal(lane, "fallthrough");
    assert.equal(reason, "capsule_error");
  }
});

test("a write goes to the origin with its body, and the capsule is not run", async () => {
  const { origin, seen } = recordingOrigin();
  const gateway = createEdgeGateway({ capsule: respond([], { trap: true }), origin });
  const { lane, reason } = await gateway.handle(
    new Request("https://edge.test/feedback", { method: "POST", body: "hello" }),
  );
  assert.equal(lane, "fallthrough");
  assert.equal(reason, "method_not_edge_eligible");
  assert.equal(seen[0].method, "POST");
  assert.equal(seen[0].body, "hello");
});

test("an upgrade handshake goes to the origin, with its pair kept", async () => {
  const { origin, seen } = recordingOrigin();
  const gateway = createEdgeGateway({ capsule: respond([served(200)]), origin });
  const { lane } = await gateway.handle(
    new Request("https://edge.test/ws", {
      headers: { connection: "keep-alive, Upgrade", upgrade: "websocket" },
    }),
  );
  assert.equal(lane, "origin_only");
  assert.equal(seen[0].headers.connection, "upgrade");
  assert.equal(seen[0].headers.upgrade, "websocket");
});

test("a non-ASCII header value is origin-only", async () => {
  const { origin } = recordingOrigin();
  const gateway = createEdgeGateway({ capsule: respond([served(200)]), origin });
  const headers = new Headers();
  headers.set("x-name", "café");
  const { lane } = await gateway.handle(new Request("https://edge.test/x", { headers }));
  assert.equal(lane, "origin_only");
});

test("the gateway is the first proxy: forwarded headers are its own", async () => {
  const { origin, seen } = recordingOrigin();
  const gateway = createEdgeGateway({
    capsule: respond([declined("unknown_route")]),
    origin,
    clientIp: () => "203.0.113.7",
  });
  await gateway.handle(
    new Request("https://edge.test/x", {
      headers: {
        "x-forwarded-for": "10.0.0.1",
        "x-forwarded-host": "evil.test",
        "x-forwarded-proto": "http",
        forwarded: "for=10.0.0.1",
        "x-real-ip": "10.0.0.1",
        "keep-alive": "timeout=5",
        connection: "x-drop-me",
        "x-drop-me": "1",
        te: "trailers",
      },
    }),
  );
  const headers = seen[0].headers;
  assert.equal(headers["x-forwarded-for"], "203.0.113.7");
  assert.equal(headers["x-forwarded-host"], "edge.test");
  assert.equal(headers["x-forwarded-proto"], "https");
  for (const gone of ["forwarded", "x-real-ip", "keep-alive", "connection", "x-drop-me", "te"]) {
    assert.equal(headers[gone], undefined, gone);
  }
});

test("a dot segment or a backslash is refused, never forwarded", () => {
  assert.ok(isSafePath("/a/b?c=../d"));
  for (const path of ["/a/../b", "/a/./b", "/a/%2e%2E/b", "/a/%2e", "a/b", "/a\\b"]) {
    assert.equal(isSafePath(path), false, path);
  }
});

// ── kv ───────────────────────────────────────────────────────────────

test("kv may be a Map, a function, or a Workers-style binding", async () => {
  const capsule = respond([JSON.stringify({ op: "kv_get", key: "k" }) + "\n", served(200)]);
  const workersKv = {
    reads: [],
    async get(key, type) {
      this.reads.push([key, type]);
      return new TextEncoder().encode("v").buffer;
    },
  };
  for (const kv of [new Map([["k", "v"]]), async () => "v", workersKv]) {
    const gateway = createEdgeGateway({ capsule, origin: () => assert.fail("not asked"), kv });
    const { lane } = await gateway.handle(new Request("https://edge.test/x"));
    assert.equal(lane, "edge");
  }
  assert.deepEqual(workersKv.reads, [["k", "arrayBuffer"]]);
});

test("a failing kv read is a miss, not an error", async () => {
  const capsule = new Capsule(
    respond([JSON.stringify({ op: "kv_get", key: "k" }) + "\n", served(200)]),
  );
  const outcome = await capsule.run(
    { method: "GET", uri: "/", headers: [] },
    { kv: async () => { throw new Error("kv is down"); } },
  );
  assert.equal(outcome.kind, "served");
});

test("preload reads keys before the first run", async () => {
  const reads = [];
  const capsule = new Capsule(
    respond([JSON.stringify({ op: "kv_get", key: "k" }) + "\n", served(200)]),
  );
  await capsule.run(
    { method: "GET", uri: "/", headers: [] },
    { kv: async (key) => (reads.push(key), "v"), preload: ["k", "k"] },
  );
  assert.deepEqual(reads, ["k"]);
});

// ── identity ─────────────────────────────────────────────────────────

test("identity claims cross the wire; a resolver failure goes to the origin", async () => {
  const capsule = respond([served(200)]);
  let frame = null;
  const original = Capsule.prototype.run;
  Capsule.prototype.run = function (request, options) {
    frame = request;
    return original.call(this, request, options);
  };
  try {
    const gateway = createEdgeGateway({
      capsule,
      origin: () => assert.fail("not asked"),
      identity: async () => ({ user_id: "u-7", roles: ["reader"] }),
    });
    await gateway.handle(new Request("https://edge.test/me"));
    assert.deepEqual(frame.identity, { user_id: "u-7", roles: ["reader"] });
  } finally {
    Capsule.prototype.run = original;
  }
  const { origin, seen } = recordingOrigin();
  const failing = createEdgeGateway({
    capsule,
    origin,
    identity: async () => { throw new Error("session store down"); },
  });
  const { lane } = await failing.handle(new Request("https://edge.test/me"));
  assert.equal(lane, "fallthrough");
  assert.equal(seen.length, 1);
});

// ── a URL origin over real HTTP ──────────────────────────────────────

function listen(handler) {
  return new Promise((resolve) => {
    const server = createServer(handler);
    server.listen(0, "127.0.0.1", () => resolve(server));
  });
}

test("a URL origin: no redirects followed, hop-by-hop stripped, headers probed", async () => {
  const seen = [];
  let probes = 0;
  const server = await listen((request, response) => {
    seen.push({ url: request.url, headers: request.headers });
    if (request.url === "/") {
      probes += 1;
      response.writeHead(200, {
        "x-frame-options": "DENY",
        "x-content-type-options": "nosniff",
        "content-security-policy": `script-src 'nonce-${probes}'`,
        "access-control-allow-origin": "https://per-route.test",
      });
      return response.end("home");
    }
    if (request.url === "/moved") {
      response.writeHead(302, { location: "/elsewhere", "keep-alive": "timeout=5" });
      return response.end();
    }
    response.writeHead(200, { "content-type": "text/plain" });
    response.end("origin");
  });
  const base = `http://127.0.0.1:${server.address().port}`;
  try {
    const gateway = createEdgeGateway({
      capsule: respond([declined("unknown_route")]),
      origin: base + "/",
      probeOrigin: true,
    });
    const moved = await gateway.fetch(new Request("https://edge.test/moved"));
    assert.equal(moved.status, 302);
    assert.equal(moved.headers.get("location"), "/elsewhere");
    assert.equal(moved.headers.get("keep-alive"), null);
    assert.equal(seen.at(-1).headers["x-forwarded-host"], "edge.test");

    const edge = createEdgeGateway({
      capsule: respond([served(200, [], "edge")]),
      origin: base,
      probeOrigin: true,
    });
    const first = await edge.fetch(new Request("https://edge.test/a"));
    await edge.fetch(new Request("https://edge.test/b"));
    assert.equal(first.headers.get("x-frame-options"), "DENY");
    assert.equal(first.headers.get("x-content-type-options"), "nosniff");
    // A nonce changes between probes and CORS can differ per route: neither
    // is copied.
    assert.equal(first.headers.get("content-security-policy"), null);
    assert.equal(first.headers.get("access-control-allow-origin"), null);
    assert.equal(probes, 2, "two probe GETs, once per gateway");
  } finally {
    server.close();
  }
});

test("an unreachable URL origin is a 502", async () => {
  const gateway = createEdgeGateway({
    capsule: respond([declined("unknown_route")]),
    origin: "http://127.0.0.1:9",
  });
  const response = await gateway.fetch(new Request("https://edge.test/x"));
  assert.equal(response.status, 502);
});

test("onLane sees every answer, and a throwing hook changes nothing", async () => {
  const lanes = [];
  const gateway = createEdgeGateway({
    capsule: respond([served(200)]),
    origin: () => new Response("o"),
    onLane: ({ lane }) => {
      lanes.push(lane);
      throw new Error("a broken logger");
    },
  });
  const response = await gateway.fetch(new Request("https://edge.test/x"));
  assert.equal(response.status, 200);
  assert.deepEqual(lanes, ["edge"]);
});
