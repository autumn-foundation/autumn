mod security;

use autumn_web::htmx::{HtmxFragments, OobSwap, inject_hx_swap_oob};
use maud::html;

#[test]
fn htmx_fragments_injection_poc() {
    let oob_value = "innerHTML:#\"><script>alert(1)</script><template hx-swap-oob=\"";

    // Test 1: HtmxFragments::oob
    let markup = html! { div id="test" { "content" } };
    let fragments = HtmxFragments::oob_only().oob_with_strategy(
        "test",
        OobSwap::Custom(oob_value.to_string()),
        markup,
    );
    let mut w = String::new();
    maud::Render::render_to(&fragments, &mut w);

    assert!(
        !w.contains("<script>alert(1)</script>"),
        "Injection successful via HtmxFragments: {}",
        w
    );

    // Test 2: inject_hx_swap_oob
    let html_str = "<div class=\"foo\">bar</div>";
    let injected = inject_hx_swap_oob(html_str, oob_value).unwrap();
    assert!(
        !injected.contains("<script>alert(1)</script>"),
        "Injection successful via inject_hx_swap_oob: {}",
        injected
    );
}

#[test]
fn htmx_headers_injection_poc() {
    let invalid_header_value = "invalid\r\nSet-Cookie: pwn=1";
    // Based on how hx_response_ext_ignores_invalid_header_values is tested
    // The framework calls HeaderValue::from_str which fails with newlines
    let res = axum::http::HeaderValue::from_str(invalid_header_value);
    assert!(
        res.is_err(),
        "Header injection bypass via newline is possible"
    );
}

#[test]
fn htmx_attribute_injection_poc() {
    let oob_value = "innerHTML:#test' onmouseover='alert(1)'";

    // Test 1: HtmxFragments::oob
    let markup = html! { div id="test" { "content" } };
    let fragments = HtmxFragments::oob_only().oob_with_strategy(
        "test",
        OobSwap::Custom(oob_value.to_string()),
        markup,
    );
    let mut w = String::new();
    maud::Render::render_to(&fragments, &mut w);

    assert!(
        !w.contains("onmouseover='alert(1)'"),
        "Attribute injection successful via HtmxFragments: {}",
        w
    );

    // Test 2: inject_hx_swap_oob
    let html_str = "<div class=\"foo\">bar</div>";
    let injected = inject_hx_swap_oob(html_str, oob_value).unwrap();
    assert!(
        !injected.contains("onmouseover='alert(1)'"),
        "Attribute injection successful via inject_hx_swap_oob: {}",
        injected
    );
}
