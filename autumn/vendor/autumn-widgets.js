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
    } else if (getHidden().value) {
      // ID mode: a non-empty hidden value at init means a prior selection was
      // seeded server-side (an edit-form GET, or a 422 re-render preserving a
      // previous pick) — assign the name now so it round-trips on submit even
      // if the user never touches this field again. The 'input' listener
      // below still clears the name the moment the user edits the query,
      // same as it already does for a live-session selection.
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

  // initNav is guarded per-CONTROL (toggle / menu toggle), not once per nav
  // element: when htmx swaps a <nav data-autumn-nav>'s innerHTML (the nav
  // itself stays in the DOM, only its children are replaced), the swapped-in
  // hamburger/dropdown buttons are brand-new nodes with no dataset flags of
  // their own, so they need wiring again even though the nav was already
  // initialized once. The keydown listener is the one nav-level exception —
  // it always queries for the *current* open toggle at Escape-press time, so
  // it never needs re-registering and must only ever be added once.
  function initNav(nav) {
    nav.classList.add('autumn-nav--enhanced');

    var toggle = nav.querySelector('[data-nav-toggle]');
    var collapse = toggle && document.getElementById(toggle.getAttribute('aria-controls'));
    if (toggle && collapse && !toggle.dataset.navToggleInit) {
      toggle.dataset.navToggleInit = '1';
      toggle.hidden = false;
      toggle.addEventListener('click', function () {
        var open = toggle.getAttribute('aria-expanded') === 'true';
        toggle.setAttribute('aria-expanded', open ? 'false' : 'true');
        collapse.classList.toggle('autumn-nav__collapse--open', !open);
      });
    }

    nav.querySelectorAll('[data-nav-menu-toggle]').forEach(function (menuToggle) {
      if (menuToggle.dataset.navMenuToggleInit) return;
      menuToggle.dataset.navMenuToggleInit = '1';
      // Collapse the server-rendered "open" default without decrementing
      // openNavMenuCount — it was never incremented for these, so treating
      // this as a close would drive the counter negative.
      setNavMenuExpanded(menuToggle, false);
      menuToggle.addEventListener('click', function () {
        var open = menuToggle.getAttribute('aria-expanded') === 'true';
        // Re-query rather than close over the toggle list captured at wiring
        // time, so a later swap's newly-wired toggles are included too.
        nav.querySelectorAll('[data-nav-menu-toggle]').forEach(closeNavMenu);
        if (!open) openNavMenu(menuToggle);
      });
    });

    if (nav.dataset.navKeydownInit) return;
    nav.dataset.navKeydownInit = '1';
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

  // ── modal / confirm_action ────────────────────────────────────────────────
  // autumn_web::widgets::modal / confirm_action open and close their
  // <dialog> via the native HTML Invoker Commands API (command="show-modal"
  // / command="close" + commandfor) wherever it's supported — no JS needed
  // there. This is the fallback for browsers that don't support it yet:
  // one delegated click handler wired to the same data-modal-open /
  // data-modal-close attributes the widget always emits. showModal()/close()
  // give ESC-close, a focus trap, and focus-into/-return-from the dialog for
  // free from the browser in both paths.
  if (!('command' in HTMLButtonElement.prototype)) {
    document.addEventListener('click', function (e) {
      var opener = e.target.closest('[data-modal-open]');
      if (opener) {
        var toOpen = document.getElementById(opener.getAttribute('data-modal-open'));
        if (toOpen && toOpen.showModal) toOpen.showModal();
        return;
      }
      var closer = e.target.closest('[data-modal-close]');
      if (closer) {
        var toClose = document.getElementById(closer.getAttribute('data-modal-close'));
        if (toClose && toClose.close) toClose.close();
      }
    });
  }

  // ── modal light-dismiss (closedby="any") backdrop-click polyfill ──────────
  // `closedby="any"` (ModalConfig::light_dismiss) is a newer <dialog>
  // feature with limited browser support — where it's unsupported, the
  // native backdrop click silently does nothing. Always wired (not
  // feature-detected): dialog.close() on an already-closed dialog is a
  // spec'd no-op, so this never double-fires where closedby IS natively
  // supported. e.target === the dialog itself only happens for a genuine
  // backdrop click here: .autumn-modal has zero padding and its one child,
  // .autumn-modal__content, fills the whole box, so any click landing
  // inside the visible dialog always targets a descendant, never the
  // <dialog> element directly.
  document.addEventListener('click', function (e) {
    var t = e.target;
    if (t.matches && t.matches('dialog[closedby="any"][open]')) t.close();
  });

  // ── theme_toggle (#3396) ──────────────────────────────────────────────────
  // autumn_web::widgets::theme_toggle is a no-JS POST form. This code applies
  // the choice in place: data-theme on <html> and aria-pressed on each toggle.
  // A toggle that posts to the framework route also writes the autumn.theme
  // cookie (the cookie that route writes) and sends no request. A toggle with
  // an app action posts as usual, so the app stores the choice its own way.
  var THEMES = ['system', 'light', 'dark'];
  var THEME_PATH = '/_autumn/theme';

  // The first valid stored theme, or null. The server reads it the same way.
  function readThemeCookie() {
    var pairs = document.cookie.split(';');
    for (var i = 0; i < pairs.length; i++) {
      var pair = pairs[i].trim();
      if (pair.indexOf('autumn.theme=') !== 0) continue;
      var value = pair.slice('autumn.theme='.length);
      if (THEMES.indexOf(value) >= 0) return value;
    }
    return null;
  }

  function applyTheme(theme) {
    var root = document.documentElement;
    if (theme === 'system') root.removeAttribute('data-theme');
    else root.setAttribute('data-theme', theme);
    document.querySelectorAll('[data-autumn-theme-toggle] button[name="theme"]')
      .forEach(function (button) {
        button.setAttribute('aria-pressed', button.value === theme ? 'true' : 'false');
      });
  }

  function storeTheme(theme) {
    document.cookie = theme === 'system'
      ? 'autumn.theme=; Max-Age=0; Path=/; SameSite=Lax'
      : 'autumn.theme=' + theme + '; Max-Age=31536000; Path=/; SameSite=Lax';
  }

  // The listener uses the capture phase. Thus an htmx-boosted form also
  // sends nothing for the framework route.
  document.addEventListener('submit', function (e) {
    var form = e.target;
    if (!form.matches || !form.matches('form[data-autumn-theme-toggle]')) return;
    var button = e.submitter;
    if (!button || button.name !== 'theme' || THEMES.indexOf(button.value) < 0) return;
    applyTheme(button.value);
    if (form.getAttribute('action') === THEME_PATH) {
      storeTheme(button.value);
      e.preventDefault();
      e.stopPropagation();
    }
  }, true);

  // A cached page can carry a stale data-theme. Only a page with a
  // cookie-backed toggle (one that posts to the framework route) follows the
  // cookie: its layout renders the stored preference, and no cookie means
  // System. Other pages keep the data-theme their app set, for example a
  // light-only pin or a theme read from a user profile.
  function syncTheme() {
    var cookieBacked = 'form[data-autumn-theme-toggle][action="' + THEME_PATH + '"]';
    if (!document.querySelector(cookieBacked)) return;
    applyTheme(readThemeCookie() || 'system');
  }

  // The back/forward cache restores a page without a new load.
  window.addEventListener('pageshow', function (e) {
    if (e.persisted) syncTheme();
  });

  function initAll() {
    document.querySelectorAll('[data-ac-value-id]').forEach(initAutocomplete);
    document.querySelectorAll('[data-autumn-nav]').forEach(initNav);
    syncTheme();
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
