// autumn-edge-host.mjs — the web-standard host for Autumn edge capsules.
//
// One ES module, no dependencies, no vendor SDK. It uses only what the
// WinterTC Minimum Common API guarantees — `WebAssembly`, `fetch`,
// `Request`/`Response`/`Headers`, `URL`, `TextEncoder`/`TextDecoder`,
// `atob`/`btoa` — so the same file runs inside a Cloudflare Worker, Deno,
// Bun, Vercel/Netlify edge functions and Node 18+. See ADR-0024.
//
// Two layers:
//
//   Capsule        — the reference host (`autumn-edge/src/host.rs`) in
//                    JavaScript: the same WASI preview-1 shim, the same NDJSON
//                    dialogue (wire protocol v1), the same budgets, the same
//                    fixed clock and fixed-seed entropy. The edge-greeting
//                    conformance suite runs both hosts over one corpus and
//                    requires identical outcomes.
//   EdgeGateway    — `autumn_edge::gateway` + the edge node's forwarding
//                    rules over the Fetch API: GET/HEAD go to the capsule, the
//                    capsule's answer is checked, and every decline is
//                    forwarded to the origin unchanged.
//
// A KV store on an edge platform is asynchronous; a WASI call is not. The
// capsule asks for a key with a synchronous `fd_write`, so `Capsule.run`
// answers from a per-request snapshot and, on a key it does not hold yet,
// unwinds the instance, awaits the read, and runs the request again from the
// start. That replay is sound because the host makes a capsule deterministic:
// the clock is fixed, the entropy is fixed, and the only input is the frame.
// A request that reads N keys costs at most N + 1 instantiations.
//
// Experimental, like the wire protocol it speaks (issue #1790).

export const WIRE_VERSION = 1;

/// The response header a handler sets to decline (never reaches a client).
export const FALLTHROUGH_SENTINEL = "x-autumn-edge-fallthrough";

/// Request headers that never reach a capsule.
export const SENSITIVE_HEADERS = Object.freeze([
  "cookie",
  "authorization",
  "proxy-authorization",
  FALLTHROUGH_SENTINEL,
]);

/// Headers that describe one connection or hop (RFC 9110 7.6.1 and 11.7).
export const HOP_BY_HOP = Object.freeze([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authentication-info",
  "proxy-authorization",
  "proxy-connection",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
]);

/// The origin's static security headers, copied onto edge responses by the
/// origin probe. Same list as `autumn_edge::conformance::SECURITY_HEADERS`.
export const SECURITY_HEADERS = Object.freeze([
  "permissions-policy",
  "referrer-policy",
  "strict-transport-security",
  "x-content-type-options",
  "x-frame-options",
  "x-xss-protection",
]);

export const FALLTHROUGH_REASONS = Object.freeze([
  "unknown_route",
  "method_not_edge_eligible",
  "missing_capability",
  "capsule_error",
]);

/// Same budgets as `autumn_edge::host`.
export const MEMORY_BUDGET_BYTES = 256 * 1024 * 1024;
export const STDOUT_LINE_BUDGET_BYTES = 64 * 1024 * 1024;
export const STDERR_BUDGET_BYTES = 1024 * 1024;
const STDERR_EXCERPT = 512;

/// Distinct KV keys one request may read before it falls through.
export const MAX_KV_ROUNDS = 32;

const ERRNO = Object.freeze({ SUCCESS: 0, BADF: 8, INVAL: 28, NOSPC: 51, SPIPE: 70 });
const FDSTAT_SIZE = 24;
const RANDOM_SEED = 0x9e3779b97f4a7c15n;
const MASK64 = 0xffffffffffffffffn;

const encoder = new TextEncoder();
const lossy = new TextDecoder("utf-8");

// ── base64 ───────────────────────────────────────────────────────────

export function encodeBase64(bytes) {
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode.apply(null, bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

const BASE64 = /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/;

/// Strict standard base64, as the Rust side decodes it: padding required,
/// no whitespace. Throws on anything else.
export function decodeBase64(text) {
  if (!BASE64.test(text)) {
    throw new Error("invalid base64");
  }
  const binary = atob(text);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}

// ── wire helpers ─────────────────────────────────────────────────────

/// Names lowercased, stably sorted by name: insertion order is kept within
/// a repeated name. Same as `autumn_edge::wire::canonicalize_headers`.
export function canonicalizeHeaders(pairs) {
  return pairs
    .map(([name, value]) => [String(name).toLowerCase(), String(value)])
    .sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
}

export function stripSensitiveHeaders(pairs) {
  return pairs.filter(([name]) => !SENSITIVE_HEADERS.includes(String(name).toLowerCase()));
}

/// The host → guest `request` frame. `request` is
/// `{ method, uri, headers: [[name, value]], body?: Uint8Array, identity? }`.
export function requestFrame(request, capabilities = []) {
  const frame = {
    op: "request",
    wire_version: WIRE_VERSION,
    provided_capabilities: [...capabilities],
    method: request.method,
    uri: request.uri,
    headers: canonicalizeHeaders(stripSensitiveHeaders(request.headers || [])),
    body_b64: encodeBase64(request.body || new Uint8Array(0)),
  };
  if (request.identity) {
    frame.identity = {
      user_id: String(request.identity.user_id),
      roles: (request.identity.roles || []).map(String),
    };
  }
  return frame;
}

export function fallthrough(reason, detail) {
  return { kind: "fallthrough", reason, detail };
}

/// An outcome as the guest frame that carries it — the shape the Rust
/// conformance suite parses with `from_line::<GuestFrame>`.
export function outcomeToFrame(outcome) {
  if (outcome.kind === "served") {
    const { status, headers, body } = outcome.response;
    return { op: "response", status, headers, body_b64: encodeBase64(body) };
  }
  return { op: "fallthrough", reason: outcome.reason, detail: outcome.detail };
}

const isPair = (pair) =>
  Array.isArray(pair) &&
  pair.length === 2 &&
  typeof pair[0] === "string" &&
  typeof pair[1] === "string";

/// Parse one guest line strictly, as `from_line::<GuestFrame>` does.
function parseGuestFrame(line) {
  const frame = JSON.parse(line);
  if (frame === null || typeof frame !== "object" || Array.isArray(frame)) {
    throw new Error("a frame must be a JSON object");
  }
  switch (frame.op) {
    case "kv_get":
      if (typeof frame.key !== "string") throw new Error("missing field `key`");
      return { op: "kv_get", key: frame.key };
    case "response": {
      const { status } = frame;
      if (!Number.isInteger(status) || status < 0 || status > 0xffff) {
        throw new Error("`status` must be a u16");
      }
      const headers = frame.headers === undefined ? [] : frame.headers;
      if (!Array.isArray(headers) || !headers.every(isPair)) {
        throw new Error("`headers` must be a list of [name, value] string pairs");
      }
      const body = frame.body_b64 === undefined ? "" : frame.body_b64;
      if (typeof body !== "string") throw new Error("`body_b64` must be a string");
      return {
        op: "response",
        response: { status, headers: headers.map(([n, v]) => [n, v]), body: decodeBase64(body) },
      };
    }
    case "fallthrough":
      if (!FALLTHROUGH_REASONS.includes(frame.reason)) {
        throw new Error(`unknown edge fallthrough reason \`${frame.reason}\``);
      }
      if (typeof frame.detail !== "string") throw new Error("missing field `detail`");
      return { op: "fallthrough", reason: frame.reason, detail: frame.detail };
    default:
      throw new Error(`unknown variant \`${frame.op}\``);
  }
}

// ── the capsule host ─────────────────────────────────────────────────

/// Thrown out of the guest to unwind it when it asks for a key the snapshot
/// does not hold yet. Never escapes `Capsule.run`.
class KvPending extends Error {
  constructor(key) {
    super(`kv key \`${key}\` is not loaded yet`);
    this.key = key;
  }
}

function toBytes(value) {
  if (value === null || value === undefined) return null;
  if (typeof value === "string") return encoder.encode(value);
  if (value instanceof Uint8Array) return value;
  if (value instanceof ArrayBuffer) return new Uint8Array(value);
  if (ArrayBuffer.isView(value)) {
    return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  }
  throw new TypeError("a kv value must be a string, bytes, null or undefined");
}

/// One read from a KV source: a function `(key) => value`, or an object with
/// `get(key, "arrayBuffer")` — a `Map`, or a Cloudflare Workers KV binding.
/// Any failure is a miss: `EdgeKv` is a non-authoritative accelerator.
async function readKv(kv, key) {
  try {
    const value = typeof kv === "function" ? await kv(key) : await kv.get(key, "arrayBuffer");
    return toBytes(value);
  } catch {
    return null;
  }
}

class HostState {
  constructor(lookup, kvProvided) {
    this.stdin = [];
    this.lineParts = [];
    this.lineLength = 0;
    this.stderr = [];
    this.stderrLength = 0;
    this.lookup = lookup;
    this.kvProvided = kvProvided;
    this.terminal = null;
    this.random = RANDOM_SEED;
  }

  feed(text) {
    this.stdin.push(encoder.encode(text));
  }

  takeStdin(length) {
    const parts = [];
    let taken = 0;
    while (taken < length && this.stdin.length > 0) {
      const head = this.stdin[0];
      const step = Math.min(length - taken, head.length);
      parts.push(head.subarray(0, step));
      if (step === head.length) this.stdin.shift();
      else this.stdin[0] = head.subarray(step);
      taken += step;
    }
    return concat(parts, taken);
  }

  onGuestLine(line) {
    if (this.terminal) return; // anything after the answer is noise
    let frame;
    try {
      frame = parseGuestFrame(line.replace(/[\r\n]+$/, ""));
    } catch (err) {
      this.terminal = fallthrough(
        "capsule_error",
        `the capsule emitted a malformed frame: ${messageOf(err)}`,
      );
      return;
    }
    if (frame.op === "kv_get") {
      const value = this.kvProvided ? this.lookup(frame.key) : null;
      this.feed(
        JSON.stringify({ op: "kv_value", value_b64: value ? encodeBase64(value) : null }) + "\n",
      );
    } else if (frame.op === "response") {
      this.terminal = { kind: "served", response: frame.response };
    } else {
      this.terminal = fallthrough(frame.reason, frame.detail);
    }
  }

  /// `false` once one line outgrows `STDOUT_LINE_BUDGET_BYTES`.
  writeStdout(bytes) {
    let start = 0;
    while (start < bytes.length) {
      const newline = bytes.indexOf(10, start);
      const end = newline === -1 ? bytes.length : newline;
      if (this.lineLength + (end - start) > STDOUT_LINE_BUDGET_BYTES) return false;
      this.lineParts.push(bytes.slice(start, end));
      this.lineLength += end - start;
      if (newline === -1) break;
      const line = concat(this.lineParts, this.lineLength);
      this.lineParts = [];
      this.lineLength = 0;
      this.onGuestLine(lossy.decode(line));
      start = newline + 1;
    }
    return true;
  }

  writeStderr(bytes) {
    const room = STDERR_BUDGET_BYTES - this.stderrLength;
    if (room <= 0) return;
    const kept = bytes.slice(0, room);
    this.stderr.push(kept);
    this.stderrLength += kept.length;
  }

  stderrExcerpt() {
    const text = lossy.decode(concat(this.stderr, this.stderrLength)).trim();
    return [...text].slice(0, STDERR_EXCERPT).join("");
  }

  /// SplitMix64, seeded like the reference host: the same bytes every run.
  nextRandomByte() {
    this.random = (this.random + 0x9e3779b97f4a7c15n) & MASK64;
    let z = this.random;
    z = ((z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n) & MASK64;
    z = ((z ^ (z >> 27n)) * 0x94d049bb133111ebn) & MASK64;
    return Number((z ^ (z >> 31n)) & 0xffn);
  }
}

function concat(parts, length) {
  if (parts.length === 1) return parts[0];
  const out = new Uint8Array(length);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

function messageOf(err) {
  return err && err.message ? err.message : String(err);
}

function capsuleError(state, detail) {
  const stderr = state.stderrExcerpt();
  return fallthrough("capsule_error", stderr ? `${detail} (stderr: ${stderr})` : detail);
}

/// The WASI preview-1 surface a capsule gets: exactly the reference host's,
/// with the same errno for every refusal. No `path_open`, no resolving
/// `fd_prestat_*`, no sockets — a capsule importing one fails to link.
function wasiImports(state, memoryOf) {
  // Guest pointers arrive as signed i32; a negative one is out of range, as
  // `usize::try_from` makes it on the Rust side.
  const inBounds = (memory, at, length) =>
    at >= 0 && length >= 0 && at + length <= memory.buffer.byteLength;
  const readU32 = (memory, at) =>
    inBounds(memory, at, 4) ? new DataView(memory.buffer).getUint32(at, true) : null;
  const writeBytes = (memory, at, bytes) => {
    if (!inBounds(memory, at, bytes.length)) return false;
    new Uint8Array(memory.buffer).set(bytes, at);
    return true;
  };
  const writeU32 = (memory, at, value) => {
    const bytes = new Uint8Array(4);
    new DataView(bytes.buffer).setUint32(0, value, true);
    return writeBytes(memory, at, bytes);
  };
  const iovec = (memory, base, index) => {
    if (base < 0) return null;
    const at = base + index * 8;
    const pointer = readU32(memory, at);
    const length = readU32(memory, at + 4);
    return pointer === null || length === null ? null : [pointer, length];
  };
  const twoZeroes = (first, second) => {
    const memory = memoryOf();
    if (!memory) return ERRNO.INVAL;
    return writeU32(memory, first, 0) && writeU32(memory, second, 0)
      ? ERRNO.SUCCESS
      : ERRNO.INVAL;
  };

  return {
    fd_read(fd, iovs, iovsLength, nread) {
      if (fd !== 0) return ERRNO.BADF;
      const memory = memoryOf();
      if (!memory) return ERRNO.INVAL;
      let total = 0;
      for (let index = 0; index < iovsLength; index++) {
        const vec = iovec(memory, iovs, index);
        if (!vec) return ERRNO.INVAL;
        const [pointer, length] = vec;
        const chunk = state.takeStdin(length);
        if (!writeBytes(memory, pointer, chunk)) return ERRNO.INVAL;
        total += chunk.length;
        if (total > 0xffffffff) return ERRNO.INVAL;
        if (chunk.length < length) break;
      }
      return writeU32(memory, nread, total) ? ERRNO.SUCCESS : ERRNO.INVAL;
    },

    fd_write(fd, iovs, iovsLength, nwritten) {
      if (fd !== 1 && fd !== 2) return ERRNO.BADF;
      const memory = memoryOf();
      if (!memory) return ERRNO.INVAL;
      let total = 0;
      for (let index = 0; index < iovsLength; index++) {
        const vec = iovec(memory, iovs, index);
        if (!vec) return ERRNO.INVAL;
        const [pointer, length] = vec;
        // Bounds first: `length` is guest-chosen.
        if (!inBounds(memory, pointer, length)) return ERRNO.INVAL;
        total += length;
        if (total > 0xffffffff) return ERRNO.INVAL;
        // A view, not a copy: the state copies what it keeps. The lookup a
        // `kv_get` line triggers may unwind from here (`KvPending`).
        const bytes = new Uint8Array(memory.buffer, pointer, length);
        if (fd === 2) state.writeStderr(bytes);
        else if (!state.writeStdout(bytes)) return ERRNO.NOSPC;
      }
      return writeU32(memory, nwritten, total) ? ERRNO.SUCCESS : ERRNO.INVAL;
    },

    random_get(pointer, length) {
      const memory = memoryOf();
      if (!memory) return ERRNO.INVAL;
      if (!inBounds(memory, pointer, length)) return ERRNO.INVAL;
      const bytes = new Uint8Array(length);
      for (let i = 0; i < length; i++) bytes[i] = state.nextRandomByte();
      return writeBytes(memory, pointer, bytes) ? ERRNO.SUCCESS : ERRNO.INVAL;
    },

    clock_time_get(_clock, _precision, out) {
      const memory = memoryOf();
      if (!memory) return ERRNO.INVAL;
      // Time stands still at the edge, as in the reference host.
      return writeBytes(memory, out, new Uint8Array(8)) ? ERRNO.SUCCESS : ERRNO.INVAL;
    },

    environ_get: () => ERRNO.SUCCESS,
    environ_sizes_get: twoZeroes,
    args_get: () => ERRNO.SUCCESS,
    args_sizes_get: twoZeroes,
    // Returns instead of throwing: the terminal frame decides the outcome.
    proc_exit() {},
    sched_yield: () => ERRNO.SUCCESS,
    fd_close: () => ERRNO.SUCCESS,
    fd_seek: () => ERRNO.SPIPE,

    fd_fdstat_get(fd, out) {
      if (fd < 0 || fd > 2) return ERRNO.BADF;
      const memory = memoryOf();
      if (!memory) return ERRNO.INVAL;
      const fdstat = new Uint8Array(FDSTAT_SIZE);
      fdstat[0] = 2; // character device: stdio is a pipe and nothing more
      return writeBytes(memory, out, fdstat) ? ERRNO.SUCCESS : ERRNO.INVAL;
    },

    // No preopened directories, ever.
    fd_prestat_get: () => ERRNO.BADF,
    fd_prestat_dir_name: () => ERRNO.BADF,
  };
}

/// A compiled capsule. Compile once per isolate; run once per request.
export class Capsule {
  /// `module` is a `WebAssembly.Module` (what a Workers `.wasm` import is).
  constructor(module) {
    if (!(module instanceof WebAssembly.Module)) {
      throw new TypeError("Capsule needs a WebAssembly.Module; use Capsule.load for bytes");
    }
    this.module = module;
  }

  /// From a `WebAssembly.Module`, bytes, a `Response`, or a promise of one.
  static async load(source) {
    const resolved = await source;
    if (resolved instanceof Capsule) return resolved;
    if (resolved instanceof WebAssembly.Module) return new Capsule(resolved);
    if (typeof Response !== "undefined" && resolved instanceof Response) {
      return new Capsule(await WebAssembly.compile(await resolved.arrayBuffer()));
    }
    return new Capsule(await WebAssembly.compile(toBytes(resolved)));
  }

  /// `module::name` of every import, for an allowlist check.
  imports() {
    return WebAssembly.Module.imports(this.module).map((i) => `${i.module}::${i.name}`);
  }

  /// Serve one request synchronously. `kv` is `(key) => Uint8Array | null`;
  /// it is only consulted when `capabilities` includes `"kv"`.
  runSync(request, { capabilities = [], kv = () => null } = {}) {
    const state = new HostState((key) => toBytes(kv(key)), capabilities.includes("kv"));
    state.feed(JSON.stringify(requestFrame(request, capabilities)) + "\n");

    let instance = null;
    const memoryOf = () => {
      const memory = instance && instance.exports.memory;
      return memory instanceof WebAssembly.Memory ? memory : null;
    };
    try {
      instance = new WebAssembly.Instance(this.module, {
        wasi_snapshot_preview1: wasiImports(state, memoryOf),
      });
    } catch (err) {
      if (err instanceof KvPending) throw err;
      return capsuleError(state, `the capsule could not be instantiated: ${messageOf(err)}`);
    }
    const memory = memoryOf();
    if (memory && memory.buffer.byteLength > MEMORY_BUDGET_BYTES) {
      return capsuleError(
        state,
        `the capsule could not be instantiated: its memory exceeds the ${MEMORY_BUDGET_BYTES}-byte budget`,
      );
    }
    const start = instance.exports._start;
    if (typeof start !== "function") {
      return capsuleError(
        state,
        "the capsule exports no `_start`; it must be built as a wasm32-wasip1 *command*",
      );
    }

    let trap = null;
    try {
      start();
    } catch (err) {
      if (err instanceof KvPending) throw err;
      trap = err;
    }
    if (state.terminal) return state.terminal;
    const grown = memoryOf();
    if (grown && grown.buffer.byteLength > MEMORY_BUDGET_BYTES) {
      return capsuleError(state, `the capsule outgrew its ${MEMORY_BUDGET_BYTES}-byte memory budget`);
    }
    return capsuleError(
      state,
      trap ? `the capsule trapped: ${messageOf(trap)}` : "the capsule exited without answering",
    );
  }

  /// Serve one request against an asynchronous KV source (see `readKv`):
  /// replay-on-miss over a per-request snapshot. `preload` names keys to
  /// read before the first run, which saves one instantiation per key.
  async run(request, { capabilities = [], kv = null, preload = [] } = {}) {
    const caps = [...capabilities];
    if (kv && !caps.includes("kv")) caps.push("kv");
    if (!kv) return this.runSync(request, { capabilities: caps });

    const snapshot = new Map();
    await Promise.all(
      [...new Set(preload)].map(async (key) => snapshot.set(key, await readKv(kv, key))),
    );
    const lookup = (key) => {
      if (snapshot.has(key)) return snapshot.get(key);
      throw new KvPending(key);
    };
    for (let round = 0; ; round++) {
      try {
        return this.runSync(request, { capabilities: caps, kv: lookup });
      } catch (err) {
        if (!(err instanceof KvPending)) throw err;
        if (round >= MAX_KV_ROUNDS) {
          return fallthrough(
            "capsule_error",
            `the capsule read more than ${MAX_KV_ROUNDS} distinct kv keys in one request`,
          );
        }
        snapshot.set(err.key, await readKv(kv, err.key));
      }
    }
  }
}

// ── the gateway ──────────────────────────────────────────────────────

const NO_BODY_STATUSES = [204, 205, 304];
const HEADER_VALUE = /^[\t\x20-\x7e]*$/;

function isUnsafeResponseHeader(name) {
  return name === "set-cookie" || name === FALLTHROUGH_SENTINEL || HOP_BY_HOP.includes(name);
}

/// Throws on a header `responseHeaders` may not set, as
/// `EdgeGateway::with_response_headers` panics on one.
export function checkResponseHeader(name) {
  const lower = String(name).toLowerCase();
  if (isUnsafeResponseHeader(lower) || lower === "content-length") {
    throw new Error(
      `cannot set \`${lower}\`: it changes framing, session or hop state, which the gateway checks on the capsule's response`,
    );
  }
}

function contentLengthFits(status, head, declared, bodyLength) {
  switch (status) {
    case 204:
      return false;
    case 205:
      return declared === 0;
    case 304:
      return true;
    default:
      return head || declared === bodyLength;
  }
}

/// The capsule's answer as a `Response`, or `null` when the gateway refuses
/// it — the same checks as `autumn_edge::gateway`: a status outside 200-599,
/// an invalid header, `set-cookie`, the sentinel, a hop-by-hop header, a body
/// on HEAD/204/205/304, or a `content-length` that does not fit.
export function intoResponse(response, head) {
  const { status, headers, body } = response;
  const noBody = head || NO_BODY_STATUSES.includes(status);
  if (status < 200 || status > 599 || (noBody && body.length > 0)) return null;
  const out = new Headers();
  let contentLength = null;
  for (const [rawName, value] of headers) {
    const name = rawName.toLowerCase();
    if (isUnsafeResponseHeader(name) || !HEADER_VALUE.test(value)) return null;
    if (name === "content-length") {
      const trimmed = value.trim();
      if (!/^\d+$/.test(trimmed)) return null;
      const declared = Number(trimmed);
      if (
        !contentLengthFits(status, head, declared, body.length) ||
        (contentLength !== null && contentLength !== declared)
      ) {
        return null;
      }
      contentLength = declared;
    }
    try {
      out.append(name, value);
    } catch {
      return null;
    }
  }
  return new Response(noBody ? null : body, { status, headers: out });
}

/// True when the path has no `.`/`..` segment (also as `%2e`) and no `\`.
export function isSafePath(target) {
  const path = target.split("?")[0];
  return (
    path.startsWith("/") &&
    !path.includes("\\") &&
    path.split("/").every((segment) => {
      const decoded = segment.toLowerCase().replaceAll("%2e", ".");
      return decoded !== "." && decoded !== "..";
    })
  );
}

function isUpgrade(headers) {
  return (
    headers.has("upgrade") &&
    (headers.get("connection") || "")
      .split(",")
      .some((token) => token.trim().toLowerCase() === "upgrade")
  );
}

/// The headers both lanes get. Hop-by-hop headers (and those `connection`
/// names) are removed; an upgrade keeps its pair. The gateway is the first
/// proxy: inbound `forwarded`, `x-real-ip` and `x-forwarded-*` are dropped
/// and set from what this request actually is.
function laneHeaders(request, url, clientIp) {
  const incoming = request.headers;
  const upgrade = isUpgrade(incoming) ? incoming.get("upgrade") : null;
  const named = (incoming.get("connection") || "")
    .split(",")
    .map((token) => token.trim().toLowerCase())
    .filter((token) => token && token !== "host");
  const drop = new Set([
    ...HOP_BY_HOP,
    ...named,
    "forwarded",
    "x-real-ip",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
  ]);
  const headers = new Headers();
  for (const [name, value] of incoming) {
    if (!drop.has(name)) headers.append(name, value);
  }
  if (upgrade) {
    headers.set("connection", "upgrade");
    headers.set("upgrade", upgrade);
  }
  headers.set("x-forwarded-proto", url.protocol.replace(/:$/, ""));
  headers.set("x-forwarded-host", incoming.get("host") || url.host);
  if (clientIp) headers.set("x-forwarded-for", clientIp);
  return { headers, upgrade };
}

function plain(status, text) {
  return new Response(text, { status, headers: { "content-type": "text/plain; charset=utf-8" } });
}

/// A capsule in front of an origin, over the Fetch API.
///
/// Options:
///   capsule          a `WebAssembly.Module`, bytes, or a promise of either
///   origin           a base URL string (`https://origin.example.com`), or a
///                    function `(request) => Response | Promise<Response>`
///                    for an origin in the same process
///   kv               optional KV source: `(key) => value`, a `Map`, or a
///                    Workers KV binding. Provides the `kv` capability.
///   preload          KV keys to read before the first run
///   identity         optional `(request) => claims | null` resolving
///                    `{ user_id, roles }` after authoritative verification.
///                    A throw falls through to the origin.
///   responseHeaders  `[[name, value]]` set on every edge response
///   probeOrigin      copy the origin's static security headers (and a
///                    nonce-free CSP) from two `GET probePath`s, once
///   probePath        defaults to `/`
///   clientIp         `(request) => string | null` for `x-forwarded-for`
///                    (on Workers: `r => r.headers.get("cf-connecting-ip")`)
///   onLane           `({ lane, reason, request, response }) => void`
export function createEdgeGateway(options) {
  return new EdgeGateway(options);
}

export class EdgeGateway {
  constructor({
    capsule,
    origin,
    kv = null,
    preload = [],
    identity = null,
    responseHeaders = [],
    probeOrigin = false,
    probePath = "/",
    clientIp = null,
    onLane = null,
  }) {
    if (!capsule) throw new TypeError("createEdgeGateway needs a `capsule`");
    if (typeof origin !== "function" && typeof origin !== "string") {
      throw new TypeError("createEdgeGateway needs an `origin` URL or function");
    }
    if (!probePath.startsWith("/")) throw new Error(`probePath \`${probePath}\` must start with \`/\``);
    for (const [name] of responseHeaders) checkResponseHeader(name);
    this.capsule = Capsule.load(capsule);
    this.origin = typeof origin === "string" ? origin.replace(/\/+$/, "") : origin;
    this.kv = kv;
    this.preload = preload;
    this.identity = identity;
    this.responseHeaders = responseHeaders.map(([n, v]) => [String(n).toLowerCase(), String(v)]);
    this.probe = probeOrigin && typeof origin === "string" ? null : Promise.resolve([]);
    this.probePath = probePath;
    this.clientIp = clientIp;
    this.onLane = onLane;
  }

  /// The `Response` for `request`.
  async fetch(request) {
    return (await this.handle(request)).response;
  }

  /// `{ response, lane, reason }`. `lane` is `"edge"`, `"fallthrough"` (with
  /// the capsule's `reason`), `"origin_only"` (the capsule was not asked), or
  /// `"refused"` (a 400 for a path the gateway will not forward).
  async handle(request) {
    const result = await this.route(request);
    if (this.onLane) {
      try {
        this.onLane({ ...result, request });
      } catch {
        // a logging hook never changes the answer
      }
    }
    return result;
  }

  async route(request) {
    const url = new URL(request.url);
    const target = url.pathname + url.search;
    if (!isSafePath(target)) {
      return { response: plain(400, "bad request\n"), lane: "refused", reason: null };
    }
    const { headers, upgrade } = laneHeaders(
      request,
      url,
      this.clientIp ? this.clientIp(request) : null,
    );
    const method = request.method.toUpperCase();

    if (upgrade) return this.toOrigin(request, target, headers, "origin_only", null);
    if (method !== "GET" && method !== "HEAD") {
      return this.toOrigin(request, target, headers, "fallthrough", "method_not_edge_eligible");
    }
    const pairs = [...headers];
    // A non-ASCII value may be bytes the Fetch API decoded as Latin-1: it
    // cannot cross the wire as the UTF-8 the capsule expects, so only the
    // origin answers it — the gateway's rule for a non-UTF-8 value.
    if (pairs.some(([, value]) => /[^\x00-\x7f]/.test(value))) {
      return this.toOrigin(request, target, headers, "origin_only", null);
    }

    let claims = null;
    if (this.identity) {
      try {
        claims = (await this.identity(request)) || null;
      } catch {
        return this.toOrigin(request, target, headers, "fallthrough", "missing_capability");
      }
    }

    const capsule = await this.capsule;
    const outcome = await capsule.run(
      { method, uri: target, headers: pairs, identity: claims },
      { kv: this.kv, preload: this.preload },
    );
    if (outcome.kind === "served") {
      const response = intoResponse(outcome.response, method === "HEAD");
      if (response) {
        // They replace a capsule header of the same name; a name given more
        // than once keeps every value.
        const extra = await this.staticHeaders();
        for (const [name] of extra) response.headers.delete(name);
        for (const [name, value] of extra) response.headers.append(name, value);
        return { response, lane: "edge", reason: null };
      }
      return this.toOrigin(request, target, headers, "fallthrough", "capsule_error");
    }
    return this.toOrigin(request, target, headers, "fallthrough", outcome.reason);
  }

  /// Probed headers first, then `responseHeaders`, which replace a probed
  /// header of the same name.
  async staticHeaders() {
    if (!this.probe) {
      this.probe = probeStaticHeaders(this.origin, this.probePath).catch((err) => {
        this.probe = null; // try again on the next request
        console.warn(`[autumn-edge] origin header probe failed: ${messageOf(err)}`);
        return [];
      });
    }
    const probed = await this.probe;
    const explicit = new Set(this.responseHeaders.map(([name]) => name));
    return [...probed.filter(([name]) => !explicit.has(name)), ...this.responseHeaders];
  }

  async toOrigin(request, target, headers, lane, reason) {
    if (typeof this.origin === "function") {
      const forwarded = new Request(request, { headers });
      return { response: await this.origin(forwarded), lane, reason };
    }
    headers.delete("host");
    const init = { method: request.method, headers, redirect: "manual" };
    if (request.body && request.method !== "GET" && request.method !== "HEAD") {
      init.body = request.body;
      init.duplex = "half";
    }
    let answer;
    try {
      answer = await fetch(this.origin + target, init);
    } catch {
      return { response: plain(502, "bad gateway\n"), lane, reason };
    }
    if (answer.status === 101) return { response: answer, lane, reason };
    const out = new Headers(answer.headers);
    const named = (out.get("connection") || "").split(",").map((t) => t.trim().toLowerCase());
    for (const name of [...HOP_BY_HOP, ...named]) if (name && name !== "host") out.delete(name);
    return {
      response: new Response(answer.body, {
        status: answer.status,
        statusText: answer.statusText,
        headers: out,
      }),
      lane,
      reason,
    };
  }
}

/// The origin's static security headers: each of `SECURITY_HEADERS` and
/// `content-security-policy` that two `GET`s return with the same values.
/// A CSP nonce changes, so it is not copied. CORS is never copied.
export async function probeStaticHeaders(origin, path = "/") {
  const get = async () => (await fetch(origin + path, { redirect: "manual" })).headers;
  const first = await get();
  const second = await get();
  const out = [];
  for (const name of [...SECURITY_HEADERS, "content-security-policy"]) {
    const value = first.get(name);
    if (value !== null && value === second.get(name)) out.push([name, value]);
  }
  return out;
}
