//! The subset check: does one call site still agree with the endpoint it calls?
//!
//! A call site's *read-set* is every response field it names, and its
//! *write-set* is every request field it sets. The contract holds when the
//! read-set is a subset of what the callee produces, the write-set is a subset
//! of what the callee accepts, and every field the callee requires is supplied.
//!
//! This is the diagnostic half of the check. The authoritative half is the
//! const assertion `#[contract_checked]` emits against the callee's own const
//! field table, which rustc tracks across crates and which therefore cannot go
//! stale. This runs only when the endpoint's JSON artifact is on disk, and only
//! to say which field is at fault — a const-eval panic message must be a
//! literal, so it cannot name a field the caller macro did not already know.

use crate::wire::ir::ResolvedEndpoint;

/// One call site, as `#[contract_checked]` read it out of the caller's source.
#[derive(Debug, Clone)]
pub struct CallSite {
    /// The enclosing function's name.
    pub caller: String,
    /// The call as written, for the diagnostic. The compiler supplies the
    /// file and line from the span the assertion carries.
    pub snippet: String,
    /// The client method invoked — the endpoint's name.
    pub method: String,
    /// Response fields the caller names, by Rust identifier.
    pub reads: Vec<String>,
    /// Request fields the caller sets, or `None` when the request is not an
    /// inline struct literal and the write-set is therefore unknowable.
    pub writes: Option<Vec<String>>,
    /// Whether the request literal lists every field (no `..rest`). When it
    /// does, rustc already forces every field to be present, so the
    /// missing-required check has nothing left to catch.
    pub writes_exhaustive: bool,
}

/// What a call site and an endpoint disagree about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViolationKind {
    /// The caller reads a response field the service does not produce.
    ResponseFieldMissing(String),
    /// The caller sets a request field the service does not accept.
    RequestFieldUnknown(String),
    /// The caller omits a request field the service requires.
    RequestFieldMissing(String),
}

/// A single contract breach, with everything a diagnostic needs to name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// What broke.
    pub kind: ViolationKind,
    /// The offending call as written.
    pub snippet: String,
    /// The calling function's name.
    pub caller: String,
    /// `service.name` of the endpoint called.
    pub endpoint: String,
    /// `METHOD /path` of the endpoint called.
    pub route: String,
}

impl Violation {
    /// The compiler diagnostic for this breach.
    #[must_use]
    pub fn message(&self) -> String {
        let Self {
            snippet,
            caller,
            endpoint,
            route,
            ..
        } = self;
        let detail = match &self.kind {
            ViolationKind::ResponseFieldMissing(f) => format!(
                "reads response field `{f}`, which endpoint `{endpoint}` ({route}) no longer produces"
            ),
            ViolationKind::RequestFieldUnknown(f) => format!(
                "sets request field `{f}`, which endpoint `{endpoint}` ({route}) does not accept"
            ),
            ViolationKind::RequestFieldMissing(f) => {
                format!("omits request field `{f}`, which endpoint `{endpoint}` ({route}) requires")
            }
        };
        format!("wire contract broken in `{caller}` at `{snippet}`: {detail}")
    }
}

/// Compare one call site against the endpoint's descriptor.
#[must_use]
pub fn check(site: &CallSite, endpoint: &ResolvedEndpoint) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut report = |kind| {
        out.push(Violation {
            kind,
            snippet: site.snippet.clone(),
            caller: site.caller.clone(),
            endpoint: endpoint.endpoint.id(),
            route: format!("{} {}", endpoint.endpoint.method, endpoint.endpoint.path),
        });
    };

    for read in &site.reads {
        if endpoint.response.produced(read).is_none() {
            report(ViolationKind::ResponseFieldMissing(read.clone()));
        }
    }

    let Some(writes) = &site.writes else {
        return out;
    };
    for write in writes {
        if endpoint.request.accepted(write).is_none() {
            report(ViolationKind::RequestFieldUnknown(write.clone()));
        }
    }
    // An exhaustive literal names every field of the struct, so rustc has
    // already rejected an omission. Only a `..rest` initializer can hide one.
    if !site.writes_exhaustive {
        for required in endpoint.request.deserialized.iter().filter(|f| f.required) {
            if !writes.contains(&required.rust_name) {
                report(ViolationKind::RequestFieldMissing(
                    required.rust_name.clone(),
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::ir::{EndpointDescriptor, WireFieldDescriptor, WireTypeDescriptor};

    fn field(rust_name: &str, required: bool) -> WireFieldDescriptor {
        WireFieldDescriptor {
            rust_name: rust_name.to_owned(),
            wire_name: rust_name.to_owned(),
            ty: "String".to_owned(),
            required,
        }
    }

    /// `catalog.get_item` — takes `NewItem { name, price_cents? }`, returns
    /// `Item { id, name, price_cents }`.
    fn endpoint() -> ResolvedEndpoint {
        ResolvedEndpoint {
            endpoint: EndpointDescriptor {
                service: "catalog".to_owned(),
                name: "get_item".to_owned(),
                endpoint_ident: "get_item_endpoint".to_owned(),
                krate: "catalog".to_owned(),
                method: "POST".to_owned(),
                path: "/items".to_owned(),
                request_type: "NewItem".to_owned(),
                response_type: "Item".to_owned(),
            },
            request: WireTypeDescriptor {
                name: "NewItem".to_owned(),
                serialized: vec![field("name", true), field("price_cents", false)],
                deserialized: vec![field("name", true), field("price_cents", false)],
            },
            response: WireTypeDescriptor {
                name: "Item".to_owned(),
                serialized: vec![
                    field("id", true),
                    field("name", true),
                    field("price_cents", true),
                ],
                deserialized: vec![
                    field("id", true),
                    field("name", true),
                    field("price_cents", true),
                ],
            },
        }
    }

    fn site(reads: &[&str], writes: Option<&[&str]>, exhaustive: bool) -> CallSite {
        CallSite {
            caller: "show_item".to_owned(),
            snippet: "catalog.get_item(req)".to_owned(),
            method: "get_item".to_owned(),
            reads: reads.iter().map(|s| (*s).to_owned()).collect(),
            writes: writes.map(|w| w.iter().map(|s| (*s).to_owned()).collect()),
            writes_exhaustive: exhaustive,
        }
    }

    #[test]
    fn agreeing_call_site_has_no_violations() {
        let v = check(&site(&["id", "name"], Some(&["name"]), false), &endpoint());
        assert!(
            v.is_empty(),
            "compatible call site must not be rejected: {v:?}"
        );
    }

    #[test]
    fn reading_a_field_the_service_does_not_produce_is_a_violation() {
        let v = check(&site(&["id", "sku"], Some(&["name"]), false), &endpoint());
        assert_eq!(
            v.iter().map(|v| v.kind.clone()).collect::<Vec<_>>(),
            vec![ViolationKind::ResponseFieldMissing("sku".to_owned())]
        );
    }

    #[test]
    fn setting_a_field_the_service_does_not_accept_is_a_violation() {
        let v = check(
            &site(&["id"], Some(&["name", "colour"]), false),
            &endpoint(),
        );
        assert_eq!(
            v.iter().map(|v| v.kind.clone()).collect::<Vec<_>>(),
            vec![ViolationKind::RequestFieldUnknown("colour".to_owned())]
        );
    }

    /// The `..Default::default()` case: compiles, 400s at runtime. This is the
    /// break rustc cannot see, so it is the one the check exists for.
    #[test]
    fn omitting_a_required_request_field_behind_a_rest_initializer_is_a_violation() {
        let v = check(&site(&["id"], Some(&["price_cents"]), false), &endpoint());
        assert_eq!(
            v.iter().map(|v| v.kind.clone()).collect::<Vec<_>>(),
            vec![ViolationKind::RequestFieldMissing("name".to_owned())]
        );
    }

    /// An exhaustive literal has no `..rest`, so rustc already forces every
    /// field to be present. Reporting it again would be a false positive.
    #[test]
    fn exhaustive_literal_is_not_checked_for_missing_required_fields() {
        let v = check(&site(&["id"], Some(&["price_cents"]), true), &endpoint());
        assert!(v.is_empty(), "rustc already covers this: {v:?}");
    }

    #[test]
    fn unknown_write_set_checks_reads_only() {
        let v = check(&site(&["sku"], None, false), &endpoint());
        assert_eq!(
            v.iter().map(|v| v.kind.clone()).collect::<Vec<_>>(),
            vec![ViolationKind::ResponseFieldMissing("sku".to_owned())]
        );
    }

    #[test]
    fn message_names_the_call_site_the_endpoint_and_the_field() {
        let v = check(&site(&["sku"], None, false), &endpoint());
        let msg = v[0].message();
        assert!(msg.contains("catalog.get_item(req)"), "{msg}");
        assert!(msg.contains("show_item"), "{msg}");
        assert!(msg.contains("catalog.get_item"), "{msg}");
        assert!(msg.contains("`sku`"), "{msg}");
    }

    /// The success metric from issue #1755, at checker speed.
    ///
    /// `examples/mesh-storefront/contract-sweep.py` proves the same thing
    /// through real `cargo build`s; this proves it in microseconds, so a
    /// regression in the rules themselves shows up in the ordinary test lane.
    /// Each case mutates the descriptor the way a callee edit would, against
    /// one fixed call site: reads `id` and `name`, sets `name` through a
    /// `..rest` initializer.
    #[test]
    // A flat table of mutations, one line per guarantee. Splitting it would
    // separate a case from the list it is measured against.
    #[allow(clippy::too_many_lines)]
    fn every_wire_breaking_mutation_is_caught_and_no_compatible_one_is() {
        fn drop_field(mut e: ResolvedEndpoint, name: &str) -> ResolvedEndpoint {
            e.response.serialized.retain(|f| f.rust_name != name);
            e.request.deserialized.retain(|f| f.rust_name != name);
            e
        }
        fn rename(mut e: ResolvedEndpoint, from: &str, to: &str) -> ResolvedEndpoint {
            for f in e
                .response
                .serialized
                .iter_mut()
                .chain(e.request.deserialized.iter_mut())
            {
                if f.rust_name == from {
                    f.rust_name = to.to_owned();
                    f.wire_name = to.to_owned();
                }
            }
            e
        }

        let site = site(&["id", "name"], Some(&["name"]), false);

        let breaking: Vec<(&str, ResolvedEndpoint)> = vec![
            ("response field removed", {
                let mut e = endpoint();
                e.response.serialized.retain(|f| f.rust_name != "id");
                e
            }),
            ("response field renamed", {
                let mut e = endpoint();
                e.response.serialized[0].rust_name = "item_id".to_owned();
                e
            }),
            (
                "response field serde-skipped in both directions",
                drop_field(endpoint(), "name"),
            ),
            ("response field skip_serializing", {
                let mut e = endpoint();
                e.response.serialized.retain(|f| f.rust_name != "name");
                e
            }),
            ("request field removed", {
                let mut e = endpoint();
                e.request.deserialized.retain(|f| f.rust_name != "name");
                e
            }),
            ("request field renamed", rename(endpoint(), "name", "title")),
            ("request field skip_deserializing", {
                let mut e = endpoint();
                e.request.deserialized.retain(|f| f.rust_name != "name");
                e
            }),
            ("optional request field became required", {
                let mut e = endpoint();
                e.request.deserialized[1].required = true;
                e
            }),
            ("new required request field", {
                let mut e = endpoint();
                e.request.deserialized.push(field("sku", true));
                e
            }),
            ("whole response type replaced", {
                let mut e = endpoint();
                e.response.serialized = vec![field("sku", true)];
                e
            }),
        ];

        let compatible: Vec<(&str, ResolvedEndpoint)> = vec![
            ("new optional response field", {
                let mut e = endpoint();
                e.response.serialized.push(field("badge", false));
                e
            }),
            ("new required response field", {
                let mut e = endpoint();
                e.response.serialized.push(field("badge", true));
                e
            }),
            ("new optional request field", {
                let mut e = endpoint();
                e.request.deserialized.push(field("coupon", false));
                e
            }),
            ("required request field became optional", {
                let mut e = endpoint();
                e.request.deserialized[0].required = false;
                e
            }),
            ("response field renamed on the wire only", {
                let mut e = endpoint();
                e.response.serialized[0].wire_name = "itemId".to_owned();
                e
            }),
            ("response fields reordered", {
                let mut e = endpoint();
                e.response.serialized.reverse();
                e
            }),
            ("response field type widened", {
                let mut e = endpoint();
                e.response.serialized[0].ty = "std::string::String".to_owned();
                e
            }),
            ("unread response field removed", {
                let mut e = endpoint();
                e.response
                    .serialized
                    .retain(|f| f.rust_name != "price_cents");
                e
            }),
            ("unset optional request field removed", {
                let mut e = endpoint();
                e.request
                    .deserialized
                    .retain(|f| f.rust_name != "price_cents");
                e
            }),
            ("route path changed", {
                let mut e = endpoint();
                e.endpoint.path = "/v2/items".to_owned();
                e
            }),
        ];

        let missed: Vec<&str> = breaking
            .iter()
            .filter(|(_, e)| check(&site, e).is_empty())
            .map(|(label, _)| *label)
            .collect();
        let false_positives: Vec<&str> = compatible
            .iter()
            .filter(|(_, e)| !check(&site, e).is_empty())
            .map(|(label, _)| *label)
            .collect();

        assert!(
            missed.is_empty(),
            "wire-breaking mutations not caught: {missed:?}"
        );
        assert!(
            false_positives.is_empty(),
            "compatible mutations wrongly rejected: {false_positives:?}"
        );
    }

    #[test]
    fn every_violation_is_reported_not_just_the_first() {
        let v = check(
            &site(&["sku", "colour"], Some(&["shade"]), false),
            &endpoint(),
        );
        assert_eq!(
            v.len(),
            4,
            "two bad reads, one bad write, one missing required: {v:?}"
        );
    }
}
