//! The release bundle: every wire descriptor of one build, joined and sorted.
//!
//! The wire macros write one JSON file per endpoint, type, client and checked
//! caller. [`Bundle::from_dir`] joins them. An endpoint carries its request and
//! response shapes inline. A caller call names the endpoint it reaches by
//! `service` and `endpoint`. The bytes are the same on every rebuild: all
//! lists are sorted, and no path or time goes in.

use std::collections::BTreeSet;
use std::path::Path;

use autumn_macros_support::wire::{
    CallerCall, CallerDescriptor, ClientDescriptor, EndpointDescriptor, TypeDescriptor,
    WireTypeDescriptor,
};
use serde::{Deserialize, Serialize};

/// The bundle format this CLI writes and reads.
pub const BUNDLE_FORMAT: u32 = 1;

/// The type name of an endpoint with no request body.
const NO_BODY: &str = "NoBody";

/// The wire contract of one release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    /// The bundle format. A reader refuses a format it does not know.
    pub format: u32,
    /// Every endpoint, sorted by service, then name.
    pub endpoints: Vec<BundleEndpoint>,
    /// Every checked call, sorted.
    pub callers: Vec<BundleCall>,
}

/// One endpoint with its shapes inline.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BundleEndpoint {
    /// The service, from `#[endpoint(service = "…")]`.
    pub service: String,
    /// The endpoint name.
    pub name: String,
    /// The crate that defines the endpoint.
    pub krate: String,
    /// HTTP method, uppercase.
    pub method: String,
    /// Route path, with `{param}` placeholders.
    pub path: String,
    /// What the endpoint accepts.
    pub request: WireTypeDescriptor,
    /// What the endpoint produces.
    pub response: WireTypeDescriptor,
}

impl BundleEndpoint {
    /// `service.name`.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}.{}", self.service, self.name)
    }

    /// `METHOD /path`.
    #[must_use]
    pub fn route(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

/// One call from a `#[contract_checked]` function to an endpoint.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BundleCall {
    /// The crate that holds the caller.
    pub krate: String,
    /// The calling function.
    pub function: String,
    /// The service called.
    pub service: String,
    /// The endpoint called.
    pub endpoint: String,
    /// Response fields the caller reads, by Rust name.
    pub reads: Vec<String>,
    /// Request fields the call site sets, by Rust name. `None` when unknown.
    pub writes: Option<Vec<String>>,
}

impl BundleCall {
    /// `krate::function`.
    #[must_use]
    pub fn caller(&self) -> String {
        format!("{}::{}", self.krate, self.function)
    }
}

impl Bundle {
    /// The endpoint `service.name`, if the bundle has it.
    #[must_use]
    pub fn endpoint(&self, service: &str, name: &str) -> Option<&BundleEndpoint> {
        self.endpoints
            .iter()
            .find(|e| e.service == service && e.name == name)
    }

    /// Parse a bundle and refuse a format this CLI does not know.
    ///
    /// # Errors
    /// The bytes are not a bundle, or the format is not [`BUNDLE_FORMAT`].
    pub fn from_slice(bytes: &[u8]) -> Result<Self, String> {
        let bundle: Self =
            serde_json::from_slice(bytes).map_err(|e| format!("not a contract bundle: {e}"))?;
        if bundle.format != BUNDLE_FORMAT {
            return Err(format!(
                "contract bundle format {} is not supported; this CLI reads format {BUNDLE_FORMAT}",
                bundle.format
            ));
        }
        Ok(bundle)
    }

    /// The canonical bytes: pretty JSON and a final newline.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        // Serializing plain structs, strings, and vectors cannot fail.
        let mut bytes = serde_json::to_vec_pretty(self).unwrap_or_default();
        bytes.push(b'\n');
        bytes
    }

    /// Join the descriptors in `dir` into a bundle.
    ///
    /// # Errors
    /// A descriptor does not parse, a name resolves to zero or two targets,
    /// or the directory holds no endpoint.
    pub fn from_dir(dir: &Path) -> Result<Self, String> {
        let endpoints: Vec<EndpointDescriptor> = read_all(dir, "endpoint.")?;
        let types: Vec<TypeDescriptor> = read_all(dir, "type.")?;
        let clients: Vec<ClientDescriptor> = read_all(dir, "client.")?;
        let callers: Vec<CallerDescriptor> = read_all(dir, "caller.")?;
        if endpoints.is_empty() {
            return Err(format!(
                "no endpoint descriptor in {}: no crate in the pass has an `#[endpoint]`",
                dir.display()
            ));
        }

        let mut seen = BTreeSet::new();
        let mut joined = Vec::with_capacity(endpoints.len());
        for endpoint in &endpoints {
            if !seen.insert((endpoint.service.clone(), endpoint.name.clone())) {
                return Err(format!(
                    "two endpoints are named `{}`; give one a different name",
                    endpoint.id()
                ));
            }
            joined.push(BundleEndpoint {
                service: endpoint.service.clone(),
                name: endpoint.name.clone(),
                krate: endpoint.krate.clone(),
                method: endpoint.method.clone(),
                path: endpoint.path.clone(),
                request: resolve_type(&endpoint.request_type, endpoint, &types)?,
                response: resolve_type(&endpoint.response_type, endpoint, &types)?,
            });
        }
        joined.sort();

        let mut calls = BTreeSet::new();
        for caller in &callers {
            for call in &caller.calls {
                let target = resolve_call(caller, call, &clients, &endpoints)?;
                calls.insert(BundleCall {
                    krate: caller.krate.clone(),
                    function: caller.function.clone(),
                    service: target.service.clone(),
                    endpoint: target.name.clone(),
                    reads: sorted(call.reads.clone()),
                    writes: call.writes.clone().map(sorted),
                });
            }
        }

        Ok(Self {
            format: BUNDLE_FORMAT,
            endpoints: joined,
            callers: calls.into_iter().collect(),
        })
    }
}

/// Sort and dedup a list of names.
fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names.dedup();
    names
}

/// Sort a shape's fields and aliases, so a reorder in source does not change
/// the bytes. Field order is not part of a JSON object.
fn canonical(mut shape: WireTypeDescriptor) -> WireTypeDescriptor {
    for field in shape
        .serialized
        .iter_mut()
        .chain(shape.deserialized.iter_mut())
    {
        field.aliases.sort();
        field.aliases.dedup();
    }
    shape.serialized.sort();
    shape.deserialized.sort();
    shape
}

/// The shape of type `name`, as the endpoint's crate sees it.
///
/// A type in the endpoint's own crate wins. Otherwise exactly one crate must
/// define it. `NoBody` has no fields.
fn resolve_type(
    name: &str,
    endpoint: &EndpointDescriptor,
    types: &[TypeDescriptor],
) -> Result<WireTypeDescriptor, String> {
    if name == NO_BODY {
        return Ok(WireTypeDescriptor {
            name: NO_BODY.to_owned(),
            ..WireTypeDescriptor::default()
        });
    }
    let named: Vec<&TypeDescriptor> = types.iter().filter(|t| t.shape.name == name).collect();
    let own = named.iter().find(|t| t.krate == endpoint.krate);
    let found = match (own, named.as_slice()) {
        (Some(own), _) => *own,
        (None, [one]) => *one,
        (None, []) => {
            return Err(format!(
                "endpoint `{}` uses type `{name}`, which has no descriptor; derive `WireShape` \
                 on it",
                endpoint.id()
            ));
        }
        (None, _) => {
            return Err(format!(
                "endpoint `{}` uses type `{name}`, and more than one crate defines a type of \
                 that name",
                endpoint.id()
            ));
        }
    };
    Ok(canonical(found.shape.clone()))
}

/// The endpoint one caller call reaches: caller -> client -> marker -> endpoint.
fn resolve_call<'a>(
    caller: &CallerDescriptor,
    call: &CallerCall,
    clients: &[ClientDescriptor],
    endpoints: &'a [EndpointDescriptor],
) -> Result<&'a EndpointDescriptor, String> {
    let at = format!(
        "`{}::{}` calls `{}`",
        caller.krate, caller.function, call.method
    );
    let client = match clients
        .iter()
        .filter(|c| c.krate == caller.krate && c.name == call.client)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [one] => *one,
        [] => {
            return Err(format!(
                "{at} through `{}`, and crate `{}` has no `wire_client!` of that name",
                call.client, caller.krate
            ));
        }
        _ => {
            return Err(format!(
                "{at} through `{}`, and crate `{}` declares two clients of that name",
                call.client, caller.krate
            ));
        }
    };
    let marker = client
        .endpoints
        .iter()
        .find(|e| e.method == call.method)
        .map(|e| e.marker.as_str())
        .ok_or_else(|| format!("{at}, and client `{}` has no such method", client.name))?;
    let segments: Vec<&str> = marker.split("::").collect();
    let ident = segments.last().copied().unwrap_or_default();
    // A path that starts with a crate name narrows the match to that crate.
    let crate_hint = match segments.as_slice() {
        [first, _, ..] if !["crate", "self", "super"].contains(first) => Some((*first).to_owned()),
        [_, _, ..] => Some(caller.krate.replace('-', "_")),
        _ => None,
    };
    let candidates: Vec<&EndpointDescriptor> = endpoints
        .iter()
        .filter(|e| e.endpoint_ident == ident)
        .collect();
    let by_crate: Vec<&EndpointDescriptor> = candidates
        .iter()
        .copied()
        .filter(|e| crate_hint.as_deref() == Some(e.krate.replace('-', "_").as_str()))
        .collect();
    match (by_crate.as_slice(), candidates.as_slice()) {
        ([one], _) | ([], [one]) => Ok(one),
        (_, []) => Err(format!(
            "{at} (`{marker}`), and no `#[endpoint]` in the pass defines `{ident}`"
        )),
        _ => Err(format!(
            "{at} (`{marker}`), and more than one crate defines `{ident}`"
        )),
    }
}

/// Parse every `<prefix>*.json` file in `dir`, in file-name order.
fn read_all<T: serde::de::DeserializeOwned>(dir: &Path, prefix: &str) -> Result<Vec<T>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".json"))
        })
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| {
            let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
            serde_json::from_slice(&bytes).map_err(|e| format!("parse {}: {e}", path.display()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use autumn_macros_support::wire::{ClientEndpoint, WireFieldDescriptor};

    fn field(name: &str, required: bool) -> WireFieldDescriptor {
        WireFieldDescriptor {
            rust_name: name.to_owned(),
            wire_name: name.to_owned(),
            ty: "String".to_owned(),
            required,
            aliases: Vec::new(),
        }
    }

    fn write(dir: &Path, name: &str, value: &impl Serialize) {
        std::fs::write(
            dir.join(name),
            serde_json::to_vec_pretty(value).expect("serializes"),
        )
        .expect("writes");
    }

    fn endpoint(name: &str, path: &str, request: &str) -> EndpointDescriptor {
        EndpointDescriptor {
            service: "catalog".to_owned(),
            name: name.to_owned(),
            endpoint_ident: format!("{name}_endpoint"),
            krate: "mesh-catalog".to_owned(),
            method: "GET".to_owned(),
            path: path.to_owned(),
            request_type: request.to_owned(),
            response_type: "Item".to_owned(),
        }
    }

    fn item() -> TypeDescriptor {
        TypeDescriptor {
            krate: "mesh-catalog".to_owned(),
            shape: WireTypeDescriptor {
                name: "Item".to_owned(),
                serialized: vec![field("name", true), field("id", true)],
                deserialized: vec![field("name", true), field("id", true)],
                closed: false,
            },
        }
    }

    fn client() -> ClientDescriptor {
        ClientDescriptor {
            krate: "mesh-storefront".to_owned(),
            name: "CatalogClient".to_owned(),
            endpoints: vec![ClientEndpoint {
                method: "get_item".to_owned(),
                marker: "mesh_catalog::get_item_endpoint".to_owned(),
            }],
        }
    }

    fn caller(function: &str, reads: &[&str]) -> CallerDescriptor {
        CallerDescriptor {
            krate: "mesh-storefront".to_owned(),
            function: function.to_owned(),
            calls: vec![CallerCall {
                client: "CatalogClient".to_owned(),
                method: "get_item".to_owned(),
                reads: reads.iter().map(|r| (*r).to_owned()).collect(),
                writes: None,
            }],
        }
    }

    /// A descriptor directory for one callee and one caller.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = dir.path();
        write(
            d,
            "endpoint.mesh-catalog.catalog.get_item.json",
            &endpoint("get_item", "/items/{id}", "NoBody"),
        );
        write(d, "type.mesh-catalog.Item.json", &item());
        write(d, "client.mesh-storefront.CatalogClient.json", &client());
        write(
            d,
            "caller.mesh-storefront.show_item.1.json",
            &caller("show_item", &["name"]),
        );
        dir
    }

    #[test]
    fn the_bundle_joins_endpoints_types_and_callers() {
        let dir = fixture();
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.format, BUNDLE_FORMAT);
        let [ep] = bundle.endpoints.as_slice() else {
            panic!("one endpoint: {bundle:?}");
        };
        assert_eq!(ep.id(), "catalog.get_item");
        assert_eq!(ep.request.name, "NoBody");
        assert_eq!(ep.response.name, "Item");
        let [call] = bundle.callers.as_slice() else {
            panic!("one call: {bundle:?}");
        };
        assert_eq!(call.caller(), "mesh-storefront::show_item");
        assert_eq!(
            (call.service.as_str(), call.endpoint.as_str()),
            ("catalog", "get_item")
        );
        assert_eq!(call.reads, ["name"]);
    }

    #[test]
    fn fields_are_sorted_so_a_reorder_does_not_change_the_bytes() {
        let dir = fixture();
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        let names: Vec<&str> = bundle.endpoints[0]
            .response
            .serialized
            .iter()
            .map(|f| f.rust_name.as_str())
            .collect();
        assert_eq!(names, ["id", "name"]);
    }

    #[test]
    fn the_bytes_are_the_same_whatever_order_the_files_were_written_in() {
        let first = fixture();
        let second = tempfile::tempdir().expect("tempdir");
        let d = second.path();
        // Same content, written in the opposite order, under other file names.
        write(
            d,
            "caller.mesh-storefront.show_item.zz.json",
            &caller("show_item", &["name"]),
        );
        write(d, "client.mesh-storefront.CatalogClient.json", &client());
        let mut reordered = item();
        reordered.shape.serialized.reverse();
        reordered.shape.deserialized.reverse();
        write(d, "type.mesh-catalog.Item.json", &reordered);
        write(
            d,
            "endpoint.mesh-catalog.catalog.get_item.json",
            &endpoint("get_item", "/items/{id}", "NoBody"),
        );
        let a = Bundle::from_dir(first.path()).expect("bundle").to_bytes();
        let b = Bundle::from_dir(d).expect("bundle").to_bytes();
        assert!(!a.is_empty());
        assert_eq!(a, b);
        assert!(a.ends_with(b"\n"));
    }

    #[test]
    fn the_bytes_round_trip() {
        let dir = fixture();
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        let back = Bundle::from_slice(&bundle.to_bytes()).expect("parses");
        assert_eq!(back, bundle);
    }

    #[test]
    fn an_unknown_format_is_refused() {
        let err = Bundle::from_slice(br#"{"format": 99, "endpoints": [], "callers": []}"#)
            .expect_err("refused");
        assert!(err.contains("format 99"), "{err}");
    }

    #[test]
    fn an_empty_directory_is_an_error_not_an_empty_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = Bundle::from_dir(dir.path()).expect_err("refused");
        assert!(err.contains("no endpoint descriptor"), "{err}");
    }

    #[test]
    fn a_response_type_with_no_descriptor_is_an_error() {
        let dir = fixture();
        std::fs::remove_file(dir.path().join("type.mesh-catalog.Item.json")).expect("rm");
        let err = Bundle::from_dir(dir.path()).expect_err("refused");
        assert!(err.contains("`Item`"), "{err}");
    }

    #[test]
    fn a_caller_whose_endpoint_is_missing_is_an_error() {
        let dir = fixture();
        std::fs::remove_file(
            dir.path()
                .join("endpoint.mesh-catalog.catalog.get_item.json"),
        )
        .expect("rm");
        write(
            dir.path(),
            "endpoint.mesh-catalog.catalog.list.json",
            &endpoint("list", "/items", "NoBody"),
        );
        let err = Bundle::from_dir(dir.path()).expect_err("refused");
        assert!(err.contains("get_item_endpoint"), "{err}");
    }

    #[test]
    fn a_caller_whose_client_is_missing_is_an_error() {
        let dir = fixture();
        std::fs::remove_file(dir.path().join("client.mesh-storefront.CatalogClient.json"))
            .expect("rm");
        let err = Bundle::from_dir(dir.path()).expect_err("refused");
        assert!(err.contains("CatalogClient"), "{err}");
    }

    #[test]
    fn a_crate_qualified_marker_picks_its_crates_endpoint() {
        let dir = fixture();
        let mut other = endpoint("get_item", "/other/{id}", "NoBody");
        other.krate = "warehouse".to_owned();
        other.service = "warehouse".to_owned();
        write(
            dir.path(),
            "endpoint.warehouse.warehouse.get_item.json",
            &other,
        );
        let mut other_item = item();
        other_item.krate = "warehouse".to_owned();
        write(dir.path(), "type.warehouse.Item.json", &other_item);
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.callers[0].service, "catalog");
    }

    #[test]
    fn two_functions_with_one_name_keep_both_calls() {
        let dir = fixture();
        write(
            dir.path(),
            "caller.mesh-storefront.show_item.2.json",
            &caller("show_item", &["id"]),
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.callers.len(), 2);
    }

    #[test]
    fn an_identical_call_is_kept_once() {
        let dir = fixture();
        write(
            dir.path(),
            "caller.mesh-storefront.show_item.2.json",
            &caller("show_item", &["name"]),
        );
        let bundle = Bundle::from_dir(dir.path()).expect("bundle");
        assert_eq!(bundle.callers.len(), 1);
        let _: BTreeSet<&BundleCall> = bundle.callers.iter().collect();
    }
}
