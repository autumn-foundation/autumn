// Autumn widget runtime — ships as a same-origin static asset so it works
// under the default `script-src 'self'` Content Security Policy.
// Include once in your layout:
//   <script src="/static/js/autumn-widgets.js" defer></script>
(function () {
  'use strict';

  // Guard: only initialize once even when the script tag appears multiple times.
  if (window.__autumnWidgets) return;
  window.__autumnWidgets = true;

  // ── Min-length enforcement ────────────────────────────────────────────────
  // Cancel htmx requests from active-search or autocomplete inputs when the
  // typed value is non-empty but shorter than data-ac-min-length.
  // Empty inputs are NOT cancelled so the server can clear stale results.
  // When a below-minimum request is cancelled, also clear the results container
  // so results from a previous valid query don't remain visible.
  document.addEventListener('htmx:configRequest', function (e) {
    var elt = e.detail && e.detail.elt;
    if (!elt || !elt.dataset) return;
    var minLen = parseInt(elt.dataset.acMinLength || '0', 10);
    var len = (elt.value || '').length;
    if (minLen > 0 && len > 0 && len < minLen) {
      e.preventDefault();
      // Clear the htmx target so stale results from a previous valid query
      // don't remain visible while the input is below the minimum length.
      var targetSel = elt.getAttribute('hx-target');
      if (targetSel) {
        var target = document.querySelector(targetSel);
        if (target) target.innerHTML = '';
      }
    }
  });

  // ── Autocomplete widget ───────────────────────────────────────────────────

  function initAutocomplete(wrapper) {
    if (wrapper.dataset.acInit) return;
    wrapper.dataset.acInit = '1';

    var queryInput = wrapper.querySelector('[data-ac-query]');
    var valueId = wrapper.dataset.acValueId;
    var valueName = wrapper.dataset.acValueName;
    var freeText = 'acFreeText' in wrapper.dataset;
    var listbox = wrapper.querySelector('[role="listbox"]');
    if (!queryInput || !valueId || !valueName || !listbox) return;

    function getHidden() { return document.getElementById(valueId); }

    // Free-text mode: assign the name immediately so the field is always
    // included in form submission even if the user never types in the input.
    // This preserves any default value set on the hidden input in the HTML.
    if (freeText) {
      getHidden().name = valueName;
    }

    function selectOption(opt) {
      var hidden = getHidden();
      hidden.name = valueName;
      hidden.value = opt.dataset.value || '';
      queryInput.value = opt.textContent.trim();
      listbox.innerHTML = '';
    }

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
      var minLen = parseInt(queryInput.dataset.acMinLength || '0', 10);
      if (freeText) {
        // Free-text mode: keep the hidden field in sync with the typed query.
        // Use this for tag-style fields where the submitted value is the text itself.
        hidden.name = valueName;
        hidden.value = queryInput.value;
      } else {
        // ID mode (default): a stale selection is invalid once the user edits
        // the query. The hidden field gains its name only when an option is
        // selected so no-JS forms are not broken by a phantom empty field.
        hidden.name = '';
        hidden.value = '';
      }
      // Clear stale options when the query drops below the minimum length.
      if (minLen > 0 && queryInput.value.length < minLen) {
        listbox.innerHTML = '';
      }
    });
  }

  // ── nav_bar widget ──────────────────────────────────────────────────────
  // Progressive enhancement for autumn_web::widgets::nav_bar: before this
  // runs, the hamburger toggle is hidden and every dropdown is rendered open
  // so all links stay visible with JS off. Once it runs, the hamburger is
  // revealed and dropdowns collapse into working disclosure widgets.

  // Total open dropdown menus across every nav_bar on the page, so the
  // document-level click-outside handler below can skip its DOM query
  // entirely on the common case where nothing is open.
  var openNavMenuCount = 0;

  // Set expanded/collapsed DOM state without touching openNavMenuCount —
  // used for the init-time normalization below, which collapses the
  // server-rendered "all open" default rather than reacting to a real
  // user-driven open/close transition the counter should track.
  function setNavMenuExpanded(toggle, expanded) {
    var menu = document.getElementById(toggle.getAttribute('aria-controls'));
    if (!menu) return;
    toggle.setAttribute('aria-expanded', expanded ? 'true' : 'false');
    menu.hidden = !expanded;
  }

  function closeNavMenu(toggle) {
    if (toggle.getAttribute('aria-expanded') === 'true') openNavMenuCount--;
    setNavMenuExpanded(toggle, false);
  }

  function openNavMenu(toggle) {
    if (toggle.getAttribute('aria-expanded') !== 'true') openNavMenuCount++;
    setNavMenuExpanded(toggle, true);
  }

  function initNav(nav) {
    if (nav.dataset.navInit) return;
    nav.dataset.navInit = '1';
    nav.classList.add('autumn-nav--enhanced');

    var toggle = nav.querySelector('[data-nav-toggle]');
    var collapse = toggle && document.getElementById(toggle.getAttribute('aria-controls'));
    if (toggle && collapse) {
      toggle.hidden = false;
      toggle.addEventListener('click', function () {
        var open = toggle.getAttribute('aria-expanded') === 'true';
        toggle.setAttribute('aria-expanded', open ? 'false' : 'true');
        collapse.classList.toggle('autumn-nav__collapse--open', !open);
      });
    }

    var menuToggles = nav.querySelectorAll('[data-nav-menu-toggle]');
    menuToggles.forEach(function (menuToggle) {
      // Collapse the server-rendered "open" default without decrementing
      // openNavMenuCount — it was never incremented for these, so treating
      // this as a close would drive the counter negative.
      setNavMenuExpanded(menuToggle, false);
      menuToggle.addEventListener('click', function () {
        var open = menuToggle.getAttribute('aria-expanded') === 'true';
        menuToggles.forEach(closeNavMenu);
        if (!open) openNavMenu(menuToggle);
      });
    });

    // Escape closes the open dropdown (if any) and refocuses its trigger.
    nav.addEventListener('keydown', function (e) {
      if (e.key !== 'Escape') return;
      var openToggle = nav.querySelector('[data-nav-menu-toggle][aria-expanded="true"]');
      if (!openToggle) return;
      closeNavMenu(openToggle);
      openToggle.focus();
    });
  }

  // Clicking outside a nav_bar closes any of its open dropdowns. Skips the
  // DOM query entirely when no dropdown is open anywhere on the page.
  document.addEventListener('click', function (e) {
    if (openNavMenuCount === 0) return;
    document.querySelectorAll('[data-nav-menu-toggle][aria-expanded="true"]').forEach(function (menuToggle) {
      var nav = menuToggle.closest('[data-autumn-nav]');
      if (nav && !nav.contains(e.target)) closeNavMenu(menuToggle);
    });
  });

  function initAll() {
    document.querySelectorAll('[data-ac-value-id]').forEach(initAutocomplete);
    document.querySelectorAll('[data-autumn-nav]').forEach(initNav);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', initAll);
  } else {
    initAll();
  }

  // Re-initialize after htmx swaps in new widgets.
  // Also check the swapped-in target itself in case it IS the wrapper
  // (e.g. hx-swap="outerHTML" on the wrapper element).
  document.addEventListener('htmx:afterSwap', function (e) {
    if (!e.detail || !e.detail.target) return;
    var t = e.detail.target;
    if (t.dataset && t.dataset.acValueId) {
      initAutocomplete(t);
    }
    t.querySelectorAll('[data-ac-value-id]').forEach(initAutocomplete);
    if (t.dataset && 'autumnNav' in t.dataset) {
      initNav(t);
    }
    t.querySelectorAll('[data-autumn-nav]').forEach(initNav);
  });
})();
