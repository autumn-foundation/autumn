/*! htmx-ext-sse — SSE extension for htmx 2.x, vendored for Autumn.
 *  Faithfully implements the htmx SSE extension API:
 *    sse-connect="<url>"  — opens an EventSource on that element
 *    sse-swap="<event>"   — swaps element content on matching SSE events
 *    sse-close="<event>"  — closes the connection on matching SSE events
 *  OOB fragments (hx-swap-oob) embedded in SSE messages are processed
 *  automatically through htmx's normal swap pipeline.
 *
 *  Compatible with htmx 2.0.x.
 */
(function () {
  "use strict";

  /** @type {import("../htmx").HtmxInternalApi} */
  var api;

  htmx.defineExtension("sse", {
    init: function (apiRef) {
      api = apiRef;
      // Allow the application to override the EventSource factory, e.g.
      // to inject auth headers or use a polyfill.
      if (typeof htmx.createEventSource === "undefined") {
        htmx.createEventSource = function (url) {
          return new EventSource(url, { withCredentials: true });
        };
      }
    },

    onEvent: function (name, evt) {
      var elt = evt.target || (evt.detail && evt.detail.elt);
      if (!elt || typeof elt.getAttribute !== "function") return;

      switch (name) {
        // Clean up when the element leaves the DOM.
        case "htmx:beforeCleanupElement": {
          var data = api.getInternalData(elt);
          if (data.sseEventSource) {
            data.sseEventSource.close();
            delete data.sseEventSource;
          }
          break;
        }

        // Wire SSE connections after htmx has processed an element.
        case "htmx:afterProcessNode":
          ensureEventSource(elt);
          api.findAll(elt, "[sse-swap], [sse-close], [sse-connect]").forEach(function (child) {
            ensureEventSource(child);
          });
          break;
      }
    },
  });

  // ── helpers ────────────────────────────────────────────────────────────────

  /** Open an EventSource on `elt` if it has `sse-connect` and none yet. */
  function ensureEventSource(elt) {
    var url = api.getAttributeValue(elt, "sse-connect");
    if (url) {
      var d = api.getInternalData(elt);
      if (!d.sseEventSource) {
        openEventSource(elt, url);
      }
    }

    // Bind sse-swap / sse-close to the nearest ancestor's source.
    var sseSwap = api.getAttributeValue(elt, "sse-swap");
    var sseClose = api.getAttributeValue(elt, "sse-close");
    if (sseSwap || sseClose) {
      var source = findSourceForElement(elt);
      if (source) {
        if (sseSwap) bindSwap(elt, sseSwap, source);
        if (sseClose) bindClose(elt, sseClose, source);
      }
    }
  }

  /** Walk up the tree and return the nearest EventSource. */
  function findSourceForElement(elt) {
    var cur = elt;
    while (cur) {
      var src = api.getInternalData(cur).sseEventSource;
      if (src) return src;
      cur = cur.parentElement;
    }
    return null;
  }

  /** Open a new EventSource, store it, and attach swap / close listeners. */
  function openEventSource(elt, url) {
    var source = htmx.createEventSource(url);
    var d = api.getInternalData(elt);
    d.sseEventSource = source;

    source.onerror = function (e) {
      api.triggerErrorEvent(elt, "htmx:sseError", { error: e, source: source });
      if (!api.bodyContains(elt)) {
        source.close();
        delete api.getInternalData(elt).sseEventSource;
      }
    };

    source.onopen = function () {
      api.triggerEvent(elt, "htmx:sseOpen", { source: source });
    };

    // Bind swap/close on this element itself and on already-present children.
    var sseSwap = api.getAttributeValue(elt, "sse-swap");
    if (sseSwap) bindSwap(elt, sseSwap, source);

    var sseClose = api.getAttributeValue(elt, "sse-close");
    if (sseClose) bindClose(elt, sseClose, source);

    api.findAll(elt, "[sse-swap]").forEach(function (child) {
      var s = api.getAttributeValue(child, "sse-swap");
      if (s) bindSwap(child, s, source);
    });

    api.findAll(elt, "[sse-close]").forEach(function (child) {
      var c = api.getAttributeValue(child, "sse-close");
      if (c) bindClose(child, c, source);
    });
  }

  /**
   * Bind SSE event → htmx swap.
   * Supports comma-separated event names: `sse-swap="e1, e2"`.
   * OOB fragments inside the received HTML are processed by htmx's swap
   * pipeline automatically (htmx 2.x, `allowNestedOobSwaps: true`).
   */
  function bindSwap(elt, sseSwap, source) {
    sseSwap.split(",").map(function (s) { return s.trim(); }).forEach(function (eventName) {
      var key = "sseSwap:" + eventName;
      var d = api.getInternalData(elt);
      if (d[key]) return; // already bound

      var target = api.getTarget(elt) || elt;
      var swapSpec = api.getSwapSpecification(elt, null);

      var listener = function (e) {
        if (!api.bodyContains(elt)) {
          source.removeEventListener(eventName, listener);
          return;
        }
        // Use htmx's own swap so OOB fragments are handled transparently.
        api.swap(target, e.data, swapSpec);
      };

      d[key] = listener;
      source.addEventListener(eventName, listener);
    });
  }

  /** Bind an SSE event name that closes the source when received. */
  function bindClose(elt, sseClose, source) {
    sseClose.split(",").map(function (s) { return s.trim(); }).forEach(function (eventName) {
      var key = "sseClose:" + eventName;
      var d = api.getInternalData(elt);
      if (d[key]) return;

      var listener = function () {
        source.close();
      };

      d[key] = listener;
      source.addEventListener(eventName, listener);
    });
  }
})();
