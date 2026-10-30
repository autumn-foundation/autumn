// The swatch widget pack (#3400). The keyboard core in autumn-widgets.js
// moves focus with the arrow keys. This file only toggles aria-pressed.
(function () {
  'use strict';

  // Set up each toolbar one time. `autumn:init` fires on the page and on
  // each htmx swap, before roving set-up. The target can be the toolbar.
  var ready = new WeakSet();
  function init(bar) {
    if (ready.has(bar)) return;
    ready.add(bar);
    bar.setAttribute('data-swatch-ready', 'true');
  }

  document.addEventListener('autumn:init', function (e) {
    var root = e.target;
    if (!root.querySelectorAll) return;
    if (root.matches && root.matches('.autumn-swatch-toolbar')) init(root);
    root.querySelectorAll('.autumn-swatch-toolbar').forEach(init);
  });

  document.addEventListener('click', function (e) {
    var button = e.target.closest && e.target.closest('.autumn-swatch-toggle');
    if (!button) return;
    var pressed = button.getAttribute('aria-pressed') === 'true';
    button.setAttribute('aria-pressed', pressed ? 'false' : 'true');
  });
})();
