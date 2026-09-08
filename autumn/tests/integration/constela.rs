//! End-to-end tests for the Constela pipeline: parse → validate → render, plus
//! the server-side action interpreter.
//!
//! These are the regression lock on the guarantee documented in
//! `docs/guide/constela.md` and `autumn_web::constela::policy`: a document
//! written by a language model — that is, by an attacker, until proven
//! otherwise — cannot reach the page as script, cannot reach it as a
//! `javascript:` URL, cannot collide with the host page's element ids, and
//! cannot make the renderer do unbounded work.
//!
//! The payload corpus below is deliberately adversarial, in the same spirit as
//! `rich_text.rs`. Widening `ALLOWED_TAGS`, `ALLOWED_ATTRS` or
//! `ALLOWED_URL_SCHEMES` without re-deriving these expectations should fail
//! loudly here.

use autumn_web::constela::{
    ConstelaError, Document, Effect, Limits, RenderContext, RenderLimits, RouteValues, codes,
};
use serde_json::{Map, Value, json};

/// Parse and validate, or panic with the diagnostics.
fn document(source: &str) -> Document {
    match Document::parse(source, &Limits::default()) {
        Ok(document) => document,
        Err(err) => panic!("expected a valid document, got:\n{err}"),
    }
}

/// Parse, validate and render against the document's own initial state.
fn render(source: &str) -> String {
    let document = document(source);
    let ctx = RenderContext {
        state: document.initial_state(),
        ..RenderContext::default()
    };
    document.render(&ctx).expect("renders").body.0
}

/// The diagnostic codes a document produces, or an empty list if it is valid.
fn codes_of(source: &str) -> Vec<&'static str> {
    match Document::parse(source, &Limits::default()) {
        Ok(_) => Vec::new(),
        Err(err) => err.diagnostics().iter().map(|d| d.code).collect(),
    }
}

/// Wrap a view in the smallest document that carries it.
fn view(view: &str) -> String {
    format!(r#"{{"version":"1.0","view":{view}}}"#)
}

// ---------------------------------------------------------------------------
// The safety corpus
// ---------------------------------------------------------------------------

/// Tag-open markers that must never appear as live markup in rendered output.
/// A payload surviving as escaped text (`&lt;script&gt;`) is inert and does not
/// match these — which is exactly the distinction being asserted.
const FORBIDDEN_MARKUP: &[&str] = &[
    "<script",
    "<style",
    "<iframe",
    "<object",
    "<embed",
    "<base",
    "<link",
    "<meta",
    "<svg",
    "<math",
    "<template",
    "<noscript",
    "javascript:",
    "vbscript:",
    "data:text/html",
];

#[test]
fn script_bearing_tags_are_rejected_at_validation() {
    for tag in [
        "script", "style", "iframe", "object", "embed", "base", "link", "meta", "svg", "math",
        "template", "noscript", "html", "head", "body", "title", "frameset",
    ] {
        let codes = codes_of(&view(&format!(r#"{{"kind":"element","tag":"{tag}"}}"#)));
        assert!(
            codes.contains(&codes::TAG_NOT_ALLOWED),
            "<{tag}> must be rejected, got {codes:?}"
        );
    }
}

#[test]
fn event_handler_attributes_are_rejected_at_validation() {
    for attr in [
        "onclick",
        "onerror",
        "onload",
        "onmouseover",
        "onfocus",
        "onanimationstart",
        "ontoggle",
        "onbeforetoggle",
    ] {
        let codes = codes_of(&view(&format!(
            r#"{{"kind":"element","tag":"div","props":{{"{attr}":{{"expr":"lit","value":"alert(1)"}}}}}}"#
        )));
        assert!(
            codes.contains(&codes::ATTR_NOT_ALLOWED),
            "{attr} must be rejected, got {codes:?}"
        );
    }
}

#[test]
fn dangerous_url_schemes_are_rejected_at_validation() {
    for url in [
        "javascript:alert(1)",
        "JaVaScRiPt:alert(1)",
        "java\\tscript:alert(1)",
        "  javascript:alert(1)",
        "vbscript:msgbox(1)",
        "data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==",
        "file:///etc/passwd",
    ] {
        for attr in ["href", "src", "action", "formaction", "poster", "cite"] {
            let source = view(&format!(
                r#"{{"kind":"element","tag":"a","props":{{"{attr}":{{"expr":"lit","value":"{url}"}}}}}}"#
            ));
            let codes = codes_of(&source);
            assert!(
                codes.contains(&codes::URL_SCHEME),
                "{attr}={url:?} must be rejected, got {codes:?}"
            );
        }
    }
}

#[test]
fn a_url_assembled_at_runtime_is_still_scheme_checked() {
    // Validation can only judge a literal. This one is built out of state, so
    // the render-time check is the only thing standing between the document
    // and a `javascript:` href — which is why that check exists.
    let source = r#"{
        "version":"1.0",
        "state":{"scheme":{"type":"string","initial":"javascript:"}},
        "view":{"kind":"element","tag":"a","props":{"href":{
            "expr":"concat","items":[{"expr":"state","name":"scheme"},{"expr":"lit","value":"alert(1)"}]}}}
    }"#;
    let document = document(source);
    let ctx = RenderContext {
        state: document.initial_state(),
        ..RenderContext::default()
    };
    let err = document.render(&ctx).expect_err("computed javascript: URL");
    assert_eq!(err.diagnostics()[0].code, codes::URL_SCHEME);
}

#[test]
fn text_and_attribute_values_are_escaped() {
    // Carried through JSON, so it reaches the renderer as the eight characters
    // `<script>` rather than as anything the parser had a chance to reject.
    let payload = "<script>alert(1)</script>";
    let rendered = render(&format!(
        r#"{{"version":"1.0","state":{{"evil":{{"type":"string","initial":"{payload}"}}}},
            "view":{{"kind":"element","tag":"div","props":{{"title":{{"expr":"state","name":"evil"}}}},
                     "children":[{{"kind":"text","value":{{"expr":"state","name":"evil"}}}}]}}}}"#
    ));

    for marker in FORBIDDEN_MARKUP {
        assert!(
            !rendered.contains(marker),
            "{marker} survived as live markup in {rendered}"
        );
    }
    assert!(rendered.contains("&lt;script&gt;"), "{rendered}");
    assert!(rendered.contains(r#"title="&lt;script&gt;"#), "{rendered}");
}

#[test]
fn an_attribute_value_cannot_break_out_of_its_quotes() {
    let rendered = render(
        r#"{"version":"1.0",
            "state":{"breakout":{"type":"string","initial":"\" onload=\"alert(1)"}},
            "view":{"kind":"element","tag":"div","props":{"title":{"expr":"state","name":"breakout"}}}}"#,
    );
    assert_eq!(
        rendered,
        r#"<div title="&quot; onload=&quot;alert(1)"></div>"#
    );
}

#[test]
fn a_code_block_language_cannot_inject_an_attribute() {
    let rendered = render(
        r#"{"version":"1.0","view":{"kind":"code",
            "language":{"expr":"lit","value":"rust\" onload=\"alert(1)"},
            "content":{"expr":"lit","value":"<b>hi</b>"}}}"#,
    );
    assert_eq!(
        rendered,
        r#"<pre><code class="language-rustonloadalert1"><b>hi</b></code></pre>"#
            .replace("<b>hi</b>", "&lt;b&gt;hi&lt;/b&gt;")
    );
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[test]
fn ids_are_prefixed_so_a_document_cannot_clobber_the_host_page() {
    let rendered = render(
        r#"{"version":"1.0","view":{"kind":"element","tag":"div","children":[
            {"kind":"element","tag":"label","props":{
                "for":{"expr":"lit","value":"login"}}},
            {"kind":"element","tag":"input","props":{
                "id":{"expr":"lit","value":"login"},
                "aria-describedby":{"expr":"lit","value":"hint one"}}}]}}"#,
    );
    assert!(rendered.contains(r#"for="c-login""#), "{rendered}");
    assert!(rendered.contains(r#"id="c-login""#), "{rendered}");
    assert!(
        rendered.contains(r#"aria-describedby="c-hint c-one""#),
        "{rendered}"
    );
    // The unprefixed id the host page might own must not appear at all.
    assert!(!rendered.contains(r#"id="login""#), "{rendered}");
}

#[test]
fn the_id_prefix_is_configurable_for_pages_embedding_several_documents() {
    let document = document(
        r#"{"version":"1.0","view":{"kind":"element","tag":"div","props":{
            "id":{"expr":"lit","value":"root"}}}}"#,
    );
    let ctx = RenderContext {
        state: document.initial_state(),
        id_prefix: "panel-2-".to_string(),
        ..RenderContext::default()
    };
    assert_eq!(
        document.render(&ctx).expect("renders").body.0,
        r#"<div id="panel-2-root"></div>"#
    );
}

#[test]
fn blank_targets_are_hardened_against_reverse_tabnabbing() {
    let rendered = render(
        r#"{"version":"1.0","view":{"kind":"element","tag":"a","props":{
            "href":{"expr":"lit","value":"https://example.com"},
            "target":{"expr":"lit","value":"_blank"}}}}"#,
    );
    assert!(
        rendered.contains(r#"rel="noopener noreferrer""#),
        "{rendered}"
    );
}

#[test]
fn boolean_props_render_as_bare_attributes_and_false_removes_them() {
    let rendered = render(
        r#"{"version":"1.0","view":{"kind":"element","tag":"input","props":{
            "disabled":{"expr":"lit","value":true},
            "readonly":{"expr":"lit","value":false},
            "name":{"expr":"lit","value":"email"}}}}"#,
    );
    assert!(rendered.contains(" disabled"), "{rendered}");
    assert!(!rendered.contains("readonly"), "{rendered}");
    assert!(rendered.contains(r#"name="email""#), "{rendered}");
}

#[test]
fn void_elements_render_without_a_closing_tag() {
    assert_eq!(render(&view(r#"{"kind":"element","tag":"br"}"#)), "<br>");
}

#[test]
fn jsx_flavoured_prop_names_are_accepted() {
    // A model trained on JSX writes `className`; rejecting it would be a
    // diagnostic about spelling rather than about safety.
    let rendered = render(
        r#"{"version":"1.0","view":{"kind":"element","tag":"div","props":{
            "className":{"expr":"lit","value":"card"}}}}"#,
    );
    assert_eq!(rendered, r#"<div class="card"></div>"#);
}

#[test]
fn conditionals_and_loops_render() {
    let rendered = render(
        r#"{"version":"1.0",
            "state":{"items":{"type":"list","initial":["a","b"]},
                     "show":{"type":"boolean","initial":true}},
            "view":{"kind":"if","condition":{"expr":"state","name":"show"},
                "then":{"kind":"element","tag":"ul","children":[
                  {"kind":"each","items":{"expr":"state","name":"items"},"as":"item","index":"i",
                   "body":{"kind":"element","tag":"li","children":[
                     {"kind":"text","value":{"expr":"concat","items":[
                        {"expr":"var","name":"i"},{"expr":"lit","value":":"},{"expr":"var","name":"item"}]}}]}}]},
                "else":{"kind":"element","tag":"p"}}}"#,
    );
    assert_eq!(rendered, "<ul><li>0:a</li><li>1:b</li></ul>");
}

#[test]
fn components_receive_params_and_render_their_slot_children() {
    let rendered = render(
        r#"{"version":"1.0",
            "components":{"Card":{"params":{"title":{"type":"string"}},
              "view":{"kind":"element","tag":"section","children":[
                {"kind":"element","tag":"h2","children":[
                  {"kind":"text","value":{"expr":"param","name":"title"}}]},
                {"kind":"slot"}]}}},
            "view":{"kind":"component","name":"Card",
              "props":{"title":{"expr":"lit","value":"Hello"}},
              "children":[{"kind":"text","value":{"expr":"lit","value":"body"}}]}}"#,
    );
    assert_eq!(rendered, "<section><h2>Hello</h2>body</section>");
}

#[test]
fn slot_children_are_evaluated_in_the_callers_scope_not_the_components() {
    // The `var` in the slot content reads the loop the *invocation* sits in.
    // A component body cannot see that loop, so getting this wrong renders
    // empty rather than loudly failing — hence the explicit test.
    let rendered = render(
        r#"{"version":"1.0",
            "state":{"rows":{"type":"list","initial":["x","y"]}},
            "components":{"Wrap":{"view":{"kind":"element","tag":"b","children":[{"kind":"slot"}]}}},
            "view":{"kind":"each","items":{"expr":"state","name":"rows"},"as":"row",
              "body":{"kind":"component","name":"Wrap","children":[
                {"kind":"text","value":{"expr":"var","name":"row"}}]}}}"#,
    );
    assert_eq!(rendered, "<b>x</b><b>y</b>");
}

#[test]
fn portals_are_collected_rather_than_spliced_into_the_body() {
    let document = document(
        r#"{"version":"1.0","view":{"kind":"element","tag":"div","children":[
            {"kind":"text","value":{"expr":"lit","value":"body"}},
            {"kind":"portal","target":"head","children":[
              {"kind":"element","tag":"p","children":[
                {"kind":"text","value":{"expr":"lit","value":"in head"}}]}]}]}}"#,
    );
    let ctx = RenderContext {
        state: document.initial_state(),
        ..RenderContext::default()
    };
    let ui = document.render(&ctx).expect("renders");
    assert_eq!(ui.body.0, "<div>body</div>");
    assert_eq!(ui.portals.len(), 1);
    assert_eq!(ui.portals_for("head")[0].content.0, "<p>in head</p>");
    assert!(ui.portals_for("body").is_empty());
}

#[test]
fn route_expressions_read_the_supplied_route_values() {
    let document = document(
        r#"{"version":"1.0",
            "route":{"title":{"expr":"concat","items":[
              {"expr":"lit","value":"Post "},{"expr":"route","name":"id"}]},
              "meta":{"description":{"expr":"route","name":"q","source":"query"}}},
            "view":{"kind":"text","value":{"expr":"route","name":"missing","source":"query"}}}"#,
    );
    let mut route = RouteValues {
        path: "/posts/7".to_string(),
        ..RouteValues::default()
    };
    route.params.insert("id".into(), "7".into());
    route.query.insert("q".into(), "hello".into());

    let ctx = RenderContext {
        state: document.initial_state(),
        route,
        ..RenderContext::default()
    };
    let ui = document.render(&ctx).expect("renders");
    assert_eq!(ui.title.as_deref(), Some("Post 7"));
    assert_eq!(ui.meta["description"], "hello");
    // An absent query parameter renders as nothing, not as "null".
    assert_eq!(ui.body.0, "");
}

#[test]
fn event_bindings_render_as_data_attributes_for_the_host_to_wire() {
    let rendered = render(
        r#"{"version":"1.0",
            "state":{"count":{"type":"number","initial":0}},
            "actions":[{"name":"add","steps":[
              {"do":"update","target":"count","operation":"increment"}]}],
            "view":{"kind":"element","tag":"button","props":{
              "onClick":{"event":"click","action":"add","debounce":250,
                         "payload":{"by":{"expr":"lit","value":2}}}}}}"#,
    );
    assert!(
        rendered.contains(r#"data-constela-on-click="add""#),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"data-constela-payload-click="{&quot;by&quot;:2}""#),
        "{rendered}"
    );
    assert!(
        rendered.contains(r#"data-constela-debounce-click="250""#),
        "{rendered}"
    );
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

#[test]
fn a_loop_over_a_large_runtime_list_is_bounded() {
    let document = document(
        r#"{"version":"1.0",
            "state":{"items":{"type":"list","initial":[]}},
            "view":{"kind":"each","items":{"expr":"state","name":"items"},"as":"i",
              "body":{"kind":"element","tag":"li"}}}"#,
    );
    let mut state = document.initial_state();
    state.insert("items".into(), Value::Array(vec![json!(1); 10_000]));

    let ctx = RenderContext {
        state,
        limits: RenderLimits {
            max_each_items: 100,
            ..RenderLimits::default()
        },
        ..RenderContext::default()
    };
    let err = document.render(&ctx).expect_err("over the each limit");
    assert_eq!(err.diagnostics()[0].code, codes::RENDER_LIMIT);
}

#[test]
fn parse_limits_reject_an_oversized_document() {
    let limits = Limits {
        max_bytes: 32,
        ..Limits::default()
    };
    let err = Document::parse(&view(r#"{"kind":"element","tag":"div"}"#), &limits)
        .expect_err("over the byte limit");
    assert!(matches!(err, ConstelaError::Limit(_)));
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

#[test]
fn every_fault_is_reported_at_once_with_a_path_into_the_document() {
    let err = Document::parse(
        r#"{"version":"1.0","view":{"kind":"element","tag":"div","children":[
            {"kind":"element","tag":"iframe"},
            {"kind":"element","tag":"a","props":{"href":{"expr":"lit","value":"javascript:x"}}},
            {"kind":"text","value":{"expr":"state","name":"nope"}}]}}"#,
        &Limits::default(),
    )
    .expect_err("three faults");

    let report = err.to_json();
    let entries = report.as_array().expect("array");
    assert_eq!(entries.len(), 3, "{report:#}");

    let paths: Vec<&str> = entries
        .iter()
        .map(|entry| entry["path"].as_str().expect("path"))
        .collect();
    assert_eq!(
        paths,
        vec![
            "view.children[0]",
            "view.children[1].props.href",
            "view.children[2].value"
        ]
    );
    let codes: Vec<&str> = entries
        .iter()
        .map(|entry| entry["code"].as_str().expect("code"))
        .collect();
    assert_eq!(
        codes,
        vec![
            codes::TAG_NOT_ALLOWED,
            codes::URL_SCHEME,
            codes::UNKNOWN_REF
        ]
    );
}

#[test]
fn an_unsupported_expression_form_names_the_supported_ones() {
    let err = Document::parse(
        &view(r#"{"kind":"text","value":{"expr":"call","method":"map"}}"#),
        &Limits::default(),
    )
    .expect_err("`call` is not in the supported subset");
    assert!(matches!(err, ConstelaError::Syntax(_)));
    let message = &err.diagnostics()[0].message;
    assert!(message.contains("call"), "{message}");
    assert!(message.contains("concat"), "{message}");
}

#[test]
fn a_constela_error_becomes_a_422_not_a_500() {
    use autumn_web::error::AutumnError;

    let err = Document::parse("{", &Limits::default()).expect_err("bad json");
    let autumn: AutumnError = err.into();
    assert_eq!(autumn.status(), http::StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------------------------------------------------------------------------
// The server-side action interpreter
// ---------------------------------------------------------------------------

/// The counter from the Constela README, as an Autumn app would serve it.
const COUNTER: &str = r#"{
    "version": "1.0",
    "state": { "count": { "type": "number", "initial": 0 } },
    "actions": [
      { "name": "increment", "steps": [{ "do": "update", "target": "count", "operation": "increment" }] },
      { "name": "decrement", "steps": [{ "do": "update", "target": "count", "operation": "decrement" }] }
    ],
    "view": {
      "kind": "element", "tag": "div",
      "children": [
        { "kind": "text", "value": { "expr": "lit", "value": "Count: " } },
        { "kind": "text", "value": { "expr": "state", "name": "count" } },
        { "kind": "element", "tag": "button",
          "props": { "onClick": { "event": "click", "action": "increment" } },
          "children": [{ "kind": "text", "value": { "expr": "lit", "value": "+" } }] }
      ]
    }
  }"#;

#[test]
fn dispatch_and_rerender_is_a_working_interaction_loop() {
    let document = document(COUNTER);
    let mut state = document.initial_state();

    let render_now = |state: &Map<String, Value>| {
        let ctx = RenderContext {
            state: state.clone(),
            ..RenderContext::default()
        };
        document.render(&ctx).expect("renders").body.0
    };

    assert!(render_now(&state).contains("Count: 0"));

    for expected in 1..=3 {
        let outcome = document
            .dispatch("increment", &mut state, &Map::new())
            .expect("dispatches");
        assert!(outcome.is_pure(), "{outcome:?}");
        assert_eq!(state["count"], json!(expected));
        assert!(render_now(&state).contains(&format!("Count: {expected}")));
    }

    document
        .dispatch("decrement", &mut state, &Map::new())
        .expect("dispatches");
    assert_eq!(state["count"], json!(2));
}

#[test]
fn a_payload_is_in_scope_for_the_actions_expressions() {
    let document = document(
        r#"{"version":"1.0",
            "state":{"query":{"type":"string","initial":""}},
            "actions":[{"name":"search","steps":[
              {"do":"set","target":"query","value":{"expr":"var","name":"value"}}]}],
            "view":{"kind":"element","tag":"input"}}"#,
    );
    let mut state = document.initial_state();
    let mut payload = Map::new();
    payload.insert("value".into(), json!("autumn"));

    document
        .dispatch("search", &mut state, &payload)
        .expect("dispatches");
    assert_eq!(state["query"], json!("autumn"));
}

#[test]
fn browser_side_steps_are_reported_rather_than_performed() {
    let document = document(
        r#"{"version":"1.0",
            "state":{"data":{"type":"object","initial":{}},
                     "loaded":{"type":"boolean","initial":false}},
            "actions":[{"name":"load","steps":[
              {"do":"fetch","url":{"expr":"lit","value":"https://example.com/api"},
               "method":"POST","result":"res",
               "onSuccess":[{"do":"set","target":"loaded","value":{"expr":"lit","value":true}}]}]}],
            "view":{"kind":"element","tag":"div"}}"#,
    );
    let mut state = document.initial_state();
    let outcome = document
        .dispatch("load", &mut state, &Map::new())
        .expect("dispatches");

    assert_eq!(outcome.effects.len(), 1);
    match &outcome.effects[0] {
        Effect::Fetch { url, result, .. } => {
            assert_eq!(url, "https://example.com/api");
            assert_eq!(result.as_deref(), Some("res"));
        }
        other => panic!("expected a fetch effect, got {other:?}"),
    }
    // The server did not make the request, so it must not have guessed which
    // branch the browser would have taken.
    assert_eq!(state["loaded"], json!(false));
}

#[test]
fn a_conditional_step_runs_only_the_branch_it_selects() {
    let document = document(
        r#"{"version":"1.0",
            "state":{"n":{"type":"number","initial":10},
                     "label":{"type":"string","initial":""}},
            "actions":[{"name":"classify","steps":[
              {"do":"if","condition":{"expr":"bin","op":">","left":{"expr":"state","name":"n"},
                                      "right":{"expr":"lit","value":5}},
               "then":[{"do":"set","target":"label","value":{"expr":"lit","value":"big"}}],
               "else":[{"do":"set","target":"label","value":{"expr":"lit","value":"small"}}]}]}],
            "view":{"kind":"element","tag":"div"}}"#,
    );
    let mut state = document.initial_state();
    document
        .dispatch("classify", &mut state, &Map::new())
        .expect("dispatches");
    assert_eq!(state["label"], json!("big"));

    state.insert("n".into(), json!(1));
    document
        .dispatch("classify", &mut state, &Map::new())
        .expect("dispatches");
    assert_eq!(state["label"], json!("small"));
}

#[test]
fn dispatching_an_undeclared_action_is_an_error_not_a_silent_no_op() {
    let document = document(COUNTER);
    let mut state = document.initial_state();
    let err = document
        .dispatch("drop_tables", &mut state, &Map::new())
        .expect_err("no such action");
    assert_eq!(err.diagnostics()[0].code, codes::UNKNOWN_REF);
}

#[test]
fn a_navigate_step_cannot_smuggle_a_javascript_url_through_state() {
    let document = document(
        r#"{"version":"1.0",
            "state":{"target":{"type":"string","initial":"/safe"}},
            "actions":[{"name":"go","steps":[
              {"do":"navigate","url":{"expr":"state","name":"target"}}]}],
            "view":{"kind":"element","tag":"div"}}"#,
    );
    let mut state = document.initial_state();
    document
        .dispatch("go", &mut state, &Map::new())
        .expect("a relative URL is fine");

    state.insert("target".into(), json!("javascript:alert(1)"));
    let err = document
        .dispatch("go", &mut state, &Map::new())
        .expect_err("computed javascript: URL");
    assert_eq!(err.diagnostics()[0].code, codes::URL_SCHEME);
}

// ---------------------------------------------------------------------------
// The `markdown` node kind, which needs autumn-web's `markdown` feature
// ---------------------------------------------------------------------------
//
// The JSON below uses `r##"..."##`: a Markdown heading is `"# ` inside a JSON
// string, and `"#` would close a single-hash raw string.

/// A `markdown` node with the feature off must fail *validation*, not render
/// nothing. Silently dropping content because of a build flag is the failure
/// mode this diagnostic exists to prevent.
#[cfg(not(feature = "markdown"))]
#[test]
fn a_markdown_node_without_the_feature_is_a_diagnostic_not_silence() {
    let codes = codes_of(&view(
        r##"{"kind":"markdown","content":{"expr":"lit","value":"# hi"}}"##,
    ));
    assert!(codes.contains(&codes::FEATURE_REQUIRED), "{codes:?}");
}

#[cfg(feature = "markdown")]
#[test]
fn a_markdown_node_renders_through_the_user_content_sanitizer() {
    let rendered = render(&view(
        r##"{"kind":"markdown","content":{"expr":"lit","value":"# Title\n\nSome **bold** text."}}"##,
    ));
    assert!(rendered.contains("<h1"), "{rendered}");
    assert!(rendered.contains("<strong>bold</strong>"), "{rendered}");
}

/// The whole reason the `markdown` node routes through
/// `markdown::render_user_content` rather than a plain Markdown renderer: its
/// source is a document a model wrote, so raw HTML in it must not survive.
#[cfg(feature = "markdown")]
#[test]
fn markdown_content_cannot_smuggle_raw_html_through() {
    let payload = concat!(
        "<script>alert(1)</script>\\n\\n",
        "<img src=x onerror=alert(1)>\\n\\n",
        "[click](javascript:alert(1))"
    );
    let rendered = render(&view(&format!(
        r#"{{"kind":"markdown","content":{{"expr":"lit","value":"{payload}"}}}}"#
    )));
    for marker in FORBIDDEN_MARKUP {
        assert!(
            !rendered.contains(marker),
            "{marker} survived as live markup in {rendered}"
        );
    }
    // The payloads survive as *escaped text*, which is the distinction that
    // matters: `&lt;img ... onerror=...&gt;` is characters on the page, not an
    // element with a handler. Asserting they are present, rather than merely
    // that the tag markers are absent, is what proves the content was escaped
    // rather than silently dropped.
    assert!(rendered.contains("&lt;script&gt;"), "{rendered}");
    assert!(rendered.contains("&lt;img src=x onerror="), "{rendered}");
    // The `javascript:` link was degraded to its own text, destination gone.
    assert!(rendered.contains("<p>click</p>"), "{rendered}");
}

/// A prop *shaped like* an event handler is a binding, whatever it is named —
/// so a document naming one `onclick` must still not produce an `onclick`
/// attribute. The allowlist check only sees attribute-valued props, which is
/// exactly why this path needs its own assertion.
#[test]
fn a_handler_named_onclick_still_renders_only_as_a_data_attribute() {
    let rendered = render(
        r#"{"version":"1.0",
            "state":{"n":{"type":"number","initial":0}},
            "actions":[{"name":"bump","steps":[
              {"do":"update","target":"n","operation":"increment"}]}],
            "view":{"kind":"element","tag":"button","props":{
              "onclick":{"event":"click","action":"bump"}}}}"#,
    );
    assert_eq!(
        rendered,
        r#"<button data-constela-on-click="bump"></button>"#
    );
}

#[test]
fn a_document_cannot_forge_the_renderers_own_data_attributes() {
    let codes = codes_of(&view(
        r#"{"kind":"element","tag":"div","props":{
            "data-constela-on-click":{"expr":"lit","value":"forged"}}}"#,
    ));
    assert!(codes.contains(&codes::ATTR_NOT_ALLOWED), "{codes:?}");
}

/// Every AST type derives `Serialize` as well as `Deserialize`, so an app can
/// store a validated document, hand it to another service, or show it back to
/// the model that wrote it. That direction has no other caller in the crate, so
/// without this it would be derived-and-never-exercised — and a wrong `rename`
/// on an internally-tagged enum variant fails silently in exactly that
/// direction.
#[test]
fn a_document_round_trips_through_serialization() {
    // One of each construct whose serialized spelling is renamed: an operator,
    // an update operation, a node kind in camelCase, a keyword-clashing field.
    let source = r#"{
        "version":"1.0",
        "route":{"path":"/p/{id}","title":{"expr":"route","name":"id","source":"param"}},
        "styles":{"btn":{"base":"b","variants":{"size":{"sm":"s"}},"defaultVariants":{"size":"sm"}}},
        "lifecycle":{"onMount":"load"},
        "state":{"n":{"type":"number","initial":0},"items":{"type":"list","initial":[]}},
        "actions":[{"name":"load","steps":[
          {"do":"update","target":"items","operation":"replaceAt",
           "index":{"expr":"lit","value":0},"value":{"expr":"lit","value":1}},
          {"do":"setPath","target":"items","path":{"expr":"lit","value":"0"},
           "value":{"expr":"lit","value":2}},
          {"do":"if","condition":{"expr":"bin","op":">=","left":{"expr":"state","name":"n"},
                                  "right":{"expr":"lit","value":1}},
           "then":[{"do":"set","target":"n","value":{"expr":"not","operand":{"expr":"lit","value":false}}}],
           "else":[]},
          {"do":"fetch","url":{"expr":"lit","value":"/api"},"method":"POST","result":"r"}]}],
        "components":{"C":{"params":{"p":{"type":"string","required":false}},
                           "view":{"kind":"text","value":{"expr":"param","name":"p"}}}},
        "view":{"kind":"errorBoundary",
          "fallback":{"kind":"text","value":{"expr":"lit","value":"oops"}},
          "content":{"kind":"element","tag":"div","props":{
            "class":{"expr":"style","name":"btn","variants":{"size":{"expr":"lit","value":"sm"}}},
            "onClick":{"event":"click","action":"load","payload":{"by":{"expr":"lit","value":1}}}},
            "children":[
              {"kind":"each","items":{"expr":"state","name":"items"},"as":"it","index":"i",
               "key":{"expr":"var","name":"i"},
               "body":{"kind":"component","name":"C","props":{"p":{"expr":"var","name":"it"}}}},
              {"kind":"island","id":"is1","strategy":"visible",
               "content":{"kind":"suspense","id":"s1",
                 "fallback":{"kind":"text","value":{"expr":"lit","value":"…"}},
                 "content":{"kind":"portal","target":"head","children":[]}}}]}}
    }"#;

    let document = document(source);
    let json = serde_json::to_string(document.program()).expect("serializes");

    // Round-tripping through the *validating* parser is the assertion that
    // matters: a variant that serialized under the wrong name would no longer
    // deserialize, and one that serialized under a name meaning something else
    // would fail validation on the way back.
    let reparsed = Document::parse(&json, &Limits::default()).expect("re-parses");
    assert_eq!(reparsed.program(), document.program());
}

/// A `setPath` whose path is absurdly deep is refused, not written.
///
/// The write itself is iterative and would survive; the *structure* it creates
/// would not. `serde_json::Value` drops recursively, so a two-hundred-thousand
/// level object overflows the stack whenever that state is next freed — a crash
/// with no visible connection to the request that caused it. The guard is what
/// makes the depth of generated state bounded rather than merely the walk over
/// it.
#[test]
fn a_setpath_deeper_than_the_limit_is_refused_rather_than_written() {
    let document = document(
        r#"{"version":"1.0",
            "state":{"tree":{"type":"object","initial":{}},
                     "path":{"type":"string","initial":""}},
            "actions":[{"name":"write","steps":[
              {"do":"setPath","target":"tree","path":{"expr":"state","name":"path"},
               "value":{"expr":"lit","value":1}}]}],
            "view":{"kind":"element","tag":"div"}}"#,
    );

    let mut state = document.initial_state();

    // A reasonable path is written.
    state.insert("path".into(), json!("a.b.c"));
    document
        .dispatch("write", &mut state, &Map::new())
        .expect("a short path is fine");
    assert_eq!(state["tree"], json!({"a": {"b": {"c": 1}}}));

    // A pathological one is refused, and leaves the state alone.
    let before = state["tree"].clone();
    let deep = vec!["a"; 200_000].join(".");
    state.insert("path".into(), Value::String(deep));
    let err = document
        .dispatch("write", &mut state, &Map::new())
        .expect_err("over the depth limit");
    assert_eq!(err.diagnostics()[0].code, codes::RENDER_LIMIT);
    assert_eq!(state["tree"], before);
}

// ---------------------------------------------------------------------------
// Regressions from the Codex review on #2623
// ---------------------------------------------------------------------------

/// A long *chain* of components is not bounded by any parse limit that looks at
/// nesting: components are sibling entries in one shallow map, so `A -> B -> C`
/// a few thousand deep costs a handful of JSON nodes each and passes the size,
/// depth and node bounds. Cycle detection therefore has to be iterative — a
/// recursive DFS would take one stack frame per link and overflow the request
/// thread during *validation*, before any render guard could see it.
#[test]
fn a_long_component_chain_validates_without_overflowing_the_stack() {
    const LINKS: usize = 4_000;

    let mut components = Vec::with_capacity(LINKS);
    for i in 0..LINKS {
        // Each link references the next; the last renders text and stops.
        let body = if i + 1 < LINKS {
            format!(r#"{{"kind":"component","name":"C{}"}}"#, i + 1)
        } else {
            r#"{"kind":"text","value":{"expr":"lit","value":"end"}}"#.to_string()
        };
        components.push(format!(r#""C{i}":{{"view":{body}}}"#));
    }
    let source = format!(
        r#"{{"version":"1.0","components":{{{}}},"view":{{"kind":"component","name":"C0"}}}}"#,
        components.join(",")
    );

    // Well inside the parse bounds, which is exactly the point.
    let document = Document::parse(&source, &Limits::unbounded()).expect("validates");
    assert_eq!(document.program().components.len(), LINKS);
}

/// The same chain, closed into a cycle, is still *detected* — making the walk
/// iterative must not cost the check it exists to perform.
#[test]
fn a_long_component_cycle_is_still_detected() {
    const LINKS: usize = 2_000;

    let mut components = Vec::with_capacity(LINKS);
    for i in 0..LINKS {
        let next = (i + 1) % LINKS; // the last one points back at C0
        components.push(format!(
            r#""C{i}":{{"view":{{"kind":"component","name":"C{next}"}}}}"#
        ));
    }
    let source = format!(
        r#"{{"version":"1.0","components":{{{}}},"view":{{"kind":"component","name":"C0"}}}}"#,
        components.join(",")
    );

    let err = Document::parse(&source, &Limits::unbounded()).expect_err("cycle");
    assert_eq!(err.diagnostics()[0].code, codes::CYCLE);
}

/// Counting nodes does not bound bytes. A handful of nodes can emit gigabytes:
/// one big string in state, rendered once per iteration of an `each`.
#[test]
fn rendered_output_is_capped_in_bytes_not_just_nodes() {
    let document = document(
        r#"{"version":"1.0",
            "state":{"blob":{"type":"string","initial":""},
                     "rows":{"type":"list","initial":[]}},
            "view":{"kind":"each","items":{"expr":"state","name":"rows"},"as":"r",
              "body":{"kind":"text","value":{"expr":"state","name":"blob"}}}}"#,
    );

    let mut state = document.initial_state();
    state.insert("blob".into(), Value::String("x".repeat(64 * 1024)));
    state.insert("rows".into(), Value::Array(vec![json!(1); 1_000]));

    // 1 000 nodes and 1 000 iterations — inside both of those budgets — but
    // ~64 MiB of markup.
    let ctx = RenderContext {
        state,
        ..RenderContext::default()
    };
    let err = document.render(&ctx).expect_err("over the byte budget");
    assert_eq!(err.diagnostics()[0].code, codes::RENDER_LIMIT);
    assert!(
        err.diagnostics()[0].message.contains("bytes"),
        "{:?}",
        err.diagnostics()[0].message
    );
}

/// Dispatch must not run past an effect it did not perform. The `set` below
/// reads the `fetch`'s `result`, which the server has no value for — running it
/// would write `null` over good data and call that a state transition.
#[test]
fn dispatch_stops_at_an_effect_rather_than_binding_its_result_to_null() {
    let document = document(
        r#"{"version":"1.0",
            "state":{"data":{"type":"object","initial":{"kept":true}}},
            "actions":[{"name":"load","steps":[
              {"do":"fetch","url":{"expr":"lit","value":"/api"},"result":"res"},
              {"do":"set","target":"data","value":{"expr":"var","name":"res"}}]}],
            "view":{"kind":"element","tag":"div"}}"#,
    );

    let mut state = document.initial_state();
    let outcome = document
        .dispatch("load", &mut state, &Map::new())
        .expect("dispatches");

    assert!(outcome.is_suspended(), "{outcome:?}");
    assert_eq!(outcome.effects.len(), 1);
    // The critical assertion: the untaken step left the good value alone.
    assert_eq!(state["data"], json!({"kept": true}));
}

/// Suspension propagates out of an `if` branch, not just a top-level list.
#[test]
fn dispatch_suspension_propagates_out_of_a_branch() {
    let document = document(
        r#"{"version":"1.0",
            "state":{"flag":{"type":"boolean","initial":true},
                     "note":{"type":"string","initial":"kept"}},
            "actions":[{"name":"go","steps":[
              {"do":"if","condition":{"expr":"state","name":"flag"},
               "then":[{"do":"navigate","url":{"expr":"lit","value":"/next"}}],
               "else":[]},
              {"do":"set","target":"note","value":{"expr":"lit","value":"ran anyway"}}]}],
            "view":{"kind":"element","tag":"div"}}"#,
    );

    let mut state = document.initial_state();
    let outcome = document
        .dispatch("go", &mut state, &Map::new())
        .expect("dispatches");

    assert!(outcome.is_suspended(), "{outcome:?}");
    assert_eq!(state["note"], json!("kept"));
}

/// An `id` and a link to it must stay a matched pair. Prefixing one and not the
/// other breaks in-fragment navigation and lets `#section` resolve against the
/// host page instead.
#[test]
fn a_fragment_link_is_prefixed_to_match_the_id_it_targets() {
    // `r##"…"##`: the JSON below contains `"#`, which would close a
    // single-hash raw string.
    let rendered = render(
        r##"{"version":"1.0","view":{"kind":"element","tag":"div","children":[
            {"kind":"element","tag":"a","props":{"href":{"expr":"lit","value":"#section"}}},
            {"kind":"element","tag":"section","props":{"id":{"expr":"lit","value":"section"}}},
            {"kind":"element","tag":"a","props":{"href":{"expr":"lit","value":"/other#section"}}}]}}"##,
    );

    assert!(rendered.contains(r##"href="#c-section""##), "{rendered}");
    assert!(rendered.contains(r#"id="c-section""#), "{rendered}");
    // A cross-document fragment points at ids this render did not write.
    assert!(rendered.contains(r#"href="/other#section""#), "{rendered}");
}
