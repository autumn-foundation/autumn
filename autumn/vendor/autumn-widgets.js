// Autumn widget runtime — ships as a same-origin static asset so it works
// under the default `script-src 'self'` Content Security Policy.
// Include once in your layout:
//   <script src="/static/js/autumn-widgets.js" defer></script>
(function () {
  'use strict';

  // Guard: only initialize once even when the script tag appears multiple times.
  if (window.__autumnWidgets) return;
  window.__autumnWidgets = true;

  // ── Keyboard core (issue #3397) ───────────────────────────────────────────
  // WAI-ARIA APG keyboard model, bound by data-* attributes. Listeners sit on
  // `document`, so widgets that htmx swaps in work at once. Contract (see
  // docs/guide/keyboard.md):
  //
  //   data-autumn-roving[="<items>"]   roving tabindex over the items
  //   data-autumn-activedescendant     focus stays on the controller
  //   data-autumn-orientation          "horizontal" | "vertical" | "both"
  //   data-autumn-wrap="false"         stop at the first and last item
  //   data-autumn-typeahead            type a letter to move (roving only)
  //   data-autumn-dismiss              Escape, outside click or focus-out closes
  //
  // The controller (or trigger) is the element whose aria-controls names the
  // list (or layer). Events: `autumn:navigate` (detail.item), `autumn:dismiss`.
  // `autumn:init` fires on each new root (the page, an htmx swap) before
  // roving init, so plugin JS can set roles that roving reads (#3400).

  var ITEMS = '[role="tab"],[role="menuitem"],[role="option"],[role="radio"],[role="treeitem"]';
  var LIST = '[data-autumn-roving],[data-autumn-activedescendant]';
  var uid = 0;

  // Init guards. A WeakSet, not a data-* flag: htmx history snapshots keep
  // attributes but not listeners, so a restored node must init again.
  var ready = new WeakSet();
  function once(el) {
    if (ready.has(el)) return false;
    ready.add(el);
    return true;
  }

  function emit(el, name, item) {
    el.dispatchEvent(new CustomEvent(name, { bubbles: true, detail: { item: item } }));
  }

  // The items of a list: not disabled, not hidden, not in a nested list.
  function listItems(list) {
    var sel = list.getAttribute('data-autumn-roving') || ITEMS;
    return Array.prototype.filter.call(list.querySelectorAll(sel), function (el) {
      return !el.hidden && el.getAttribute('aria-disabled') !== 'true' &&
        el.closest(LIST) === list;
    });
  }

  // The new index for `key`, or -1 when the key does not move.
  function nextIndex(list, count, index, key) {
    if (key === 'Home') return 0;
    if (key === 'End') return count - 1;
    var o = list.getAttribute('data-autumn-orientation') || 'both';
    var rtl = getComputedStyle(list).direction === 'rtl';
    var d = 0;
    if (o !== 'vertical') {
      if (key === (rtl ? 'ArrowLeft' : 'ArrowRight')) d = 1;
      if (key === (rtl ? 'ArrowRight' : 'ArrowLeft')) d = -1;
    }
    if (o !== 'horizontal') {
      if (key === 'ArrowDown') d = 1;
      if (key === 'ArrowUp') d = -1;
    }
    if (!d) return -1;
    if (index < 0) return d > 0 ? 0 : count - 1;
    var n = index + d;
    if (n >= 0 && n < count) return n;
    return list.getAttribute('data-autumn-wrap') === 'false' ? index : (n + count) % count;
  }

  // Typeahead: one repeated letter cycles; a longer string matches a prefix.
  // The buffer resets after 500 ms or in a different list.
  var typed = '';
  var typedAt = 0;
  var typedList = null;
  function typeahead(list, all, index, ch) {
    var now = Date.now();
    typed = (list !== typedList || now - typedAt > 500 ? '' : typed) + ch.toLowerCase();
    typedAt = now;
    typedList = list;
    var cycle = typed.split('').every(function (c) { return c === typed[0]; });
    var q = cycle ? typed[0] : typed;
    var start = cycle ? index + 1 : Math.max(index, 0);
    for (var i = 0; i < all.length; i++) {
      var el = all[(start + i) % all.length];
      if (el.textContent.trim().toLowerCase().indexOf(q) === 0) return el;
    }
    return null;
  }

  function setRoving(list, item) {
    listItems(list).forEach(function (el) { el.tabIndex = el === item ? 0 : -1; });
  }

  function focusItem(list, item) {
    setRoving(list, item);
    item.focus();
    emit(list, 'autumn:navigate', item);
  }

  // One tab stop per list: the selected or current item, else the first.
  function initRoving(list) {
    var all = listItems(list);
    var pick = all.filter(function (el) {
      return el.getAttribute('aria-selected') === 'true' ||
        el.getAttribute('aria-checked') === 'true' || el.hasAttribute('aria-current');
    })[0];
    if (all.length) setRoving(list, pick || all[0]);
  }

  // Move the active descendant. `null` clears it.
  function setActive(ctrl, list, item) {
    listItems(list).forEach(function (el) {
      el.setAttribute('aria-selected', el === item ? 'true' : 'false');
    });
    if (!item) {
      ctrl.removeAttribute('aria-activedescendant');
      return;
    }
    if (!item.id) item.id = 'autumn-ad-' + (++uid);
    ctrl.setAttribute('aria-activedescendant', item.id);
    if (item.scrollIntoView) item.scrollIntoView({ block: 'nearest' });
    emit(list, 'autumn:navigate', item);
  }

  // aria-controls is a list of ids, so match one word of it.
  function triggerOf(layer) {
    return layer.id ? document.querySelector('[aria-controls~="' + CSS.escape(layer.id) + '"]') : null;
  }

  // The data-autumn-activedescendant list that `el` controls, if any.
  function controlledList(el) {
    var ids = (el.getAttribute('aria-controls') || '').split(/\s+/);
    for (var i = 0; i < ids.length; i++) {
      var list = ids[i] && document.getElementById(ids[i]);
      if (list && list.hasAttribute('data-autumn-activedescendant')) return list;
    }
    return null;
  }

  // Show or hide a layer and keep its trigger's aria-expanded in sync.
  function setOpen(layer, open) {
    var trigger = triggerOf(layer);
    layer.hidden = !open;
    if (!trigger) return;
    trigger.setAttribute('aria-expanded', open ? 'true' : 'false');
    if (!open && layer.hasAttribute('data-autumn-activedescendant')) setActive(trigger, layer, null);
  }

  function dismiss(layer, refocus) {
    var trigger = triggerOf(layer);
    setOpen(layer, false);
    if (refocus && trigger) trigger.focus();
    emit(layer, 'autumn:dismiss', null);
  }

  // Calls `fn(layer, inside)` for each open layer. `inside` tells if `node`
  // is in the layer or its trigger. Inner layers come first. An empty layer
  // is not open.
  function eachOpenLayer(node, fn) {
    var layers = document.querySelectorAll('[data-autumn-dismiss]:not([hidden])');
    for (var i = layers.length - 1; i >= 0; i--) {
      if (!layers[i].childElementCount) continue;
      var trigger = triggerOf(layers[i]);
      var inside = !!node && (layers[i].contains(node) || (!!trigger && trigger.contains(node)));
      if (fn(layers[i], inside) === false) return;
    }
  }

  function rovingKey(e, list) {
    var all = listItems(list);
    var index = all.indexOf(e.target);
    if (index < 0) return;
    var n = nextIndex(list, all.length, index, e.key);
    var item = n >= 0 ? all[n] : null;
    if (!item && e.key.length === 1 && e.key !== ' ' && list.hasAttribute('data-autumn-typeahead')) {
      item = typeahead(list, all, index, e.key);
    }
    if (!item) return;
    e.preventDefault();
    focusItem(list, item);
  }

  function activeKey(e, ctrl, list) {
    var all = listItems(list);
    var index = all.indexOf(document.getElementById(ctrl.getAttribute('aria-activedescendant') || ''));
    if (e.key === 'Enter') {
      if (index < 0 || list.hidden) return;
      e.preventDefault();
      all[index].click();
      return;
    }
    // In a text box, Home, End, Left and Right move the caret.
    var textBox = ctrl.matches('input, textarea');
    if (textBox && /^(Home|End|ArrowLeft|ArrowRight)$/.test(e.key)) return;
    var n = all.length ? nextIndex(list, all.length, list.hidden ? -1 : index, e.key) : -1;
    if (n < 0) return;
    e.preventDefault();
    if (list.hidden) setOpen(list, true);
    setActive(ctrl, list, all[n]);
  }

  document.addEventListener('keydown', function (e) {
    var t = e.target;
    if (e.defaultPrevented || e.altKey || e.ctrlKey || e.metaKey || !t || !t.closest) return;
    // An IME composition owns Enter and the arrow keys.
    if (e.isComposing || e.keyCode === 229) return;
    if (e.key === 'Escape') {
      eachOpenLayer(t, function (layer, inside) {
        if (!inside) return true;
        e.preventDefault();
        dismiss(layer, layer.contains(t));
        return false;
      });
      return;
    }
    var controlled = controlledList(t);
    if (controlled) {
      activeKey(e, t, controlled);
      return;
    }
    var list = t.closest('[data-autumn-roving]');
    if (list) rovingKey(e, list);
  });

  // A pointer or script focus moves the tab stop too.
  document.addEventListener('focusin', function (e) {
    var list = e.target.closest && e.target.closest('[data-autumn-roving]');
    if (list && listItems(list).indexOf(e.target) >= 0) setRoving(list, e.target);
  });

  // A click outside a layer and its trigger closes the layer.
  document.addEventListener('click', function (e) {
    eachOpenLayer(e.target, function (layer, inside) {
      if (!inside) dismiss(layer, false);
    });
  });

  // Focus that leaves a layer and its trigger closes the layer. A null
  // relatedTarget (a click on a non-focusable area) is left to the click
  // handler above.
  document.addEventListener('focusout', function (e) {
    var to = e.relatedTarget;
    if (!to) return;
    eachOpenLayer(e.target, function (layer, inside) {
      if (!inside) return;
      var trigger = triggerOf(layer);
      if (!layer.contains(to) && !(trigger && trigger.contains(to))) dismiss(layer, false);
    });
  });

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

  // ── Tabs widget ───────────────────────────────────────────────────────────
  // APG tabs with automatic activation. JS adds `autumn-tabs--enhanced`, so
  // the CSS shows panels by `[hidden]`, not by `:target`. With JS off, the
  // `:target` CSS still switches panels.

  function ownTablist(root) {
    for (var i = 0; i < root.children.length; i++) {
      if (root.children[i].getAttribute('role') === 'tablist') return root.children[i];
    }
    return null;
  }

  function panelOf(tab) {
    return document.getElementById(tab.getAttribute('aria-controls') || '');
  }

  function selectTab(list, tab, updateUrl) {
    listItems(list).forEach(function (t) {
      var on = t === tab;
      var panel = panelOf(t);
      t.setAttribute('aria-selected', on ? 'true' : 'false');
      t.classList.toggle('autumn-tabs__tab--active', on);
      t.tabIndex = on ? 0 : -1;
      if (panel) {
        panel.hidden = !on;
        panel.classList.toggle('autumn-tabs__panel--active', on);
      }
    });
    var shown = panelOf(tab);
    // Keep the deep link. replaceState does not scroll or add history.
    if (updateUrl && shown && location.hash !== '#' + shown.id) {
      history.replaceState(history.state, '', '#' + shown.id);
    }
  }

  // The tab whose panel is, or holds, the URL fragment's element.
  function tabFromHash(list) {
    var target = null;
    try {
      target = document.getElementById(decodeURIComponent(location.hash.slice(1)));
    } catch (_) {
      return null; // A malformed fragment, e.g. "#100%".
    }
    if (!target) return null;
    return listItems(list).filter(function (t) {
      var panel = panelOf(t);
      return panel && panel.contains(target);
    })[0] || null;
  }

  // Guard on the tablist, not the root: an innerHTML swap into the root
  // keeps the root but brings a new tablist and new panels.
  function initTabs(root) {
    var list = ownTablist(root);
    if (!list || !once(list)) return;
    root.classList.add('autumn-tabs--enhanced');
    var tabs = listItems(list);
    if (!tabs.length) return;
    var selected = tabs.filter(function (t) { return t.getAttribute('aria-selected') === 'true'; })[0];
    selectTab(list, tabFromHash(list) || selected || tabs[0], false);
    list.addEventListener('click', function (e) {
      // A modified click opens the deep link in a new tab or window.
      if (e.ctrlKey || e.metaKey || e.shiftKey || e.altKey || e.button !== 0) return;
      var tab = e.target.closest('[role="tab"]');
      if (!tab || tab.closest('[role="tablist"]') !== list) return;
      e.preventDefault();
      selectTab(list, tab, true);
      tab.focus();
    });
    list.addEventListener('autumn:navigate', function (e) {
      selectTab(list, e.detail.item, true);
    });
    // Space on a link scrolls the page. The focused tab is already selected.
    list.addEventListener('keydown', function (e) {
      if (e.key === ' ' && e.target.closest('[role="tab"]')) e.preventDefault();
    });
  }

  // An in-page link into a hidden panel: show the panel, then scroll, as the
  // browser could not scroll to a hidden element.
  window.addEventListener('hashchange', function () {
    var shown = false;
    document.querySelectorAll('.autumn-tabs--enhanced').forEach(function (root) {
      var list = ownTablist(root);
      var tab = list && tabFromHash(list);
      if (tab) {
        selectTab(list, tab, false);
        shown = true;
      }
    });
    var target = shown && document.getElementById(location.hash.slice(1));
    if (target) target.scrollIntoView();
  });

  // ── Autocomplete widget ───────────────────────────────────────────────────
  // The listbox uses the keyboard core (data-autumn-activedescendant and
  // data-autumn-dismiss). This code selects options and keeps
  // aria-expanded true only while the popup shows at least one option.

  function initAutocomplete(wrapper) {
    if (!once(wrapper)) return;

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
      setActive(queryInput, listbox, null);
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
  // runs, the hamburger toggle is hidden. Once it runs, the hamburger is
  // revealed. Dropdowns are popover menus (see below).

  // initNav is guarded per-CONTROL (the toggle), not once per nav element:
  // when htmx swaps a <nav data-autumn-nav>'s innerHTML (the nav itself stays
  // in the DOM, only its children are replaced), the swapped-in hamburger is
  // a brand-new node, so it needs wiring again even though the nav was
  // already initialized once.
  function initNav(nav) {
    nav.classList.add('autumn-nav--enhanced');

    var toggle = nav.querySelector('[data-nav-toggle]');
    var collapse = toggle && document.getElementById(toggle.getAttribute('aria-controls'));
    if (toggle && collapse && once(toggle)) {
      toggle.hidden = false;
      toggle.addEventListener('click', function () {
        var open = toggle.getAttribute('aria-expanded') === 'true';
        toggle.setAttribute('aria-expanded', open ? 'false' : 'true');
        collapse.classList.toggle('autumn-nav__collapse--open', !open);
      });
    }
  }

  // ── popover menu (issue #3402) ────────────────────────────────────────────
  // dropdown_menu and NavMenu are native popovers. The browser opens them from
  // command/commandfor and gives light dismiss, Escape and focus return. This
  // adds the rest of the APG menu button: aria-expanded, focus on open,
  // ArrowDown/ArrowUp on the button, Space on a link, and Tab or a click on an
  // item to close. The keyboard core gives the arrows, Home/End and typeahead.

  var MENU = '[data-autumn-menu]';
  var MENU_BUTTON = '[aria-haspopup="menu"][commandfor]';
  var menuAt = null; // { menu, at }: where focus goes when that menu opens

  function menuOfButton(btn) {
    var m = btn && document.getElementById(btn.getAttribute('commandfor'));
    return m && m.matches(MENU) ? m : null;
  }

  function isOpen(el) { return el.matches(':popover-open'); }

  function initMenu(menu) {
    var trigger = triggerOf(menu);
    if (trigger) trigger.setAttribute('aria-expanded', isOpen(menu) ? 'true' : 'false');
  }

  document.addEventListener('toggle', function (e) {
    var menu = e.target;
    if (!menu.matches || !menu.matches(MENU)) return;
    var open = e.newState === 'open';
    var trigger = triggerOf(menu);
    if (trigger) trigger.setAttribute('aria-expanded', open ? 'true' : 'false');
    var req = menuAt;
    menuAt = null;
    var all = open && req && req.menu === menu ? listItems(menu) : [];
    if (all.length) focusItem(menu, req.at === 'last' ? all[all.length - 1] : all[0]);
  }, true);

  document.addEventListener('click', function (e) {
    if (!e.target.closest) return;
    var button = e.target.closest(MENU_BUTTON);
    if (button) {
      // A keyboard click (Enter or Space) has detail 0 and focuses the first item.
      var menu = menuOfButton(button);
      menuAt = menu && e.detail === 0 ? { menu: menu, at: 'first' } : null;
      return;
    }
    var item = e.target.closest(MENU + ' [role="menuitem"]');
    var owner = item && item.closest(MENU);
    if (owner && isOpen(owner)) owner.hidePopover();
  });

  document.addEventListener('keydown', function (e) {
    var t = e.target;
    if (e.defaultPrevented || e.altKey || e.ctrlKey || e.metaKey || !t || !t.closest) return;
    var menu = menuOfButton(t.closest(MENU_BUTTON));
    if (menu && (e.key === 'ArrowDown' || e.key === 'ArrowUp')) {
      e.preventDefault();
      menuAt = { menu: menu, at: e.key === 'ArrowUp' ? 'last' : 'first' };
      if (isOpen(menu)) {
        var all = listItems(menu);
        menuAt = null;
        if (all.length) focusItem(menu, e.key === 'ArrowUp' ? all[all.length - 1] : all[0]);
      } else {
        menu.showPopover();
      }
      return;
    }
    var inside = t.closest(MENU);
    if (!inside) return;
    // Tab leaves the menu: close it, then focus moves on from the button.
    if (e.key === 'Tab' && isOpen(inside)) inside.hidePopover();
    // Space activates a link item, as Enter does.
    if (e.key === ' ' && t.matches('a[role="menuitem"]')) {
      e.preventDefault();
      t.click();
    }
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
      var pop = e.target.closest('[data-popover-toggle]');
      var panel = pop && document.getElementById(pop.getAttribute('data-popover-toggle'));
      if (panel && panel.togglePopover) {
        panel.togglePopover();
        return;
      }
      var opener = e.target.closest('[data-modal-open]');
      if (opener) {
        var toOpen = document.getElementById(opener.getAttribute('data-modal-open'));
        if (toOpen && toOpen.showModal) toOpen.showModal();
        // These browsers send no `toggle` event for a dialog.
        raiseAll();
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

  // ── toast (#3399) ─────────────────────────────────────────────────────────
  // CSS runs the timer. Hover and focus pause it. This code shows the close
  // button, applies the duration, removes ended toasts, and keeps the region
  // above a modal.
  var TOAST = '[data-autumn-toast]';
  var REGION = '[data-autumn-toast-region]';
  var CLOSE = '[data-autumn-toast-close]';

  function initToast(el) {
    // htmx can remove a toast before a queued init runs.
    if (!el.isConnected) return;
    var ms = parseInt(el.getAttribute('data-autumn-toast-duration'), 10);
    // CSSOM, not a style attribute, so the CSP allows it.
    if (ms >= 0) el.style.setProperty('--autumn-toast-duration', ms + 'ms');
    var close = el.querySelector(CLOSE);
    var msg = el.querySelector('.autumn-toast__message');
    if (!close) return;
    // Each close button names its own message. Skip ids in use: an htmx
    // history restore can bring back an id the counter gives out again.
    if (msg) {
      while (msg.isConnected && (!msg.id || document.getElementById(msg.id) !== msg)) msg.id = 'autumn-toast-' + (++uid);
      close.setAttribute('aria-describedby', msg.id);
    }
    close.hidden = false;
  }

  // The top layer stacks in open order. Open the region again to move it up.
  // Hiding a popover can restart its CSS animations and move focus, so put
  // back the animation times and the focus.
  function raise(region) {
    if (!region.showPopover || !region.isConnected) return;
    var active = document.activeElement;
    var saved = region.getAnimations({ subtree: true });
    try {
      if (region.matches(':popover-open')) region.hidePopover();
      region.showPopover();
    } catch (err) { return; }
    region.getAnimations({ subtree: true }).forEach(function (a) {
      saved.forEach(function (s) {
        if (s !== a && s.animationName === a.animationName && s.effect.target === a.effect.target &&
          s.currentTime !== null) a.currentTime = s.currentTime;
      });
    });
    if (region.contains(active)) active.focus({ preventScroll: true });
  }

  function raiseAll() {
    document.querySelectorAll(REGION).forEach(raise);
  }

  function initRegion(region) {
    if (!once(region)) return;
    raise(region);
    new MutationObserver(function (records) {
      records.forEach(function (r) {
        r.addedNodes.forEach(function (n) { if (n.nodeType === 1) each(n, TOAST, initToast); });
      });
      // Fallback: some browsers send no `toggle` event for a dialog.
      try {
        if (document.querySelector('dialog:modal')) raise(region);
      } catch (err) { /* no :modal */ }
    }).observe(region, { childList: true });
  }

  // A modal or a popover opened. `toggle` does not bubble, so listen in the
  // capture phase.
  document.addEventListener('toggle', function (e) {
    var t = e.target;
    if (e.newState === 'open' && t.matches('dialog,[popover]') && !t.matches(REGION)) raiseAll();
  }, true);

  // The last element outside a toast that had focus.
  var origin = null;
  document.addEventListener('focusin', function (e) {
    if (!e.target.closest(TOAST)) origin = e.target;
  });

  // Remove a toast. If it had focus, go back to the origin. After a keyboard
  // close, go to the next toast, else the one before. (Focus in a toast
  // pauses all timers, so a mouse close does not keep focus there.)
  function closeToast(el, keyboard) {
    var had = el.contains(document.activeElement);
    var all = Array.prototype.filter.call(
      (el.closest(REGION) || document).querySelectorAll(TOAST), function (t) {
        return t === el || getComputedStyle(t).visibility !== 'hidden';
      });
    var i = all.indexOf(el);
    var next = keyboard && (all[i + 1] || all[i - 1]);
    el.remove();
    var to = (next && next.querySelector(CLOSE)) || origin;
    if (had && to && to.isConnected) to.focus();
  }

  // `detail` is 0 for a click from Enter or Space.
  document.addEventListener('click', function (e) {
    var close = e.target.closest && e.target.closest(CLOSE);
    if (close && close.closest(TOAST)) closeToast(close.closest(TOAST), e.detail === 0);
  });

  document.addEventListener('animationend', function (e) {
    if (e.animationName === 'autumn-toast-out' && e.target.matches(TOAST)) closeToast(e.target);
  });

  // htmx keeps a copy of the page as HTML. The CSP blocks a style attribute
  // when htmx restores that copy. So remove the duration style before the
  // copy, and set it again after (a microtask runs after the copy).
  document.addEventListener('htmx:beforeHistorySave', function () {
    var toasts = document.querySelectorAll(TOAST);
    toasts.forEach(function (t) {
      t.style.removeProperty('--autumn-toast-duration');
      if (!t.getAttribute('style')) t.removeAttribute('style');
    });
    queueMicrotask(function () { toasts.forEach(initToast); });
  });

  // Run `fn` on `root` (when it matches) and on each match inside it.
  function each(root, selector, fn) {
    if (root.matches && root.matches(selector)) fn(root);
    root.querySelectorAll(selector).forEach(fn);
  }

  // Widget init first: it sets the roles and selection that roving reads.
  function initAll(root) {
    each(root, '[data-autumn-tabs]', initTabs);
    each(root, '[data-ac-value-id]', initAutocomplete);
    each(root, '[data-autumn-nav]', initNav);
    each(root, MENU, initMenu);
    root.dispatchEvent(new CustomEvent('autumn:init', { bubbles: true }));
    each(root, '[data-autumn-roving]', initRoving);
    each(root, REGION, initRegion);
    each(root, TOAST, initToast);
    // The theme is page state, so only a whole-page init syncs it.
    if (root === document) syncTheme();
  }

  // A parser-run copy boots on DOMContentLoaded. Deferred plugin scripts run
  // before it, so their `autumn:init` listeners see the page-load event. An
  // async or injected copy that runs after parsing boots at once.
  var booted = false;
  function boot() {
    if (booted) return;
    booted = true;
    initAll(document);
  }
  var me = document.currentScript;
  if (document.readyState === 'loading' ||
      (document.readyState === 'interactive' && me && me.defer && !me.async)) {
    document.addEventListener('DOMContentLoaded', boot);
    window.addEventListener('load', boot);
  } else {
    boot();
  }

  // Re-initialize after htmx swaps in new widgets. htmx fires afterSwap on
  // each new element. For an outerHTML swap, `detail.target` is the old,
  // removed element, so use `e.target`.
  document.addEventListener('htmx:afterSwap', function (e) {
    if (e.target && e.target.querySelectorAll) initAll(e.target);
  });
  // A history restore (hx-boost, Back) brings back nodes without listeners.
  document.addEventListener('htmx:historyRestore', function () { initAll(document); });

  // htmx puts the response's `class` back after afterSwap (its settle step).
  // That can drop the enhanced classes and move the tab highlight. Apply
  // them again from state that settle keeps: the init guard and aria-selected.
  document.addEventListener('htmx:afterSettle', function (e) {
    if (!e.target || !e.target.querySelectorAll) return;
    each(e.target, '[data-autumn-nav]', function (nav) {
      nav.classList.add('autumn-nav--enhanced');
    });
    each(e.target, '[data-autumn-tabs]', function (root) {
      var list = ownTablist(root);
      if (!list || !ready.has(list)) return;
      root.classList.add('autumn-tabs--enhanced');
      var tab = listItems(list).filter(function (t) {
        return t.getAttribute('aria-selected') === 'true';
      })[0];
      if (tab) selectTab(list, tab, false);
    });
  });
})();
