// Behaviour tests for the WebMCP shim (issue #2619). No dependencies.
//
//   node shim_harness.js <path to autumn-webmcp.js>
//
// Each case runs the shim in a fresh `vm` context with a fake page. The
// harness prints one line per case and exits 1 when a case fails.
"use strict";

const fs = require("fs");
const vm = require("vm");
const assert = require("assert");

const SHIM = fs.readFileSync(process.argv[2], "utf8");
const ORIGIN = "https://app.test";
const SCRIPT_SRC = ORIGIN + "/shop/static/js/autumn-webmcp.0123abcd.js";

const MANIFEST = {
  version: 1,
  csrfHeader: "x-csrf-token",
  tools: [
    {
      name: "list_items",
      description: "List items",
      inputSchema: { type: "object", properties: { query: { type: "object" } } },
      annotations: { title: "List items", readOnlyHint: true },
      webmcp_annotations: { readOnlyHint: true, consequentialHint: false },
      method: "GET",
      path_template: "/items",
      path_params: [],
      has_query: true,
      has_body: false,
      confirm: false,
    },
    {
      name: "add_item",
      description: "Add an item",
      inputSchema: { type: "object", properties: { body: { type: "object" } }, required: ["body"] },
      annotations: { title: "Add an item", readOnlyHint: false },
      webmcp_annotations: { readOnlyHint: false, consequentialHint: true },
      method: "POST",
      path_template: "/lists/{list}/items",
      path_params: ["list"],
      has_query: false,
      has_body: true,
      confirm: false,
    },
    {
      name: "delete_item",
      description: "Delete an item",
      inputSchema: { type: "object", properties: { id: { type: "string" } }, required: ["id"] },
      annotations: { title: "Delete an item", readOnlyHint: false, destructiveHint: true },
      webmcp_annotations: { readOnlyHint: false, consequentialHint: true },
      method: "DELETE",
      path_template: "/items/{id}",
      path_params: ["id"],
      has_query: false,
      has_body: false,
      confirm: true,
    },
    {
      name: "read_file",
      description: "Read a file",
      inputSchema: { type: "object", properties: { rest: { type: "string" } } },
      annotations: { title: "Read a file", readOnlyHint: true },
      webmcp_annotations: { readOnlyHint: true, consequentialHint: false },
      method: "GET",
      path_template: "/files/{*rest}",
      path_params: ["*rest"],
      has_query: false,
      has_body: false,
      confirm: false,
    },
    { name: "bad name!", description: "x", method: "GET", path_template: "/x", path_params: [] },
  ],
};

function response(status, body) {
  return {
    ok: status >= 200 && status < 300,
    status,
    text: async () => (typeof body === "string" ? body : JSON.stringify(body)),
    json: async () => (typeof body === "string" ? JSON.parse(body) : body),
  };
}

// A fake page. `opts` picks which pieces exist.
function page(opts = {}) {
  const calls = [];
  const registered = [];
  const consoleCalls = [];
  const listeners = {};
  const modelContext = {
    registerTool(tool, options) {
      if (opts.rejectDuplicates && registered.some((r) => r.tool.name === tool.name)) {
        return Promise.reject(new Error("InvalidStateError"));
      }
      registered.push({ tool, options });
      return Promise.resolve();
    },
  };
  const meta = opts.token
    ? {
        getAttribute: (n) => (n === "content" ? opts.token : n === "data-header" ? opts.dataHeader || null : null),
        content: opts.token,
      }
    : null;
  const document = {
    readyState: "complete",
    currentScript: { src: opts.scriptSrc || SCRIPT_SRC },
    querySelector: (sel) => (sel.includes("csrf-token") ? meta : null),
    querySelectorAll: () => [],
    addEventListener: (type, fn) => {
      (listeners[type] = listeners[type] || []).push(fn);
    },
  };
  if (opts.documentContext !== false) document.modelContext = modelContext;
  const navigator = {};
  if (opts.navigatorContext) navigator.modelContext = opts.navigatorContext;
  const sandbox = {
    document,
    navigator,
    location: { origin: ORIGIN, href: ORIGIN + "/shop/" },
    URL,
    AbortController,
    setTimeout,
    clearTimeout,
    Promise,
    JSON,
    console: new Proxy({}, { get: (_, k) => (...a) => consoleCalls.push([k, a]) }),
    fetch: async (url, init) => {
      calls.push({ url: String(url), init: init || {} });
      if (String(url).endsWith("/_autumn/webmcp/tools.json")) {
        return opts.manifestStatus ? response(opts.manifestStatus, "nope") : response(200, opts.manifest || MANIFEST);
      }
      if (opts.redirectResponse) return { ok: false, status: 0, type: "opaqueredirect", text: async () => "" };
      return response(opts.status || 200, opts.body === undefined ? { ok: true } : opts.body);
    },
  };
  if (opts.confirm !== undefined) sandbox.confirm = opts.confirm;
  sandbox.window = sandbox;
  vm.createContext(sandbox);
  return { sandbox, calls, registered, consoleCalls, listeners, modelContext };
}

async function settle() {
  for (let i = 0; i < 5; i++) await new Promise((r) => setImmediate(r));
}

async function boot(opts) {
  const p = page(opts);
  vm.runInContext(SHIM, p.sandbox, { filename: "autumn-webmcp.js" });
  await settle();
  return p;
}

function find(p, name) {
  const hit = p.registered.find((r) => r.tool.name === name);
  assert.ok(hit, "tool " + name + " is not registered: " + p.registered.map((r) => r.tool.name));
  return hit;
}

function toolCalls(p) {
  return p.calls.filter((c) => !c.url.endsWith("/_autumn/webmcp/tools.json"));
}

const cases = {
  async "no modelContext: no fetch, no registration, no console output"() {
    const p = await boot({ documentContext: false });
    assert.strictEqual(p.calls.length, 0);
    assert.strictEqual(p.registered.length, 0);
    assert.deepStrictEqual(p.consoleCalls, []);
  },

  async "navigator.modelContext is the fallback"() {
    const registered = [];
    await boot({
      documentContext: false,
      navigatorContext: { registerTool: (t) => (registered.push(t), Promise.resolve()) },
    });
    assert.ok(registered.some((t) => t.name === "list_items"));
  },

  async "document.modelContext wins over navigator.modelContext"() {
    const other = [];
    const p = await boot({ navigatorContext: { registerTool: (t) => other.push(t) } });
    assert.strictEqual(other.length, 0);
    assert.ok(p.registered.length > 0);
  },

  async "the manifest is fetched below the app base path, same-origin"() {
    const p = await boot({});
    assert.strictEqual(p.calls[0].url, ORIGIN + "/shop/_autumn/webmcp/tools.json");
    assert.strictEqual(p.calls[0].init.credentials, "same-origin");
  },

  async "each valid tool is registered with its schema and annotations"() {
    const p = await boot({});
    const names = p.registered.map((r) => r.tool.name).sort();
    assert.deepStrictEqual(names, ["add_item", "delete_item", "list_items", "read_file"]);
    const add = find(p, "add_item");
    assert.deepStrictEqual(add.tool.inputSchema, MANIFEST.tools[1].inputSchema);
    assert.strictEqual(add.tool.description, "Add an item");
    assert.strictEqual(add.tool.title, "Add an item");
    // The object comes from the vm realm, so compare its JSON.
    assert.deepStrictEqual(JSON.parse(JSON.stringify(add.tool.annotations)), {
      readOnlyHint: false,
      consequentialHint: true,
    });
    assert.strictEqual(typeof add.tool.execute, "function");
    assert.ok(add.options && add.options.signal, "registered with an AbortSignal");
  },

  async "a missing manifest registers nothing and throws nothing"() {
    const p = await boot({ manifestStatus: 404 });
    assert.strictEqual(p.registered.length, 0);
    assert.deepStrictEqual(p.consoleCalls, []);
  },

  async "a rejected registration is swallowed"() {
    const dup = JSON.parse(JSON.stringify(MANIFEST));
    dup.tools.push(dup.tools[0]);
    let unhandled = null;
    const onUnhandled = (e) => (unhandled = e);
    process.on("unhandledRejection", onUnhandled);
    await boot({ manifest: dup, rejectDuplicates: true });
    await settle();
    process.off("unhandledRejection", onUnhandled);
    assert.strictEqual(unhandled, null);
  },

  async "execute sends a same-origin fetch with the CSRF header and a JSON body"() {
    const p = await boot({ token: "tok-1", body: { id: 7 } });
    const out = await find(p, "add_item").tool.execute({ list: "a b", body: { title: "milk" } }, {});
    const [call] = toolCalls(p);
    assert.strictEqual(call.url, ORIGIN + "/shop/lists/a%20b/items");
    assert.strictEqual(call.init.method, "POST");
    assert.strictEqual(call.init.credentials, "same-origin");
    assert.strictEqual(call.init.headers["x-csrf-token"], "tok-1");
    assert.strictEqual(call.init.headers["content-type"], "application/json");
    assert.strictEqual(call.init.body, JSON.stringify({ title: "milk" }));
    assert.strictEqual(out.isError, false);
    assert.strictEqual(out.content[0].type, "text");
    assert.strictEqual(out.content[0].text, JSON.stringify({ id: 7 }));
  },

  async "the meta data-header names the CSRF header"() {
    const p = await boot({ token: "tok-2", dataHeader: "X-Custom-Csrf" });
    await find(p, "add_item").tool.execute({ list: "a", body: {} }, {});
    assert.strictEqual(toolCalls(p)[0].init.headers["X-Custom-Csrf"], "tok-2");
  },

  async "a failed status is an error result"() {
    const p = await boot({ status: 401, body: "unauthorized" });
    const out = await find(p, "list_items").tool.execute({}, {});
    assert.strictEqual(out.isError, true);
    assert.ok(out.content[0].text.includes("unauthorized"), out.content[0].text);
  },

  async "the query object uses the Query<T> dialect"() {
    const p = await boot({});
    await find(p, "list_items").tool.execute(
      { query: { tags: ["a", "b"], one: ["x"], filter: { status: "open" }, skip: null, page: 2, on: true } },
      {},
    );
    const url = new URL(toolCalls(p)[0].url);
    assert.strictEqual(url.pathname, "/shop/items");
    assert.strictEqual(
      url.search,
      "?tags=a&tags=b&one%5B0%5D=x&filter%5Bstatus%5D=open&page=2&on=true",
    );
  },

  async "a query the dialect cannot carry is an error, not a call"() {
    const p = await boot({});
    for (const query of [{ e: [] }, { e: {} }, { e: [1, null] }, { "a[b]": 1 }, { n: { 0: 1, 1: 2 } }, "x", [1]]) {
      const out = await find(p, "list_items").tool.execute({ query }, {});
      assert.strictEqual(out.isError, true, JSON.stringify(query));
    }
    assert.strictEqual(toolCalls(p).length, 0);
  },

  async "a dot or empty path segment is refused"() {
    const p = await boot({ confirm: () => true });
    for (const id of ["..", ".", ""]) {
      const out = await find(p, "delete_item").tool.execute({ id }, {});
      assert.strictEqual(out.isError, true, id);
    }
    const missing = await find(p, "add_item").tool.execute({ body: {} }, {});
    assert.strictEqual(missing.isError, true);
    assert.strictEqual(toolCalls(p).length, 0);
  },

  async "a catch-all keeps its slashes and refuses dot segments"() {
    const p = await boot({});
    await find(p, "read_file").tool.execute({ rest: "a b/c.txt" }, {});
    assert.strictEqual(toolCalls(p)[0].url, ORIGIN + "/shop/files/a%20b/c.txt");
    const out = await find(p, "read_file").tool.execute({ rest: "a/../b" }, {});
    assert.strictEqual(out.isError, true);
    assert.strictEqual(toolCalls(p).length, 1);
  },

  async "a missing body is refused"() {
    const p = await boot({});
    const out = await find(p, "add_item").tool.execute({ list: "a" }, {});
    assert.strictEqual(out.isError, true);
    assert.strictEqual(toolCalls(p).length, 0);
  },

  async "a confirm tool asks through requestUserInteraction"() {
    const p = await boot({ confirm: () => true });
    let asked = 0;
    const declined = await find(p, "delete_item").tool.execute(
      { id: "7" },
      { requestUserInteraction: async (cb) => (asked++, false) },
    );
    assert.strictEqual(asked, 1);
    assert.strictEqual(declined.isError, true);
    assert.strictEqual(toolCalls(p).length, 0, "a declined call sends nothing");

    const approved = await find(p, "delete_item").tool.execute(
      { id: "7" },
      { requestUserInteraction: async (cb) => cb() },
    );
    assert.strictEqual(approved.isError, false);
    assert.strictEqual(toolCalls(p)[0].init.method, "DELETE");
    assert.strictEqual(toolCalls(p)[0].url, ORIGIN + "/shop/items/7");
  },

  async "without requestUserInteraction the page asks with confirm()"() {
    const p = await boot({ confirm: () => false });
    const out = await find(p, "delete_item").tool.execute({ id: "7" }, { signal: {} });
    assert.strictEqual(out.isError, true);
    assert.strictEqual(toolCalls(p).length, 0);
  },

  async "with no way to ask, a confirm tool fails closed"() {
    const p = await boot({});
    const out = await find(p, "delete_item").tool.execute({ id: "7" }, undefined);
    assert.strictEqual(out.isError, true);
    assert.strictEqual(toolCalls(p).length, 0);
  },

  async "a non-confirm tool never asks"() {
    const p = await boot({ confirm: () => assert.fail("asked") });
    const out = await find(p, "list_items").tool.execute({}, {});
    assert.strictEqual(out.isError, false);
  },

  async "a redirect is an error, so the CSRF header never follows it"() {
    const p = await boot({ token: "tok" });
    await find(p, "list_items").tool.execute({}, {});
    assert.strictEqual(toolCalls(p)[0].init.redirect, "manual");
    const r = await boot({ redirectResponse: true });
    const out = await find(r, "list_items").tool.execute({}, {});
    assert.strictEqual(out.isError, true);
  },

  async "the call passes the agent's abort signal to fetch"() {
    const p = await boot({});
    const signal = new AbortController().signal;
    await find(p, "list_items").tool.execute({}, { signal });
    assert.strictEqual(toolCalls(p)[0].init.signal, signal);
  },

  async "a script from another origin uses the page origin"() {
    const p = await boot({ scriptSrc: "https://cdn.example/static/js/autumn-webmcp.js" });
    assert.strictEqual(p.calls[0].url, ORIGIN + "/_autumn/webmcp/tools.json");
  },

  async "a script outside /static/js/ uses the page origin"() {
    const p = await boot({ scriptSrc: ORIGIN + "/vendor/webmcp.js" });
    assert.strictEqual(p.calls[0].url, ORIGIN + "/_autumn/webmcp/tools.json");
  },
};

(async () => {
  let failed = 0;
  for (const [name, run] of Object.entries(cases)) {
    try {
      await run();
      process.stdout.write("ok   " + name + "\n");
    } catch (e) {
      failed++;
      process.stdout.write("FAIL " + name + ": " + (e && e.stack ? e.stack : e) + "\n");
    }
  }
  process.stdout.write(failed ? failed + " failed\n" : "all passed\n");
  process.exit(failed ? 1 : 0);
})();
