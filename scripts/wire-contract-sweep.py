#!/usr/bin/env python3
"""Seeded-mutation sweep for Autumn's wire contracts (issue #1755).

Part 1 applies each mutation to `mesh-catalog`, rebuilds `mesh-storefront`, and
reports how many wire-breaking changes turned the build red and how many
compatible changes were falsely rejected.

Part 2 is the two-release scenario. Release 1 is the committed bundle
`examples/mesh-storefront/contracts/release-1.json`. Each case edits both
services into a release 2 and runs `autumn contracts check` against release 1.
The exit code and the named deploy order must match. Part 2 also exports the
unchanged tree twice and requires the same bytes as the committed bundle.

The script restores every file when it finishes.

Run from the workspace root:

    python3 scripts/wire-contract-sweep.py

Set AUTUMN_BIN to use a built `autumn` binary. Otherwise the script builds one.
Exits non-zero when any case does not give the expected result.
"""

import os
import subprocess
import sys
import tempfile

LIB = 'examples/mesh-catalog/src/lib.rs'
MAIN = 'examples/mesh-storefront/src/main.rs'
RELEASE_1 = 'examples/mesh-storefront/contracts/release-1.json'
BASE = open(LIB).read()
BASE_MAIN = open(MAIN).read()
PACKAGES = ['-p', 'mesh-catalog', '-p', 'mesh-storefront']

WIRE_BREAKING = [
 ("response field removed", [("    /// Price in cents.\n    pub price_cents: u32,\n}", "}"),
                             ("fn sample(id: String, name: String, price_cents: u32) -> Self {\n        Self {\n            subtitle: None,\n            id,\n            name,\n            price_cents,\n        }",
                              "fn sample(id: String, name: String, _price_cents: u32) -> Self {\n        Self { subtitle: None, id, name }")]),
 ("response field renamed", [("    /// Display name.\n    pub name: String,\n    /// Price in cents.", "    /// Display name.\n    pub title: String,\n    /// Price in cents."),
                             ("            id,\n            name,\n            price_cents,", "            id,\n            title: name,\n            price_cents,")]),
 ("response field serde(skip)", [("    /// Display name.\n    pub name: String,\n    /// Price in cents.", "    /// Display name.\n    #[serde(skip)]\n    pub name: String,\n    /// Price in cents.")]),
 ("response field skip_serializing", [("    /// Display name.\n    pub name: String,\n    /// Price in cents.", "    /// Display name.\n    #[serde(skip_serializing)]\n    pub name: String,\n    /// Price in cents.")]),
 ("response field renamed via rename_all split", [("    /// Display name.\n    pub name: String,\n    /// Price in cents.", "    /// Display name.\n    pub label: String,\n    /// Price in cents."),
                             ("            id,\n            name,\n            price_cents,", "            id,\n            label: name,\n            price_cents,")]),
 ("new required request field the type may omit", [("    /// Optional note. Absent from a request body is fine.", "    /// New, required, and dropped from the body when empty.\n    #[serde(skip_serializing_if = \"String::is_empty\")]\n    pub sku: String,\n    /// Optional note. Absent from a request body is fine.")]),
 ("request field renamed", [("    /// Price in cents. Required.\n    pub price_cents: u32,", "    /// Price in cents. Required.\n    pub cost_cents: u32,"),
                            ("        body.0.price_cents,", "        body.0.cost_cents,")]),
 ("request field skip_deserializing", [("    /// Price in cents. Required.\n    pub price_cents: u32,", "    /// Price in cents. Required.\n    #[serde(skip_deserializing)]\n    pub price_cents: u32,")]),
 ("request field serde(skip)", [("    /// Price in cents. Required.\n    pub price_cents: u32,", "    /// Price in cents. Required.\n    #[serde(skip)]\n    pub price_cents: u32,")]),
 ("optional request field became required and omittable", [("    /// Optional note. Absent from a request body is fine.\n    #[serde(default)]\n    pub note: Option<String>,", "    /// Now required, and dropped when empty.\n    #[serde(skip_serializing_if = \"String::is_empty\")]\n    pub note: String,")]),
 ("route path parameter renamed", [('#[get("/items/{id}")]', '#[get("/items/{item_id}")]')]),
 ("route path gained a parameter", [('#[get("/items/{id}")]', '#[get("/tenants/{tenant}/items/{id}")]')]),
 ("event field the listener reads skip_serializing", [("    /// New price in cents.\n    pub price_cents: u32,", "    /// New price in cents.\n    #[serde(skip_serializing)]\n    pub price_cents: u32,")]),
 ("event field the listener reads skip_deserializing", [("    /// New price in cents.\n    pub price_cents: u32,", "    /// New price in cents.\n    #[serde(skip_deserializing)]\n    pub price_cents: u32,")]),
]

COMPATIBLE = [
 ("new optional response field", [("    /// Price in cents.\n    pub price_cents: u32,\n}", "    /// Price in cents.\n    pub price_cents: u32,\n    /// Added later.\n    pub badge: Option<String>,\n}"),
                                  ("            price_cents,\n        }", "            price_cents,\n            badge: None,\n        }")]),
 # A required field gaining `skip_serializing_if` is only a break for a call
 # site that does not set it. This one does, so it must stay green.
 ("required request field gains skip_serializing_if but is set anyway", [
    ("    /// Price in cents. Required.\n    pub price_cents: u32,",
     "    /// Price in cents. Required.\n    #[serde(skip_serializing_if = \"is_zero\")]\n    pub price_cents: u32,"),
    ("/// Fetch one item.", "fn is_zero(value: &u32) -> bool {\n    *value == 0\n}\n\n/// Fetch one item."),
 ]),
 ("new defaulted request field", [("    /// Optional note. Absent from a request body is fine.", "    /// Added later, defaulted.\n    #[serde(default)]\n    pub tier: String,\n    /// Optional note. Absent from a request body is fine.")]),
 ("new required request field the type always sends", [("    /// Optional note. Absent from a request body is fine.", "    /// Added later, required, and always on the wire — both ends share the\n    /// type, so the body carries a default value for it.\n    pub sku: String,\n    /// Optional note. Absent from a request body is fine.")]),
 ("new optional request field", [("    /// Optional note. Absent from a request body is fine.", "    /// Added later, optional.\n    pub coupon: Option<String>,\n    /// Optional note. Absent from a request body is fine.")]),
 ("serde rename on a response field", [("    /// Display name.\n    pub name: String,\n    /// Price in cents.", '    /// Display name.\n    #[serde(rename = "displayName")]\n    pub name: String,\n    /// Price in cents.')]),
 ("container rename_all", [("pub struct Item {", "pub struct Item {")] + [("#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, WireShape)]\npub struct Item {", '#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, WireShape)]\n#[serde(rename_all = "camelCase")]\npub struct Item {')]),
 ("response field order swapped", [("    /// Display name.\n    pub name: String,\n    /// Price in cents.\n    pub price_cents: u32,", "    /// Price in cents.\n    pub price_cents: u32,\n    /// Display name.\n    pub name: String,")]),
 ("doc comment reworded", [("/// A catalog item, as the service puts it on the wire.", "/// A catalog item. Reworded.")]),
 ("skip_serializing_if on a new response field", [("    /// Price in cents.\n    pub price_cents: u32,\n}", '    /// Price in cents.\n    pub price_cents: u32,\n    /// Sometimes absent.\n    #[serde(skip_serializing_if = "Option::is_none")]\n    pub badge: Option<String>,\n}'),
                                                  ("            price_cents,\n        }", "            price_cents,\n            badge: None,\n        }")]),
 ("response field type widened in place", [("    /// Stable item identifier.\n    pub id: String,", "    /// Stable item identifier.\n    pub id: std::string::String,")]),
 ("new event field", [("    /// New price in cents.\n    pub price_cents: u32,", "    /// New price in cents.\n    pub price_cents: u32,\n    /// Currency code. Added later.\n    pub currency: Option<String>,")]),
 ("event renamed on the wire, listener reads the Rust name", [("    /// New price in cents.\n    pub price_cents: u32,", "    /// New price in cents.\n    #[serde(rename = \"priceCents\")]\n    pub price_cents: u32,")]),
 ("a new endpoint added", [("/// The catalog service's routes.", '/// Count items.\n#[endpoint(service = "catalog")]\n#[get("/items")]\n#[public]\npub async fn count_items() -> AutumnResult<Json<Item>> {\n    Ok(Json(Item::sample("n".to_owned(), "n".to_owned(), 0)))\n}\n\n/// The catalog service\'s routes.')]),
]

# Release 2 cases: (label, [(file, old, new)], exit code, text the report
# must hold). Exit 0 is safe in any order, 1 is safe in no order, 3 is safe
# only in the order the report names.
DEPLOY_CALLERS_FIRST = 'Deploy mesh-storefront before catalog'
DEPLOY_CALLEES_FIRST = 'Deploy catalog before mesh-storefront'
# An event's crate has endpoints, so the order names its service.
EVENT_CONSUMERS_FIRST = DEPLOY_CALLERS_FIRST
EVENT_PRODUCERS_FIRST = DEPLOY_CALLEES_FIRST
EVENT = "    /// New price in cents.\n    pub price_cents: u32,\n}"
LISTENER_READ = "    let _price = (event.id, event.price_cents);\n"
CROSS_RELEASE = [
 ("unchanged release", [], 0, 'safe in any deploy order'),
 ("unread optional response field removed", [
    (LIB, "    /// Shelf label. Optional, and no caller reads it, so a later release may\n    /// remove it.\n    pub subtitle: Option<String>,\n", ""),
    (LIB, "            subtitle: None,\n", ""),
 ], 0, 'safe in any deploy order'),
 ("read response field removed, storefront stops reading it", [
    (LIB, "    /// Price in cents.\n    pub price_cents: u32,\n}", "}"),
    (LIB, "            name,\n            price_cents,\n        }", "            name,\n        }"),
    (LIB, "fn sample(id: String, name: String, price_cents: u32)", "fn sample(id: String, name: String, _price_cents: u32)"),
    (MAIN, '        p { "$" (item.price_cents / 100) }\n', ""),
 ], 3, DEPLOY_CALLERS_FIRST),
 ("new required request field", [
    (LIB, "    /// Optional note. Absent from a request body is fine.", "    /// Stock code. Required.\n    pub sku: String,\n    /// Optional note. Absent from a request body is fine."),
 ], 3, DEPLOY_CALLERS_FIRST),
 ("new optional request field", [
    (LIB, "    /// Optional note. Absent from a request body is fine.", "    /// Coupon code. Optional.\n    pub coupon: Option<String>,\n    /// Optional note. Absent from a request body is fine."),
 ], 0, 'safe in any deploy order'),
 ("new response field the new storefront reads", [
    (LIB, "    /// Price in cents.\n    pub price_cents: u32,\n}", "    /// Price in cents.\n    pub price_cents: u32,\n    /// Promotion badge. Optional.\n    pub badge: Option<String>,\n}"),
    (LIB, "            price_cents,\n        }", "            price_cents,\n            badge: None,\n        }"),
    (MAIN, '        h1 { (item.name) }\n', '        h1 { (item.name) }\n        p { (item.badge.clone().unwrap_or_default()) }\n'),
 ], 3, DEPLOY_CALLEES_FIRST),
 ("path changed", [
    (LIB, '#[get("/items/{id}")]', '#[get("/catalog/items/{id}")]'),
 ], 1, 'not safe in any deploy order'),
 ("path changed, old route kept", [
    (LIB, '#[get("/items/{id}")]', '#[get("/catalog/items/{id}")]'),
    (LIB, "/// The catalog service's routes.", '/// The old route of `get_item`, kept for deployed callers.\n#[endpoint(service = "catalog")]\n#[get("/items/{id}")]\n#[public]\npub async fn get_item_legacy(id: Path<String>) -> AutumnResult<Json<Item>> {\n    Ok(Json(Item::sample(id.to_string(), format!(\"Item {}\", *id), 1299)))\n}\n\n/// The catalog service\'s routes.'),
    (LIB, "routes![get_item, create_item]", "routes![get_item, get_item_legacy, create_item]"),
 ], 3, DEPLOY_CALLEES_FIRST),
 ("method changed", [
    (LIB, '#[get("/items/{id}")]', '#[post("/items/{id}")]'),
 ], 1, 'not safe in any deploy order'),
 ("event field a listener reads removed, listener stops reading it", [
    (LIB, EVENT, "}"),
    (MAIN, LISTENER_READ, "    let _price = event.id;\n"),
 ], 3, EVENT_CONSUMERS_FIRST),
 ("new event field the new listener reads", [
    (LIB, EVENT, "    /// New price in cents.\n    pub price_cents: u32,\n    /// Currency code. Required.\n    pub currency: String,\n}"),
    (MAIN, LISTENER_READ, "    let _price = (event.id, event.price_cents, event.currency);\n"),
 ], 3, EVENT_PRODUCERS_FIRST),
 ("new optional event field", [
    (LIB, EVENT, "    /// New price in cents.\n    pub price_cents: u32,\n    /// Currency code. Optional.\n    pub currency: Option<String>,\n}"),
 ], 0, 'safe in any deploy order'),
 # The new listener reads only v2, and the deployed catalog sends only v1:
 # the catalog ships first, or v1 events are lost.
 ("event v2 beside v1, listener moves to v2", [
    (LIB, "impl Item {", "/// Version 2: the price carries its currency.\n#[event(name = \"catalog.item_repriced\", version = 2)]\npub struct ItemRepricedV2 {\n    /// Stable item identifier.\n    pub id: String,\n    /// New price in minor units.\n    pub amount: u32,\n    /// Currency code.\n    pub currency: String,\n}\n\nimpl Item {"),
    (MAIN, "#[listener(mesh_catalog::ItemRepriced)]", "#[listener(mesh_catalog::ItemRepricedV2)]"),
    (MAIN, "event: mesh_catalog::ItemRepriced)", "event: mesh_catalog::ItemRepricedV2)"),
    (MAIN, LISTENER_READ, "    let _price = (event.id, event.amount, event.currency);\n"),
 ], 3, EVENT_PRODUCERS_FIRST),
 ("path parameter renamed", [
    (LIB, '#[get("/items/{id}")]\n#[public]\npub async fn get_item(id: Path<String>)', '#[get("/items/{item_id}")]\n#[public]\npub async fn get_item(item_id: Path<String>)'),
    (LIB, "        id.to_string(),\n        format!(\"Item {}\", *id),", "        item_id.to_string(),\n        format!(\"Item {}\", *item_id),"),
    (MAIN, "mesh_catalog::get_item_endpoint(id),", "mesh_catalog::get_item_endpoint(item_id),"),
 ], 0, 'safe in any deploy order'),
]

def build():
    out = subprocess.run(['cargo','build','-p','mesh-storefront'], capture_output=True, text=True)
    text = out.stdout + out.stderr
    if 'wire contract broken' in text or 'event contract broken' in text: return 'caught'
    if any(l.startswith('error') for l in text.splitlines()): return 'rustc-only'
    return 'green'

def apply(edits):
    s = BASE
    for old, new in edits:
        assert old in s, f'mutation text not found: {old!r}'
        s = s.replace(old, new, 1)
    open(LIB,'w').write(s)

def apply_release(edits):
    files = {LIB: BASE, MAIN: BASE_MAIN}
    for path, old, new in edits:
        assert old in files[path], f'mutation text not found in {path}: {old!r}'
        files[path] = files[path].replace(old, new, 1)
    for path, text in files.items():
        open(path, 'w').write(text)

def restore():
    open(LIB, 'w').write(BASE)
    open(MAIN, 'w').write(BASE_MAIN)

def autumn_bin():
    explicit = os.environ.get('AUTUMN_BIN')
    if explicit:
        return explicit
    subprocess.run(['cargo', 'build', '-q', '-p', 'autumn-cli'], check=True)
    return os.path.join('target', 'debug', 'autumn')

def contracts(binary, *args):
    out = subprocess.run([binary, 'contracts', *args, *PACKAGES], capture_output=True, text=True)
    return out.returncode, out.stdout, out.stderr

try:
    results = []
    for label, edits in WIRE_BREAKING:
        apply(edits); results.append((label, 'red', build()))
    for label, edits in COMPATIBLE:
        apply(edits); results.append((label, 'green', build()))
    restore()

    binary = autumn_bin()
    stable = []
    with tempfile.TemporaryDirectory() as scratch:
        for n in (1, 2):
            out = os.path.join(scratch, f'export-{n}.json')
            code, _, err = contracts(binary, 'export', '--out', out)
            assert code == 0, err
            stable.append(open(out, 'rb').read())
    stable_ok = stable[0] == stable[1] == open(RELEASE_1, 'rb').read()

    cross = []
    for label, edits, want_code, want_text in CROSS_RELEASE:
        apply_release(edits)
        code, out, err = contracts(binary, 'check', '--against', RELEASE_1)
        ok = code == want_code and want_text in out
        cross.append((label, want_code, code, ok, out + err))
    restore()
finally:
    restore()

breaking = [r for r in results if r[1]=='red']
compatible = [r for r in results if r[1]=='green']
caught = [r for r in breaking if r[2]=='caught']
red = [r for r in breaking if r[2]!='green']
false_pos = [r for r in compatible if r[2]!='green']
for label, want, got in results:
    print(f'{label:<46} want={want:<6} got={got}')
print('---')
print(f'wire-breaking: {len(breaking)}  turned the build red: {len(red)} ({100*len(red)//len(breaking)}%)  '
      f'named by the contract check: {len(caught)} ({100*len(caught)//len(breaking)}%)')
print(f'compatible:    {len(compatible)}  falsely rejected: {len(false_pos)}')
print('--- two releases: release 2 checked against release 1')
for label, want, got, ok, text in cross:
    print(f'{label:<56} want=exit {want} got=exit {got} {"ok" if ok else "WRONG"}')
    if not ok:
        print(text)
cross_wrong = [c for c in cross if not c[3]]
print(f'bundle byte-stable across two exports and equal to {RELEASE_1}: {stable_ok}')

# The acceptance criterion is a CALLER-NAMED error, not merely a red build, so
# the exit code gates on `caught` rather than on `red`.
sys.exit(1 if false_pos or len(caught) < len(breaking) or cross_wrong or not stable_ok else 0)
