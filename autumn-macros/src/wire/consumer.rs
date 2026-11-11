//! Event consumers: `#[listener]` and `#[event_consumer]`.
//!
//! A consumer reads fields off an event payload. Its *read-set* is every
//! field it names. Each read becomes a const assertion against the event's own
//! field table, [`Event::SHAPE`]. A read of a field the event does not send
//! fails the build, and the message names the consumer, the event and the
//! field.
//!
//! The read-set also goes into a `listener.*.json` descriptor, so the
//! cross-release check knows what each deployed consumer reads.
//!
//! [`Event::SHAPE`]: ../../../autumn_web/events/trait.Event.html

use proc_macro2::TokenStream;
use quote::{ToTokens as _, quote, quote_spanned};
use syn::{ItemFn, LitStr, Path};

use crate::wire::checked::{
    PayloadBinding, PayloadReads, escape_for_assert, payload_reads, top_field,
};
use crate::wire::ir::{ListenerDescriptor, stable_key};
use crate::wire::store;

/// `func` with its checks as the first statements of its body.
///
/// In the body, not beside the function: a `const _` item is not allowed in
/// an `impl` block, and the body works for a free function and a method.
#[must_use]
pub fn with_checks(func: &ItemFn, event: &Path, reads: &PayloadReads) -> ItemFn {
    let checks = consumer_checks(func, event, reads);
    let mut func = func.clone();
    let block: syn::Block = syn::parse_quote! { { #checks } };
    func.block.stmts.splice(0..0, block.stmts);
    func
}

/// The checks for one consumer: const assertions, the descriptor write, and
/// the dir tracker.
#[must_use]
pub fn consumer_checks(func: &ItemFn, event: &Path, reads: &PayloadReads) -> TokenStream {
    let function = func.sig.ident.to_string();
    let event_text = path_text(event);
    // A nested read asserts its first segment: one build shares the nested
    // type, so rustc checks the rest. The descriptor keeps the whole path.
    let mut seen = std::collections::BTreeSet::new();
    let top: Vec<(&str, proc_macro2::Span)> = reads
        .reads
        .iter()
        .map(|(path, span)| (top_field(path), *span))
        .filter(|(field, _)| seen.insert(*field))
        .collect();
    let assertions = top.iter().map(|(field, span)| {
        let message = escape_for_assert(&format!(
            "event contract broken in `{function}`: reads field `{field}` of event \
             `{event_text}`, which the event does not send"
        ));
        let span = *span;
        let field = LitStr::new(field, span);
        let message = LitStr::new(&message, span);
        quote_spanned! { span =>
            const _: () = ::core::assert!(
                ::autumn_web::wire::event_delivers(
                    <#event as ::autumn_web::events::Event>::SHAPE,
                    #field,
                ),
                #message
            );
        }
    });
    let written = store::contract_dir().map_or_else(TokenStream::new, |dir| {
        let descriptor = ListenerDescriptor {
            krate: std::env::var("CARGO_PKG_NAME").unwrap_or_default(),
            function: function.clone(),
            event: event_text.clone(),
            reads: reads.reads.iter().map(|(field, _)| field.clone()).collect(),
        };
        // The event path is part of the key: two equal functions in two
        // modules can consume two different events.
        let key = stable_key(&format!("{event_text} {}", func.to_token_stream()));
        store::write_outcome(store::write_listener(&dir, &descriptor, &key))
    });
    let track = store::track_contract_dir();
    quote! { #track #written #(#assertions)* }
}

/// The event type of `#[listener(E, …)]` as a path, if it is one.
#[must_use]
pub fn event_path(ty: &syn::Type) -> Option<Path> {
    match ty {
        syn::Type::Path(tp) if tp.qself.is_none() => Some(tp.path.clone()),
        syn::Type::Paren(p) => event_path(&p.elem),
        syn::Type::Group(g) => event_path(&g.elem),
        _ => None,
    }
}

/// The read-set of a `#[listener]`: its second parameter is the payload.
#[must_use]
pub fn listener_reads(func: &ItemFn, event: &Path) -> PayloadReads {
    payload_reads(func, event, PayloadBinding::Param(1))
}

/// `#[event_consumer(E)]`: check a function that reads an `E` payload.
pub fn event_consumer_macro(attr: TokenStream, item: &TokenStream) -> TokenStream {
    match expand_consumer(attr, item) {
        Ok(ts) => ts,
        Err(err) => {
            let err = err.to_compile_error();
            quote! { #err #item }
        }
    }
}

fn expand_consumer(attr: TokenStream, item: &TokenStream) -> Result<TokenStream, syn::Error> {
    let event: Path = syn::parse2(attr).map_err(|e| {
        syn::Error::new(
            e.span(),
            "#[event_consumer] needs an event type: #[event_consumer(OrderPlaced)]",
        )
    })?;
    let func: ItemFn = syn::parse2(item.clone())?;
    let reads = payload_reads(&func, &event, PayloadBinding::Typed);
    if !reads.found {
        let name = &func.sig.ident;
        let ty = path_text(&event);
        return Err(syn::Error::new_spanned(
            &event,
            format!(
                "#[event_consumer] found no `{ty}` payload in `{name}` to check. Give the \
                 payload a type: a parameter (`order: {ty}`), a typed `let` (`let order: \
                 {ty} = message.payload_as()?`), or a turbofish (`payload_as::<{ty}>()`)."
            ),
        ));
    }
    let func = with_checks(&func, &event, &reads);
    Ok(quote! { #func })
}

/// A path as `a::b::C`, without generic arguments.
fn path_text(path: &Path) -> String {
    path.segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// The span of the first read of `field`, for a test.
#[cfg(test)]
fn span_of(reads: &PayloadReads, field: &str) -> Option<proc_macro2::Span> {
    reads
        .reads
        .iter()
        .find(|(f, _)| f == field)
        .map(|(_, span)| *span)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(src: &str) -> Path {
        syn::parse_str(src).expect("path parses")
    }

    fn listener(src: &str) -> Vec<String> {
        let func: ItemFn = syn::parse_str(src).expect("fixture parses");
        listener_reads(&func, &path("ContactUpdated"))
            .reads
            .into_iter()
            .map(|(f, _)| f)
            .collect()
    }

    fn consumer(src: &str) -> PayloadReads {
        let func: ItemFn = syn::parse_str(src).expect("fixture parses");
        payload_reads(&func, &path("ContactUpdated"), PayloadBinding::Typed)
    }

    fn names(reads: &PayloadReads) -> Vec<&str> {
        reads.reads.iter().map(|(f, _)| f.as_str()).collect()
    }

    #[test]
    fn a_field_read_off_the_listener_payload_is_in_the_read_set() {
        assert_eq!(
            listener(
                "async fn on(state: AppState, event: ContactUpdated) -> R { let _ = event.email; Ok(()) }"
            ),
            ["email"]
        );
    }

    #[test]
    fn the_listener_payload_is_the_second_parameter_whatever_its_type_says() {
        // A type alias or a full path still names the payload.
        assert_eq!(
            listener("async fn on(state: AppState, e: crate::Alias) -> R { let _ = e.id; Ok(()) }"),
            ["id"]
        );
    }

    #[test]
    fn a_destructured_listener_payload_names_the_fields_it_reads() {
        assert_eq!(
            listener(
                "async fn on(state: AppState, ContactUpdated { id, email, .. }: ContactUpdated) -> R { Ok(()) }"
            ),
            ["email", "id"]
        );
    }

    #[test]
    fn reads_inside_a_macro_and_a_let_are_in_the_read_set() {
        assert_eq!(
            listener(
                "async fn on(s: AppState, event: ContactUpdated) -> R { \
                 let ContactUpdated { name, .. } = event.clone(); \
                 tracing::info!(\"{}\", event.email); Ok(()) }"
            ),
            ["email", "name"]
        );
    }

    #[test]
    fn a_shadowed_name_is_not_the_payload() {
        assert_eq!(
            listener(
                "async fn on(s: AppState, event: ContactUpdated) -> R { \
                 for event in rows { let _ = event.other; } \
                 let event = 1; let _ = event.thing; Ok(()) }"
            ),
            [] as [&str; 0]
        );
    }

    #[test]
    fn a_read_inside_matches_is_a_read_and_one_inside_stringify_is_not() {
        assert_eq!(
            listener(
                "async fn on(s: AppState, event: ContactUpdated) -> R { \
                 let _ = matches!(event.email, Some(_)); \
                 let _ = stringify!(event.phone); Ok(()) }"
            ),
            ["email"]
        );
    }

    #[test]
    fn a_read_before_not_equal_in_a_macro_is_a_read() {
        assert_eq!(
            listener(
                "async fn on(s: AppState, event: ContactUpdated) -> R { \
                 assert!(event.email != None); \
                 assert!(event.id == 1); Ok(()) }"
            ),
            ["email", "id"]
        );
    }

    #[test]
    fn a_bitwise_or_in_a_macro_is_not_a_closure() {
        assert_eq!(
            listener(
                "async fn on(s: AppState, event: ContactUpdated) -> R { \
                 assert!(event.email.is_some() | event.phone.is_some()); \
                 assert!(event.id > 0 || event.name.is_empty()); Ok(()) }"
            ),
            ["email", "id", "name", "phone"]
        );
    }

    #[test]
    fn a_closure_parameter_in_a_macro_still_shadows_the_payload() {
        assert_eq!(
            listener(
                "async fn on(s: AppState, event: ContactUpdated) -> R { \
                 tracing::info!(rows.iter().map(|event| event.other)); \
                 tracing::info!(f(move |event| event.other2)); Ok(()) }"
            ),
            [] as [&str; 0]
        );
    }

    #[test]
    fn a_closure_parameter_shadows_only_inside_the_closure() {
        assert_eq!(
            listener(
                "async fn on(s: AppState, event: ContactUpdated) -> R { \
                 assert!(event.email.is_some() && rows.iter().any(|event| event.enabled)); \
                 tracing::info!(a = event.id, b = f(|event| event.other), c = event.name); Ok(()) }"
            ),
            ["email", "id", "name"]
        );
    }

    #[test]
    fn an_error_adapter_keeps_the_payload() {
        let reads = consumer(
            "async fn h(m: OutboxMessage) -> R { \
             let event = m.payload_as::<ContactUpdated>().map_err(convert)?; let _ = event.email; \
             let c = m.payload_as::<ContactUpdated>().context(\"decode\")?; let _ = c.id; \
             if let Some(o) = m.payload_as::<ContactUpdated>().ok() { let _ = o.name; } Ok(()) }",
        );
        assert_eq!(names(&reads), ["email", "id", "name"]);
    }

    #[test]
    fn a_match_that_hands_back_the_decode_is_the_payload() {
        let reads = consumer(
            "async fn h(m: OutboxMessage) -> R { \
             let event = match m.payload_as::<ContactUpdated>() { \
                 Ok(e) => e, Err(err) => return Err(err.into()) }; \
             let _ = event.email; \
             let other = match m.payload_as::<ContactUpdated>() { \
                 Ok(e) => e.id, Err(_) => { panic!(\"bad\") } }; \
             let _ = other.not_payload; Ok(()) }",
        );
        // `other` is a field, not the payload, so `not_payload` is no read.
        assert_eq!(names(&reads), ["email", "id"]);
    }

    #[test]
    fn a_struct_pattern_in_matches_reads_its_fields() {
        assert_eq!(
            listener(
                "async fn on(s: AppState, event: ContactUpdated) -> R { \
                 let _ = matches!(event, ContactUpdated { email: Some(_), .. }); \
                 let _ = matches!(&event, ContactUpdated { id, .. } if *id > event.age); Ok(()) }"
            ),
            ["age", "email", "id"]
        );
    }

    #[test]
    fn a_block_that_is_only_its_tail_holds_the_payload() {
        let reads = consumer(
            "async fn h(m: OutboxMessage, event: Local) -> R { \
             let decoded = { m.payload_as::<ContactUpdated>()? }; let _ = decoded.email; \
             let other = { let event = local(); event }; let _ = other.not_payload; Ok(()) }",
        );
        assert_eq!(names(&reads), ["email"]);
    }

    #[test]
    fn a_success_callback_on_the_decode_holds_the_payload() {
        let reads = consumer(
            "async fn h(m: OutboxMessage) -> R { \
             let _ = m.payload_as::<ContactUpdated>().map(|event| event.email); \
             let _ = m.payload_as::<ContactUpdated>().map_or(0, |c| c.id); \
             let _ = m.payload_as::<ContactUpdated>().as_ref().map(|event| event.name.clone()); \
             let _ = m.payload_as::<ContactUpdated>().map_err(|e| e.kind); \
             let _ = rows.iter().map(|event| event.not_payload); Ok(()) }",
        );
        assert_eq!(names(&reads), ["email", "id", "name"]);
    }

    #[test]
    fn a_method_call_on_the_payload_is_not_a_read() {
        assert_eq!(
            listener(
                "async fn on(s: AppState, event: ContactUpdated) -> R { event.validate(); Ok(()) }"
            ),
            [] as [&str; 0]
        );
    }

    #[test]
    fn a_typed_parameter_is_the_payload_of_a_consumer() {
        let reads = consumer(
            "async fn h(state: AppState, c: ContactUpdated) -> R { let _ = c.id; Ok(()) }",
        );
        assert!(reads.found);
        assert_eq!(names(&reads), ["id"]);
    }

    #[test]
    fn a_typed_let_is_the_payload_of_a_consumer() {
        let reads = consumer(
            "async fn h(state: AppState, m: OutboxMessage) -> R { \
             let c: ContactUpdated = m.payload_as()?; let _ = c.email; Ok(()) }",
        );
        assert!(reads.found);
        assert_eq!(names(&reads), ["email"]);
    }

    #[test]
    fn a_turbofish_names_the_payload_of_a_consumer() {
        let reads = consumer(
            "async fn h(state: AppState, m: OutboxMessage) -> R { \
             let c = m.payload_as::<ContactUpdated>()?; \
             let _ = m.payload_as::<ContactUpdated>()?.id; \
             let _ = c.email; Ok(()) }",
        );
        assert!(reads.found);
        assert_eq!(names(&reads), ["email", "id"]);
    }

    #[test]
    fn a_function_turbofish_names_the_payload_of_a_consumer() {
        let reads = consumer(
            "async fn h(m: OutboxMessage) -> R { \
             let c = serde_json::from_value::<ContactUpdated>(m.payload)?; let _ = c.id; Ok(()) }",
        );
        assert_eq!(names(&reads), ["id"]);
    }

    #[test]
    fn a_consumer_with_no_payload_is_not_found() {
        let reads = consumer("async fn h(state: AppState, m: OutboxMessage) -> R { Ok(()) }");
        assert!(!reads.found);
    }

    #[test]
    fn a_read_keeps_the_span_of_the_field() {
        let reads = consumer("async fn h(c: ContactUpdated) { let _ = c.id; }");
        assert!(span_of(&reads, "id").is_some());
    }

    #[test]
    fn event_consumer_refuses_a_function_with_no_payload() {
        let out = event_consumer_macro(
            quote! { ContactUpdated },
            &quote! { async fn h(state: AppState, m: OutboxMessage) -> R { Ok(()) } },
        )
        .to_string();
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("found no `ContactUpdated` payload"), "{out}");
    }

    #[test]
    fn event_consumer_asserts_each_read_against_the_event_table() {
        let out = event_consumer_macro(
            quote! { ContactUpdated },
            &quote! { async fn h(c: ContactUpdated) -> R { let _ = c.email; Ok(()) } },
        )
        .to_string();
        assert!(out.contains("event_delivers"), "{out}");
        assert!(
            out.contains("< ContactUpdated as :: autumn_web :: events :: Event > :: SHAPE"),
            "{out}"
        );
        assert!(
            out.contains("reads field `email` of event `ContactUpdated`"),
            "{out}"
        );
        assert!(out.contains("AUTUMN_CONTRACT_DIR"), "{out}");
    }

    #[test]
    fn a_nested_payload_field_is_a_dotted_read() {
        assert_eq!(
            listener(
                "async fn on(s: AppState, event: ContactUpdated) -> R { \
                 let _ = event.address.city; tracing::info!(\"{}\", event.address.zip); Ok(()) }"
            ),
            ["address", "address.city", "address.zip"]
        );
    }

    #[test]
    fn event_consumer_asserts_the_first_segment_of_a_nested_read() {
        let out = event_consumer_macro(
            quote! { ContactUpdated },
            &quote! { async fn h(c: ContactUpdated) -> R { let _ = c.address.city; Ok(()) } },
        )
        .to_string();
        assert!(
            out.contains("reads field `address` of event `ContactUpdated`"),
            "{out}"
        );
        assert!(!out.contains("address.city"), "{out}");
    }

    #[test]
    fn a_turbofish_read_with_no_binding_is_found() {
        let reads = consumer(
            "async fn h(m: OutboxMessage) -> R { record(m.payload_as::<ContactUpdated>()?.email).await }",
        );
        assert!(reads.found);
        assert_eq!(names(&reads), ["email"]);
        let decoded_only = consumer(
            "async fn h(m: OutboxMessage) -> R { let _ = m.payload_as::<ContactUpdated>()?; Ok(()) }",
        );
        assert!(
            decoded_only.found,
            "a decode with no read is still the payload"
        );
    }

    #[test]
    fn an_ok_or_some_arm_over_a_decode_holds_the_payload() {
        let reads = consumer(
            "async fn h(m: OutboxMessage) -> R { \
             match m.payload_as::<ContactUpdated>() { Ok(event) => record(event.secret), Err(_) => {} } \
             if let Ok(c) = serde_json::from_value::<ContactUpdated>(v) { let _ = c.id; } \
             if let Err(e) = m.payload_as::<ContactUpdated>() { let _ = e.kind; } Ok(()) }",
        );
        assert_eq!(names(&reads), ["id", "secret"]);
    }

    #[test]
    fn an_if_let_binding_does_not_reach_the_else_branch_or_later_code() {
        let reads = consumer(
            "async fn h(m: OutboxMessage, event: Local) -> R { \
             if let Ok(event) = m.payload_as::<ContactUpdated>() { let _ = event.id; } \
             else { let _ = event.local_else; } \
             let _ = event.local_after; \
             while let Some(event) = next::<ContactUpdated>() { let _ = event.email; } \
             let _ = event.local_end; Ok(()) }",
        );
        // `id` and `email` come from the two payload bindings. No
        // `local_*` read of the outer `event` leaks in.
        assert_eq!(names(&reads), ["email", "id"]);
    }

    #[test]
    fn a_decode_that_nothing_binds_is_still_the_payload() {
        for body in [
            "record(m.payload_as::<ContactUpdated>()?).await",
            "m.payload_as::<ContactUpdated>()?; Ok(())",
            "forward(serde_json::from_value::<ContactUpdated>(v)?)",
        ] {
            let reads = consumer(&format!("async fn h(m: OutboxMessage) -> R {{ {body} }}"));
            assert!(reads.found, "`{body}` decodes the payload");
            assert!(reads.reads.is_empty(), "`{body}` names no field");
        }
    }

    #[test]
    fn a_struct_pattern_that_names_the_event_is_the_payload() {
        let reads = consumer(
            "async fn h(m: OutboxMessage) -> R { let ContactUpdated { email, .. } = m.payload_as()?; Ok(()) }",
        );
        assert!(reads.found);
        assert_eq!(names(&reads), ["email"]);
    }

    #[test]
    fn if_let_match_and_at_patterns_over_the_payload_are_reads() {
        assert_eq!(
            listener(
                "async fn on(s: AppState, event: ContactUpdated) -> R { \
                 if let ContactUpdated { email, .. } = &event { let _ = email; } \
                 match event.clone() { ContactUpdated { id, .. } => {} } \
                 let whole @ ContactUpdated { name, .. } = event; Ok(()) }"
            ),
            ["email", "id", "name"]
        );
    }

    #[test]
    fn a_longer_path_is_another_type_not_the_payload() {
        let reads =
            consumer("async fn h(old: legacy::ContactUpdated) -> R { let _ = old.fax; Ok(()) }");
        assert!(!reads.found);
        let qualified = {
            let func: ItemFn =
                syn::parse_str("async fn h(c: ContactUpdated) -> R { let _ = c.id; Ok(()) }")
                    .expect("fixture parses");
            payload_reads(&func, &path("core::ContactUpdated"), PayloadBinding::Typed)
        };
        assert!(
            qualified.found,
            "a short spelling of the declared path matches"
        );
        let prefixed =
            consumer("async fn h(c: crate::ContactUpdated) -> R { let _ = c.id; Ok(()) }");
        assert!(prefixed.found, "a `crate::` prefix is the same type");
    }

    #[test]
    fn the_checks_go_inside_the_body_so_a_method_works() {
        let out = event_consumer_macro(
            quote! { ContactUpdated },
            &quote! { async fn post(&self, c: ContactUpdated) -> R { let _ = c.email; Ok(()) } },
        );
        let func: ItemFn = syn::parse2(out).expect("one function, no items beside it");
        let body = func.block.to_token_stream().to_string();
        assert!(body.contains("event_delivers"), "{body}");
    }

    #[test]
    fn a_listener_on_a_type_that_is_not_a_path_has_no_table() {
        let ty: syn::Type = syn::parse_str("(A, B)").expect("type parses");
        assert!(event_path(&ty).is_none());
        let ty: syn::Type = syn::parse_str("crate::ContactUpdated").expect("type parses");
        assert!(event_path(&ty).is_some());
    }

    #[test]
    fn event_consumer_needs_an_event_type() {
        let out = event_consumer_macro(quote! {}, &quote! { async fn h() {} }).to_string();
        assert!(out.contains("needs an event type"), "{out}");
    }
}
