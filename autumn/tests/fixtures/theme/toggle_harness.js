// Theme toggle cases (issue #3396) for the served autumn-widgets.js.
// Run: node toggle_harness.js <path to autumn-widgets.js>
// A small DOM stub stands in for the browser.
const fs = require('fs');
const src = fs.readFileSync(process.argv[2], 'utf8');
function run(initialCookie, action, hasToggle, initialTheme) {
  const listeners = {}, winListeners = {};
  const attrs = initialTheme ? { 'data-theme': initialTheme } : {};
  const root = { setAttribute: (k, v) => { attrs[k] = v; }, removeAttribute: k => { delete attrs[k]; } };
  const buttons = ['system','light','dark'].map(v => ({ name: 'theme', value: v, a: {}, setAttribute(k, x) { this.a[k] = x; } }));
  const form = { matches: s => s === 'form[data-autumn-theme-toggle]', getAttribute: () => action };
  let cookie = initialCookie;
  const document = {
    readyState: 'complete', documentElement: root,
    get cookie() { return cookie; }, set cookie(v) { cookie = v.split(';')[0]; },
    addEventListener: (t, f) => { (listeners[t] = listeners[t] || []).push(f); },
    // The runtime fires `autumn:init` on the document (#3400).
    dispatchEvent: () => true,
    querySelectorAll: s => s.includes('theme-toggle') && hasToggle ? buttons : [],
    querySelector: s => hasToggle && s.includes('theme-toggle')
      && (!s.includes('[action=') || s.includes('"' + action + '"')) ? form : null,
  };
  const window = { addEventListener: (t, f) => { (winListeners[t] = winListeners[t] || []).push(f); } };
  new Function('document', 'window', 'HTMLButtonElement', 'location', src)(document, window, { prototype: { command: 1 } }, { protocol: 'http:' });
  return {
    attrs, buttons, get cookie() { return cookie; },
    set cookie(v) { cookie = v; },
    pageshow(persisted) { (winListeners.pageshow || []).forEach(f => f({ persisted })); },
    click(v) { let prevented = false, stopped = false;
      listeners.submit.forEach(f => f({ target: form, submitter: buttons.find(b => b.value === v), preventDefault() { prevented = true; }, stopPropagation() { stopped = true; } }));
      return { prevented, stopped }; },
  };
}
const assert = require('assert');
let r = run('', '/_autumn/theme', true, 'dark');
assert.strictEqual(r.attrs['data-theme'], undefined, 'toggle page + no cookie -> system clears stale');
let ev = r.click('dark');
assert.ok(ev.prevented && ev.stopped); assert.strictEqual(r.attrs['data-theme'], 'dark'); assert.strictEqual(r.cookie, 'autumn.theme=dark');
assert.strictEqual(r.buttons[2].a['aria-pressed'], 'true');
ev = r.click('system'); assert.strictEqual(r.attrs['data-theme'], undefined); assert.strictEqual(r.cookie, 'autumn.theme=');
// A custom action stores the choice in the app (for example a profile).
// The script applies it and lets the form post. It does not own a cookie.
r = run('', '/settings/theme', true, 'dark');
assert.strictEqual(r.attrs['data-theme'], 'dark', 'profile-backed theme survives load');
ev = r.click('light');
assert.ok(!ev.prevented && !ev.stopped, 'custom action posts'); assert.strictEqual(r.attrs['data-theme'], 'light');
assert.strictEqual(r.cookie, '', 'custom action writes no cookie');
r = run('', '/settings/theme', true, 'dark');
r.pageshow(true); assert.strictEqual(r.attrs['data-theme'], 'dark', 'profile-backed theme survives restore');
r = run('', '/_autumn/theme', false, 'light'); assert.strictEqual(r.attrs['data-theme'], 'light', 'app pin stays without toggle');
r = run('x=1; autumn.theme=bad; autumn.theme=dark', '/_autumn/theme', true); assert.strictEqual(r.attrs['data-theme'], 'dark', 'first valid');
// A page without a toggle may pin its theme. A cookie from elsewhere must not change it.
r = run('autumn.theme=dark', '/_autumn/theme', false, 'light'); assert.strictEqual(r.attrs['data-theme'], 'light', 'pin survives a dark cookie');
// Back/forward cache: a restored page syncs with the cookie.
r = run('autumn.theme=light', '/_autumn/theme', true);
r.cookie = 'autumn.theme=dark';
r.pageshow(false); assert.strictEqual(r.attrs['data-theme'], 'light', 'normal load event is ignored');
r.pageshow(true); assert.strictEqual(r.attrs['data-theme'], 'dark', 'restored page syncs');
r = run('', '/_autumn/theme', false, 'light');
r.cookie = 'autumn.theme=dark';
r.pageshow(true); assert.strictEqual(r.attrs['data-theme'], 'light', 'restored pinned page stays');
console.log("all passed");
