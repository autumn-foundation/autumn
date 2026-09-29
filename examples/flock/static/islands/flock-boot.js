// ES-module loader for the Yew "literary boids" island.
//
// Served from /static/islands/ (script-src 'self'), so no inline script and no
// CSP nonce are needed. maud references it with:
//   <script type="module" src=(asset_url("islands/flock-boot.js")) defer></script>
//
// `wasm-bindgen --target web` emits ./autumn_island_flock.js (the ES-module
// glue) alongside ./autumn_island_flock_bg.wasm. We import the glue, run its
// default `init()` to fetch + instantiate the wasm (this is the step the CSP's
// 'wasm-unsafe-eval' token authorizes), then call the exported `mount(el, count)`.
// The initial boid count travels via a data-* attribute on the mount element.
import init, { mount } from './autumn_island_flock.js';

await init();

const el = document.querySelector('[data-autumn-island="flock"]');
if (el) {
  mount(el, Number(el.dataset.count ?? '120'));
}
