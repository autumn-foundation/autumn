#!/usr/bin/env python3
"""Seeded-mutation sweep for Autumn's wire contracts (issue #1755).

Applies each mutation to `mesh-catalog`, rebuilds `mesh-storefront`, and reports
how many wire-breaking changes turned the build red and how many compatible
changes were falsely rejected. Restores the file when it finishes.

Run from the workspace root:

    python3 examples/mesh-storefront/contract-sweep.py

Exits non-zero when a wire-breaking change builds green, or a compatible change
does not.
"""

import subprocess
import sys

LIB = 'examples/mesh-catalog/src/lib.rs'
BASE = open(LIB).read()

WIRE_BREAKING = [
 ("response field removed", [("    /// Price in cents.\n    pub price_cents: u32,\n}", "}"),
                             ("fn sample(id: String, name: String, price_cents: u32) -> Self {\n        Self {\n            id,\n            name,\n            price_cents,\n        }",
                              "fn sample(id: String, name: String, _price_cents: u32) -> Self {\n        Self { id, name }")]),
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
 ("a new endpoint added", [("/// The catalog service's routes.", '/// Count items.\n#[endpoint(service = "catalog")]\n#[get("/items")]\n#[public]\npub async fn count_items() -> AutumnResult<Json<Item>> {\n    Ok(Json(Item::sample("n".to_owned(), "n".to_owned(), 0)))\n}\n\n/// The catalog service\'s routes.')]),
]

def build():
    out = subprocess.run(['cargo','build','-p','mesh-storefront'], capture_output=True, text=True)
    text = out.stdout + out.stderr
    if 'wire contract broken' in text: return 'caught'
    if any(l.startswith('error') for l in text.splitlines()): return 'rustc-only'
    return 'green'

def apply(edits):
    s = BASE
    for old, new in edits:
        assert old in s, f'mutation text not found: {old!r}'
        s = s.replace(old, new, 1)
    open(LIB,'w').write(s)


results = []
for label, edits in WIRE_BREAKING:
    apply(edits); results.append((label, 'red', build()))
for label, edits in COMPATIBLE:
    apply(edits); results.append((label, 'green', build()))
open(LIB,'w').write(BASE)

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

# The acceptance criterion is a CALLER-NAMED error, not merely a red build, so
# the exit code gates on `caught` rather than on `red`.
sys.exit(1 if false_pos or len(caught) < len(breaking) else 0)
