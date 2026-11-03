// Autumn search widget runtime (issue #3408): active search and
// autocomplete. With the `widgets-search` feature on, the server puts this
// part first in /static/js/autumn-widgets.js, before the core runtime.
(function () {
  'use strict';

  // The core sets this flag after this part. A second copy stops here.
  if (window.__autumnWidgets) return;

  // Init guards. A WeakSet, not a data-* flag: htmx history snapshots keep
  // attributes but not listeners, so a restored node must init again.
  var ready = new WeakSet();
  function once(el) {
    if (ready.has(el)) return false;
    ready.add(el);
    return true;
  }

  // No option is active. The keyboard core moves it again on a key press.
  function clearActive(ctrl, list) {
    list.querySelectorAll('[role="option"]').forEach(function (el) {
      el.setAttribute('aria-selected', 'false');
    });
    ctrl.removeAttribute('aria-activedescendant');
  }

  // ── Min-length enforcement ──
  // Cancel a request when the value is shorter than data-ac-min-length, and
  // clear the old results. An empty value is sent, so the server can clear.
  document.addEventListener('htmx:configRequest', function (e) {
    var elt = e.detail && e.detail.elt;
    if (!elt || !elt.dataset) return;
    var minLen = parseInt(elt.dataset.acMinLength || '0', 10);
    var len = (elt.value || '').length;
    if (minLen > 0 && len > 0 && len < minLen) {
      e.preventDefault();
      var targetSel = elt.getAttribute('hx-target');
      if (targetSel) {
        var target = document.querySelector(targetSel);
        if (target) target.innerHTML = '';
      }
    }
  });

  // ── Autocomplete widget ──
  // The keyboard core moves through the listbox. This code selects options
  // and keeps aria-expanded true only while the popup shows an option.

  function initAutocomplete(wrapper) {
    if (!once(wrapper)) return;

    var queryInput = wrapper.querySelector('[data-ac-query]');
    var valueId = wrapper.dataset.acValueId;
    var valueName = wrapper.dataset.acValueName;
    var freeText = 'acFreeText' in wrapper.dataset;
    var listbox = wrapper.querySelector('[role="listbox"]');
    if (!queryInput || !valueId || !valueName || !listbox) return;

    function getHidden() { return document.getElementById(valueId); }

    // Free-text mode: name the field now, so it always submits.
    if (freeText) {
      getHidden().name = valueName;
    } else if (getHidden().value) {
      // ID mode: a value at init is a server-side pick (an edit form, a 422
      // re-render). Name the field so it submits. Typing clears it again.
      getHidden().name = valueName;
    }

    // A dismissal (Escape, Tab, a click outside) holds until the user types
    // or presses ArrowDown/ArrowUp. Thus a late response stays closed.
    var dismissed = false;
    listbox.addEventListener('autumn:dismiss', function () { dismissed = true; });
    queryInput.addEventListener('keydown', function (e) {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') dismissed = false;
    });

    // New content (an htmx swap, or a clear) shows the popup again, unless
    // the user dismissed it. An empty listbox stays in the accessibility
    // tree, so its live region still speaks; the CSS gives it no size.
    function sync() {
      var options = listbox.querySelectorAll('[role="option"]');
      options.forEach(function (o) { o.tabIndex = -1; });
      listbox.hidden = dismissed;
      queryInput.setAttribute('aria-expanded', !listbox.hidden && options.length ? 'true' : 'false');
      queryInput.removeAttribute('aria-activedescendant');
    }
    // `subtree` also sees a morph swap that keeps the option count.
    new MutationObserver(sync).observe(listbox, { childList: true, subtree: true });
    sync();

    function selectOption(opt) {
      var hidden = getHidden();
      hidden.name = valueName;
      hidden.value = opt.dataset.value || '';
      queryInput.value = opt.textContent.trim();
      listbox.innerHTML = '';
      sync();
      queryInput.focus();
    }

    // A mouse press on an option must not take focus from the input.
    listbox.addEventListener('mousedown', function (e) { e.preventDefault(); });

    listbox.addEventListener('click', function (e) {
      var opt = e.target.closest('[role="option"]');
      if (opt) selectOption(opt);
    });

    listbox.addEventListener('keydown', function (e) {
      var opt = e.target.closest('[role="option"]');
      if (!opt || (e.key !== 'Enter' && e.key !== ' ')) return;
      e.preventDefault();
      selectOption(opt);
    });

    queryInput.addEventListener('input', function () {
      var hidden = getHidden();
      dismissed = false;
      // The highlight belongs to the old query. Enter must not commit it.
      clearActive(queryInput, listbox);
      var minLen = parseInt(queryInput.dataset.acMinLength || '0', 10);
      if (freeText) {
        // Free-text mode: the typed text is the value.
        hidden.name = valueName;
        hidden.value = queryInput.value;
      } else {
        // ID mode: an edit makes the pick stale. The field gets its name
        // again only when an option is selected.
        hidden.name = '';
        hidden.value = '';
      }
      // Clear stale options when the query drops below the minimum length.
      if (minLen > 0 && queryInput.value.length < minLen) {
        listbox.innerHTML = '';
      }
    });
  }

  // The core fires `autumn:init` on each new root. This part is first in
  // the bundle, so the listener exists before the core boots.
  document.addEventListener('autumn:init', function (e) {
    var root = e.target;
    if (root.matches && root.matches('[data-ac-value-id]')) initAutocomplete(root);
    if (root.querySelectorAll) root.querySelectorAll('[data-ac-value-id]').forEach(initAutocomplete);
  });
})();
