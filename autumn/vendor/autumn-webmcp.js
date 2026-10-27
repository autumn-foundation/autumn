// Autumn WebMCP shim (issue #2619). Tracks the WebMCP CG draft of 2026-04-23.
// The draft is not a standard. This file changes when the draft changes.
//
// It registers two kinds of in-page tools with document.modelContext (or
// navigator.modelContext in older builds):
//   * Route tools from {base}/_autumn/webmcp/tools.json. `execute` sends a
//     same-origin fetch with the session cookie and the CSRF token. The real
//     route pipeline runs the call.
//   * Form tools: each <form> that holds a [data-webmcp-tool] marker.
//     `execute` fills the fields and submits the form. An htmx swap
//     unregisters the form tools of the content that it removes.
//
// With no modelContext it does nothing. It never writes to the console.
// Add it once to the page head with `autumn_web::webmcp::head_tags`.
(function () {
  "use strict";

  var mc = document.modelContext || navigator.modelContext;
  if (!mc || typeof mc.registerTool !== "function") return;

  var TOOLS_PATH = "/_autumn/webmcp/tools.json";
  var NAME = /^[A-Za-z0-9_.-]{1,128}$/;
  var MAX_DEPTH = 16;
  var FORM_TIMEOUT_MS = 30000;
  var MAX_TEXT = 8000;

  // The app base is the script URL up to /static/js/, so an app below a
  // path prefix keeps the prefix. A script from another origin uses the
  // page origin: the CSRF token must not go to another origin.
  var base = location.origin;
  try {
    var src = document.currentScript && document.currentScript.src;
    var cut = src ? src.lastIndexOf("/static/js/") : -1;
    if (cut >= 0 && new URL(src).origin === location.origin) base = src.slice(0, cut);
  } catch (_) {}
  var csrfHeader = "x-csrf-token";

  function result(text, isError) {
    return { content: [{ type: "text", text: String(text) }], isError: !!isError };
  }

  function fail(message) {
    var e = new Error(message);
    e.webmcp = true;
    throw e;
  }

  function csrf() {
    var meta = document.querySelector('meta[name="csrf-token"], meta[name="autumn-csrf-token"]');
    if (!meta) return null;
    return {
      header: meta.getAttribute("data-header") || csrfHeader,
      token: meta.getAttribute("content") || "",
    };
  }

  // Register `tool`. `onFail` runs when the browser refuses it (for example,
  // a duplicate name). Abort `signal` to unregister it.
  function register(tool, signal, onFail) {
    var done;
    try {
      done = mc.registerTool(tool, { signal: signal });
    } catch (_) {
      if (onFail) onFail();
      return;
    }
    if (done && typeof done.then === "function") {
      done.then(null, function () {
        if (onFail) onFail();
      });
    }
    signal.addEventListener("abort", function () {
      try {
        // The draft unregisters on abort. Older builds return a handle, or
        // need unregisterTool(name).
        if (done && typeof done.unregister === "function") done.unregister();
        else if (done === undefined && typeof mc.unregisterTool === "function") mc.unregisterTool(tool.name);
      } catch (_) {}
    });
  }

  // Ask the user before a consequential call. Use the agent's
  // requestUserInteraction() when it has one, else confirm(). If the script
  // cannot ask, the answer is no.
  async function approve(title, options) {
    var ask = function () {
      try {
        return typeof window.confirm === "function" &&
          window.confirm("Allow the AI agent to run “" + title + "”?") === true;
      } catch (_) {
        return false;
      }
    };
    try {
      if (options && typeof options.requestUserInteraction === "function") {
        return (await options.requestUserInteraction(async function () {
          return ask();
        })) === true;
      }
      return ask();
    } catch (_) {
      return false;
    }
  }

  function signalOf(options) {
    return options && options.signal && typeof options.signal.addEventListener === "function"
      ? options.signal
      : undefined;
  }

  // ── Route tools ──────────────────────────────────────────────────────────

  function segment(name, value, catchAll) {
    if (typeof value === "number" || typeof value === "boolean") value = String(value);
    if (typeof value !== "string") fail("path parameter `" + name + "` must be a string");
    var parts = catchAll ? value.split("/") : [value];
    return parts
      .map(function (part) {
        // The URL parser removes a dot segment. The call would then go to
        // another route.
        if (part === "" || part === "." || part === "..") {
          fail("path parameter `" + name + "` has an empty or dot segment");
        }
        return encodeURIComponent(part);
      })
      .join("/");
  }

  function fillPath(template, params, input) {
    var path = template;
    params.forEach(function (raw) {
      var catchAll = raw.charAt(0) === "*";
      var name = catchAll ? raw.slice(1) : raw;
      if (!Object.prototype.hasOwnProperty.call(input, name)) {
        fail("missing required path parameter `" + name + "`");
      }
      var value = segment(name, input[name], catchAll);
      path = path.replace(/\{([^}]*)\}/g, function (all, inner) {
        return inner.split(":")[0].trim() === raw ? value : all;
      });
    });
    return path;
  }

  function scalar(value) {
    return typeof value === "string" ? value : JSON.stringify(value);
  }

  function isContainer(value) {
    return value !== null && typeof value === "object";
  }

  function nameOk(name) {
    return name !== "" && name.indexOf("[") < 0 && name.indexOf("]") < 0;
  }

  // Encode one argument in the Query<T> format. A scalar is one pair. A
  // scalar array repeats the key; one element gets `[0]`. A container
  // element gets brackets. A null field gives no pair. A value that a query
  // string cannot hold is an error.
  function encodeArg(key, value, depth, out) {
    if (depth > MAX_DEPTH) fail("query argument `" + key + "` nests too deep");
    if (value === null || value === undefined) return;
    var before = out.length;
    if (Array.isArray(value)) {
      if (!value.length) fail("query argument `" + key + "` is an empty array");
      value.forEach(function (item, i) {
        if (item === null || item === undefined) fail("query argument `" + key + "[" + i + "]` is null");
      });
      if (!value.some(isContainer)) {
        if (value.length === 1) out.push([key + "[0]", scalar(value[0])]);
        else value.forEach(function (item) { out.push([key, scalar(item)]); });
      } else {
        value.forEach(function (item, i) { encodeArg(key + "[" + i + "]", item, depth + 1, out); });
      }
    } else if (isContainer(value)) {
      var keys = Object.keys(value);
      if (!keys.length) fail("query argument `" + key + "` is an empty object");
      var emitting = keys.filter(function (k) { return value[k] !== null && value[k] !== undefined; });
      if (emitting.length && emitting.every(function (k) { return /^[0-9]+$/.test(k); })) {
        fail("query argument `" + key + "` has only numeric field names");
      }
      keys.forEach(function (k) {
        if (!nameOk(k)) fail("query argument `" + key + "` has a field name a query string cannot hold");
        encodeArg(key + "[" + k + "]", value[k], depth + 1, out);
      });
    } else {
      out.push([key, scalar(value)]);
      return;
    }
    if (out.length === before) fail("query argument `" + key + "` has no value");
  }

  function encodeQuery(query) {
    if (!isContainer(query) || Array.isArray(query)) fail("`query` must be a JSON object");
    var out = [];
    Object.keys(query).forEach(function (k) {
      if (!nameOk(k)) fail("query argument `" + k + "` has a name a query string cannot hold");
      encodeArg(k, query[k], 1, out);
    });
    try {
      return out
        .map(function (pair) { return encodeURIComponent(pair[0]) + "=" + encodeURIComponent(pair[1]); })
        .join("&");
    } catch (_) {
      return fail("a query argument is not valid text");
    }
  }

  function request(tool, input, signal) {
    var path = fillPath(tool.path_template, tool.path_params || [], input);
    if (tool.has_query && input.query !== undefined) {
      var qs = encodeQuery(input.query);
      if (qs) path += "?" + qs;
    }
    var headers = { accept: "application/json" };
    // `manual`: a redirect is an error result. So the CSRF header never
    // follows a redirect to another origin.
    var init = { method: tool.method, credentials: "same-origin", redirect: "manual", headers: headers };
    if (signal) init.signal = signal;
    if (tool.has_body) {
      if (!Object.prototype.hasOwnProperty.call(input, "body")) fail("missing required `body` argument");
      headers["content-type"] = "application/json";
      init.body = JSON.stringify(input.body);
    }
    var token = csrf();
    if (token) headers[token.header] = token.token;
    return { url: base + path, init: init };
  }

  function routeTool(tool, signal) {
    var title = (tool.annotations && tool.annotations.title) || tool.name;
    var hints = tool.webmcp_annotations || {};
    register(
      {
        name: tool.name,
        title: title,
        description: tool.description || title,
        inputSchema: tool.inputSchema || { type: "object", properties: {} },
        annotations: { readOnlyHint: !!hints.readOnlyHint, consequentialHint: !!hints.consequentialHint },
        execute: async function (input, options) {
          var req;
          try {
            req = request(tool, isContainer(input) ? input : {}, signalOf(options));
          } catch (e) {
            if (e && e.webmcp) return result(e.message, true);
            throw e;
          }
          if (tool.confirm && !(await approve(title, options))) {
            return result("The user did not allow this call. Nothing was sent.", true);
          }
          var res = await fetch(req.url, req.init);
          if (res.type === "opaqueredirect") {
            return result("The route sent a redirect. The script does not follow it.", true);
          }
          var text = await res.text();
          return result(text || "HTTP " + res.status, !res.ok);
        },
      },
      signal,
    );
  }

  function loadRouteTools() {
    var signal = new AbortController().signal;
    fetch(base + TOOLS_PATH, { credentials: "same-origin", headers: { accept: "application/json" } })
      .then(function (res) { return res.ok ? res.json() : null; })
      .then(function (manifest) {
        if (!manifest || !Array.isArray(manifest.tools)) return;
        if (typeof manifest.csrfHeader === "string") csrfHeader = manifest.csrfHeader;
        manifest.tools.forEach(function (tool) {
          if (tool && NAME.test(tool.name) && typeof tool.path_template === "string") routeTool(tool, signal);
        });
      })
      .then(null, function () {});
  }

  // ── Form tools ───────────────────────────────────────────────────────────

  var SKIP = { hidden: 1, submit: 1, button: 1, reset: 1, image: 1, file: 1, password: 1 };
  var VERBS = ["get", "post", "put", "patch", "delete"];
  // form -> { controller, name }
  var forms = new Map();
  // The forms with a call in progress.
  var busy = new WeakSet();

  // The fields the agent can fill. The browser does not submit a disabled
  // field. The user cannot change a read-only field, so the agent cannot.
  function fields(form) {
    return Array.prototype.filter.call(form.elements, function (el) {
      var type = (el.type || "").toLowerCase();
      return el.name && !SKIP[type] && /^(INPUT|SELECT|TEXTAREA)$/.test(el.tagName) &&
        !el.matches(":disabled") && !el.readOnly;
    });
  }

  function text(node) {
    var copy = node.cloneNode(true);
    Array.prototype.forEach.call(copy.querySelectorAll("select,script,style,template"), function (n) {
      n.remove();
    });
    return (copy.textContent || "").replace(/\s+/g, " ").trim();
  }

  function label(el) {
    var type = (el.type || "").toLowerCase();
    var legend = (type === "radio" || type === "checkbox") && el.closest("fieldset");
    legend = legend && legend.querySelector("legend");
    if (legend) return text(legend);
    return (el.labels && el.labels[0] && text(el.labels[0])) ||
      el.getAttribute("aria-label") || el.getAttribute("placeholder") || "";
  }

  function formSchema(form) {
    var props = {};
    var required = [];
    var checkboxes = {};
    fields(form).forEach(function (el) {
      if ((el.type || "").toLowerCase() === "checkbox") {
        (checkboxes[el.name] = checkboxes[el.name] || []).push(el.value);
      }
    });
    fields(form).forEach(function (el) {
      var type = (el.type || "").toLowerCase();
      var name = el.name;
      var prop;
      if (type === "radio") {
        prop = props[name] || { type: "string", enum: [] };
        prop.enum.push(el.value);
      } else if (type === "checkbox") {
        prop = checkboxes[name].length > 1
          ? { type: "array", items: { type: "string", enum: checkboxes[name] } }
          : { type: "boolean" };
      } else if (el.tagName === "SELECT") {
        var values = Array.prototype.map.call(el.options, function (o) { return o.value; });
        prop = el.multiple
          ? { type: "array", items: { type: "string", enum: values } }
          : { type: "string", enum: values };
      } else if (type === "number" || type === "range") {
        prop = { type: "number" };
        if (el.min !== "") prop.minimum = Number(el.min);
        if (el.max !== "") prop.maximum = Number(el.max);
      } else {
        prop = { type: "string" };
        if (el.minLength > 0) prop.minLength = el.minLength;
        if (el.maxLength >= 0) prop.maxLength = el.maxLength;
        if (el.pattern) prop.pattern = "^(?:" + el.pattern + ")$";
        if (type === "email") prop.format = "email";
        if (type === "url") prop.format = "uri";
        if (type === "date") prop.format = "date";
      }
      var desc = label(el);
      if (desc && !prop.description) prop.description = desc;
      props[name] = prop;
      if (el.required && required.indexOf(name) < 0) required.push(name);
    });
    var schema = { type: "object", properties: props, additionalProperties: false };
    if (required.length) schema.required = required;
    return schema;
  }

  // Fill the fields from `input`. A value that a field cannot hold is an
  // error: the form must not send without the value of the agent.
  function fill(form, input) {
    var all = fields(form);
    var known = {};
    all.forEach(function (el) { known[el.name] = true; });
    Object.keys(input).forEach(function (name) {
      if (!known[name]) fail("the form has no field `" + name + "`");
    });
    var matched = {};
    all.forEach(function (el) {
      if (!Object.prototype.hasOwnProperty.call(input, el.name)) return;
      var value = input[el.name];
      var type = (el.type || "").toLowerCase();
      if (type === "checkbox" || (el.tagName === "SELECT" && el.multiple)) {
        var many = type === "checkbox" && !Array.isArray(value) ? null : (Array.isArray(value) ? value : [value]).map(String);
        if (type === "checkbox") {
          el.checked = many ? many.indexOf(el.value) >= 0 : value === true;
          if (many) many.forEach(function (v) { if (v === el.value) matched[el.name + "\u0000" + v] = true; });
        } else {
          Array.prototype.forEach.call(el.options, function (o) {
            o.selected = many.indexOf(o.value) >= 0;
            if (o.selected) matched[el.name + "\u0000" + o.value] = true;
          });
          many.forEach(function (v) {
            if (!matched[el.name + "\u0000" + v]) fail("field `" + el.name + "` has no option `" + v + "`");
          });
        }
      } else if (type === "radio") {
        el.checked = String(value) === el.value;
        if (el.checked) matched[el.name] = true;
      } else {
        var want = value === null || value === undefined ? "" : String(value);
        el.value = want;
        if (el.value !== want) fail("field `" + el.name + "` cannot hold the value `" + want + "`");
      }
    });
    all.forEach(function (el) {
      var type = (el.type || "").toLowerCase();
      if (!Object.prototype.hasOwnProperty.call(input, el.name)) return;
      if (type === "radio" && !matched[el.name]) fail("field `" + el.name + "` has no choice `" + input[el.name] + "`");
      if (type === "checkbox" && Array.isArray(input[el.name])) {
        input[el.name].map(String).forEach(function (v) {
          if (!matched[el.name + "\u0000" + v]) fail("field `" + el.name + "` has no choice `" + v + "`");
        });
      }
    });
    all.forEach(function (el) {
      if (!Object.prototype.hasOwnProperty.call(input, el.name)) return;
      el.dispatchEvent(new Event("input", { bubbles: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
    });
  }

  function htmxVerb(form) {
    for (var i = 0; i < VERBS.length; i++) {
      if (form.hasAttribute("hx-" + VERBS[i]) || form.hasAttribute("data-hx-" + VERBS[i])) return VERBS[i];
    }
    return null;
  }

  // An htmx form that sends on `submit` (the default trigger for a form).
  function isHtmxForm(form) {
    if (!window.htmx) return false;
    var trigger = form.getAttribute("hx-trigger") || form.getAttribute("data-hx-trigger");
    if (trigger && !/(^|[\s,])submit([\s,[]|$)/.test(trigger)) return false;
    if (htmxVerb(form)) return true;
    var boosted = form.closest("[hx-boost],[data-hx-boost]");
    var flag = boosted && (boosted.getAttribute("hx-boost") || boosted.getAttribute("data-hx-boost"));
    return flag === "true";
  }

  // The HTTP method the form sends: an hx-<verb> attribute, else the
  // `_method` override of a POST form, else the `method` attribute.
  function formMethod(form) {
    var verb = htmxVerb(form);
    if (verb) return verb.toUpperCase();
    var method = (form.getAttribute("method") || "get").trim().toUpperCase();
    var override = form.querySelector('input[name="_method"]');
    if (method === "POST" && override && override.value.trim()) return override.value.trim().toUpperCase();
    return method;
  }

  function responseText(html) {
    try {
      var doc = new DOMParser().parseFromString(html || "", "text/html");
      return text(doc.body).slice(0, MAX_TEXT);
    } catch (_) {
      return "";
    }
  }

  // Stop the htmx request of a form. htmx aborts the request of an element
  // that gets `htmx:abort`.
  function stopHtmx(form) {
    try {
      if (window.htmx && typeof window.htmx.trigger === "function") window.htmx.trigger(form, "htmx:abort");
      else form.dispatchEvent(new CustomEvent("htmx:abort"));
    } catch (_) {
      // Nothing more to do.
    }
  }

  // Submit through htmx. Wait for the result of this form's request.
  function submitHtmx(form, signal) {
    return new Promise(function (resolve) {
      var done = false;
      var timer;
      var events = ["htmx:afterRequest", "htmx:abort", "htmx:sendAbort", "htmx:validation:halted"];
      function finish(out) {
        if (done) return;
        done = true;
        clearTimeout(timer);
        events.forEach(function (type) {
          form.removeEventListener(type, onEvent);
          document.removeEventListener(type, onEvent);
        });
        if (signal) signal.removeEventListener("abort", onAbort);
        resolve(out);
      }
      // Finish first, so this call does not read its own abort event.
      function onAbort() {
        finish(result("The agent cancelled the call.", true));
        stopHtmx(form);
      }
      function onEvent(evt) {
        var d = evt.detail || {};
        if (d.elt !== form && evt.target !== form) return;
        if (evt.type !== "htmx:afterRequest") {
          finish(result("htmx did not send the form (" + evt.type + ").", true));
          return;
        }
        var xhr = d.xhr || {};
        // A redirect header (HX-Redirect, HX-Location) leaves `successful`
        // unset. Use the status then.
        var ok = d.successful === true ||
          (d.successful === undefined && !d.failed && xhr.status >= 200 && xhr.status < 400);
        finish(result(responseText(xhr.responseText) || "HTTP " + xhr.status, !ok));
      }
      events.forEach(function (type) {
        form.addEventListener(type, onEvent);
        document.addEventListener(type, onEvent);
      });
      if (signal) signal.addEventListener("abort", onAbort);
      timer = setTimeout(function () {
        finish(result("The form got no response in time.", true));
        stopHtmx(form);
      }, FORM_TIMEOUT_MS);
      form.requestSubmit();
    });
  }

  function formTool(form, marker) {
    var name = marker.getAttribute("data-webmcp-tool");
    if (!NAME.test(name || "")) return;
    var controller = new AbortController();
    forms.set(form, { controller: controller, name: name });
    var method = formMethod(form);
    var readOnly = method === "GET";
    var confirm = marker.hasAttribute("data-webmcp-confirm") || method === "DELETE";
    var title = marker.getAttribute("data-webmcp-description") || name;
    register(
      {
        name: name,
        title: title,
        description: title,
        inputSchema: formSchema(form),
        annotations: { readOnlyHint: readOnly, consequentialHint: !readOnly || confirm },
        execute: async function (input, options) {
          if (!form.isConnected) return result("The form is no longer on the page.", true);
          // One call at a time: a second call would refill the form and
          // read the response of the first.
          if (busy.has(form)) return result("The form is busy with another call. Try again when it ends.", true);
          busy.add(form);
          try {
            var signal = signalOf(options);
            // Ask first: filling a field can start an htmx request.
            if (confirm && !(await approve(title, options))) {
              return result("The user did not allow this call. Nothing was sent.", true);
            }
            if (signal && signal.aborted) return result("The agent cancelled the call. Nothing was sent.", true);
            try {
              fill(form, isContainer(input) ? input : {});
            } catch (e) {
              if (e && e.webmcp) return result(e.message, true);
              throw e;
            }
            if (!form.checkValidity()) {
              var bad = fields(form)
                .filter(function (el) { return !el.validity.valid; })
                .map(function (el) { return el.name + ": " + el.validationMessage; });
              return result("The form is not valid. " + bad.join("; "), true);
            }
            if (isHtmxForm(form)) return await submitHtmx(form, signal);
            form.requestSubmit();
            return result("The form was submitted. The page loads the response.", false);
          } finally {
            busy.delete(form);
          }
        },
      },
      controller.signal,
      function () {
        // The browser refused the tool. Forget the form, so a later scan
        // can try again.
        var entry = forms.get(form);
        if (entry && entry.controller === controller) forms.delete(form);
      },
    );
  }

  function drop(form) {
    var entry = forms.get(form);
    if (!entry) return;
    entry.controller.abort();
    forms.delete(form);
  }

  // Drop the tools of forms that left the page or lost their marker, then
  // add the new forms.
  function scanForms() {
    forms.forEach(function (entry, form) {
      var marker = form.isConnected && form.querySelector("[data-webmcp-tool]");
      if (!marker || marker.getAttribute("data-webmcp-tool") !== entry.name) drop(form);
    });
    Array.prototype.forEach.call(document.querySelectorAll("[data-webmcp-tool]"), function (marker) {
      var form = marker.closest("form");
      if (form && !forms.has(form)) formTool(form, marker);
    });
  }

  // A swap changes its target. Unregister the form tools in the target, and
  // the tool of a form that holds the target: its fields can change.
  function beforeSwap(evt) {
    var d = evt.detail || {};
    var target = d.target;
    if (d.shouldSwap === false || !target || typeof target.contains !== "function") return;
    forms.forEach(function (_, form) {
      if (target.contains(form) || form.contains(target)) drop(form);
    });
  }

  document.addEventListener("htmx:beforeSwap", beforeSwap);
  document.addEventListener("htmx:oobBeforeSwap", beforeSwap);
  ["htmx:afterSettle", "htmx:afterRequest", "htmx:historyRestore"].forEach(function (type) {
    document.addEventListener(type, scanForms);
  });

  loadRouteTools();
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", scanForms);
  else scanForms();
})();
