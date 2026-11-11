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
  // `data-ac-multiple` turns on tags mode (#3410): each pick is a chip.

  // The text of an option. A "create" entry names its value in data-label.
  function labelOf(opt) {
    return opt.dataset.label || opt.textContent.trim();
  }

  // A key that this code must not use. An IME composition owns its keys
  // (Safari sends keyCode 229 with isComposing false).
  function modifier(e) {
    return e.defaultPrevented || e.altKey || e.ctrlKey || e.metaKey ||
      e.isComposing || e.keyCode === 229;
  }

  // Put `label` in a server text. Its `{label}` marks the place.
  function fill(text, label) {
    return text.split('{label}').join(label).trim();
  }

  // Tags mode: the chip list.
  function initChips(wrapper, queryInput, valueName) {
    var list = wrapper.querySelector('[data-ac-chips]');
    var tpl = wrapper.querySelector('template[data-ac-chip-template]');
    var status = wrapper.querySelector('[data-ac-status]');
    if (!list || !tpl || !tpl.content.firstElementChild) return null;
    var d = wrapper.dataset;
    function say(text, label) { if (status) status.textContent = fill(text, label); }

    function chips() {
      return Array.prototype.slice.call(list.querySelectorAll('[data-ac-chip]'));
    }
    function button(chip) { return chip.querySelector('[data-ac-chip-remove]'); }
    function has(value) {
      return chips().some(function (c) { return c.querySelector('input').value === value; });
    }
    // In RTL, ArrowRight goes back (to the chips) and ArrowLeft forward.
    function key(back) {
      var rtl = getComputedStyle(wrapper).direction === 'rtl';
      return back === rtl ? 'ArrowRight' : 'ArrowLeft';
    }

    // One tab stop: keep the current one, else the first chip.
    function rove() {
      var all = chips().map(button);
      var stop = all.filter(function (b) { return b.tabIndex === 0; })[0] || all[0];
      all.forEach(function (b) { b.tabIndex = b === stop ? 0 : -1; });
    }

    // A server chip submits only now. With JS off, the <select> submits.
    chips().forEach(function (c) { c.querySelector('input').disabled = false; });
    list.hidden = false;

    function add(value, label) {
      if (!value || has(value)) return;
      var chip = tpl.content.firstElementChild.cloneNode(true);
      chip.querySelector('[data-ac-chip-label]').textContent = label;
      var input = chip.querySelector('input');
      input.name = valueName;
      input.value = value;
      button(chip).setAttribute('aria-label', fill(d.acRemoveLabel || 'Remove {label}', label));
      list.appendChild(chip);
      rove();
      say(d.acAdded || '{label} added', label);
    }

    // A key press moves focus to the next chip, else the previous chip,
    // else the input. A pointer press moves focus to the input.
    function remove(chip, byKey) {
      var all = chips();
      var i = all.indexOf(chip);
      var next = byKey && (all[i + 1] || all[i - 1]);
      var label = chip.querySelector('[data-ac-chip-label]').textContent;
      chip.remove();
      rove();
      (next ? button(next) : queryInput).focus();
      say(d.acRemoved || '{label} removed', label);
    }

    queryInput.addEventListener('keydown', function (e) {
      if (modifier(e)) return;
      var all = chips();
      var start = queryInput.selectionStart === 0 && queryInput.selectionEnd === 0;
      if (!all.length || !start) return;
      // A held key repeats. Only a new press removes a chip.
      if (e.key === 'Backspace' && !queryInput.value && !e.repeat) {
        e.preventDefault();
        remove(all[all.length - 1], false);
      } else if (e.key === key(true)) {
        e.preventDefault();
        button(all[all.length - 1]).focus();
      }
    });

    list.addEventListener('keydown', function (e) {
      var btn = !modifier(e) && e.target.closest('[data-ac-chip-remove]');
      if (!btn) return;
      var chip = btn.closest('[data-ac-chip]');
      if (e.key === 'Delete' || e.key === 'Backspace') {
        e.preventDefault();
        // Focus moves to the next chip, so a held key must not remove it too.
        if (!e.repeat) remove(chip, true);
      } else if (e.key === key(false) && !chip.nextElementSibling) {
        e.preventDefault();
        queryInput.focus();
      }
    });

    // A key press on a button (Enter, Space) gives a click with detail 0.
    list.addEventListener('click', function (e) {
      var btn = e.target.closest('[data-ac-chip-remove]');
      if (btn) remove(btn.closest('[data-ac-chip]'), e.detail === 0);
    });

    return { has: has, add: add };
  }

  function initAutocomplete(wrapper) {
    if (!once(wrapper)) return;

    var queryInput = wrapper.querySelector('[data-ac-query]');
    var valueId = wrapper.dataset.acValueId;
    var valueName = wrapper.dataset.acValueName;
    var freeText = 'acFreeText' in wrapper.dataset;
    var listbox = wrapper.querySelector('[role="listbox"]');
    if (!queryInput || !valueName || !listbox) return;
    var tags = 'acMultiple' in wrapper.dataset && initChips(wrapper, queryInput, valueName);
    if (!tags && !document.getElementById(valueId || '')) return;

    // htmx parses a swap and a history snapshot with scripts off. Then the
    // <noscript> select is live, and it must not submit a second value.
    wrapper.querySelectorAll('noscript select').forEach(function (s) { s.disabled = true; });

    function getHidden() { return document.getElementById(valueId); }

    if (tags) {
      // Tags mode has no single hidden field.
    } else if (freeText) {
      // Free-text mode: name the field now, so it always submits.
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
      // Tags with free text: Enter with no active option adds the text.
      if (!tags || !freeText || e.key !== 'Enter' || modifier(e)) return;
      var text = queryInput.value.trim();
      if (!text || (!listbox.hidden && queryInput.getAttribute('aria-activedescendant'))) return;
      e.preventDefault();
      tags.add(text, text);
      queryInput.value = '';
      listbox.innerHTML = '';
    });

    // New content (an htmx swap, or a clear) shows the popup again, unless
    // the user dismissed it. An empty listbox stays in the accessibility
    // tree, so its live region still speaks; the CSS gives it no size.
    // Tags mode removes an option that is already a chip, so such a list
    // is empty too.
    function sync() {
      var shown = 0;
      listbox.querySelectorAll('[role="option"]').forEach(function (o) {
        if (tags && tags.has(o.dataset.value || '')) {
          o.remove();
          return;
        }
        o.tabIndex = -1;
        if (!o.hidden) shown++;
      });
      listbox.hidden = dismissed;
      queryInput.setAttribute('aria-expanded', !listbox.hidden && shown ? 'true' : 'false');
      clearActive(queryInput, listbox);
    }
    // `subtree` also sees a morph swap that keeps the option count.
    new MutationObserver(sync).observe(listbox, { childList: true, subtree: true });
    sync();

    function selectOption(opt) {
      if (tags) {
        tags.add(opt.dataset.value || '', labelOf(opt));
        queryInput.value = '';
      } else {
        var hidden = getHidden();
        hidden.name = valueName;
        hidden.value = opt.dataset.value || '';
        queryInput.value = labelOf(opt);
      }
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

    // Free-text tags: a submit adds the typed text first.
    if (tags && freeText) {
      pending.set(wrapper, function () {
        var text = queryInput.value.trim();
        if (text) tags.add(text, text);
        queryInput.value = '';
      });
    }

    queryInput.addEventListener('input', function () {
      dismissed = false;
      // The highlight belongs to the old query. Enter must not commit it.
      clearActive(queryInput, listbox);
      var minLen = parseInt(queryInput.dataset.acMinLength || '0', 10);
      if (tags) {
        // Each chip has its own input. The query is not a value.
      } else if (freeText) {
        // Free-text mode: the typed text is the value.
        getHidden().name = valueName;
        getHidden().value = queryInput.value;
      } else {
        // ID mode: an edit makes the pick stale. The field gets its name
        // again only when an option is selected.
        getHidden().name = '';
        getHidden().value = '';
      }
      // Clear stale options when the query drops below the minimum length.
      if (minLen > 0 && queryInput.value.length < minLen) {
        listbox.innerHTML = '';
      }
    });
  }

  // The capture listener runs before htmx reads the form.
  var pending = new WeakMap();
  document.addEventListener('submit', function (e) {
    var form = e.target;
    if (!form || !form.querySelectorAll) return;
    form.querySelectorAll('[data-ac-multiple]').forEach(function (w) {
      var add = pending.get(w);
      if (add) add();
    });
  }, true);

  // The core fires `autumn:init` on each new root. This part is first in
  // the bundle, so the listener exists before the core boots.
  var ROOTS = '[data-ac-value-id],[data-ac-multiple]';
  document.addEventListener('autumn:init', function (e) {
    var root = e.target;
    if (root.matches && root.matches(ROOTS)) initAutocomplete(root);
    if (root.querySelectorAll) root.querySelectorAll(ROOTS).forEach(initAutocomplete);
  });
})();
