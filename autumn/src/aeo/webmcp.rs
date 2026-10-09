//! `WebMCP`: let in-browser AI agents call this site's MCP tools.
//!
//! Autumn serves a small script at [`WEBMCP_JS_PATH`]. On page load it calls
//! `document.modelContext.registerTool()` once for each public MCP tool. A
//! tool call goes to the MCP endpoint with the page's cookies and CSRF token,
//! so the normal route pipeline (auth, CSRF, rate limits) applies.
//!
//! Add the script to the page `<head>` with [`head_tags`]. The scaffolded
//! layout does this.

use serde_json::{Value, json};

use super::documents::SiteFacts;

/// Path of the `WebMCP` script.
pub const WEBMCP_JS_PATH: &str = "/_autumn/webmcp.js";

/// The `WebMCP` script for `facts`. With no public MCP tools it does nothing.
#[must_use]
pub(crate) fn script(facts: &SiteFacts, csrf_header: &str) -> String {
    let (endpoint, tools) = facts.mcp.as_ref().map_or_else(
        || (String::new(), Vec::new()),
        |mcp| {
            let tools = mcp
                .tools
                .iter()
                .filter(|_| mcp.public_tools)
                .filter(|t| valid_tool_name(&t.name))
                // An in-browser agent runs with the visitor's session, so
                // only a tool marked safe goes on the page.
                .filter(|t| marked_safe(&t.annotations))
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description.clone().unwrap_or_else(|| t.name.clone()),
                        "inputSchema": t.input_schema,
                        "annotations": webmcp_annotations(&t.annotations),
                    })
                })
                .collect();
            (mcp.path.clone(), tools)
        },
    );
    SCRIPT
        .replace("__ENDPOINT__", &js_json(&json!(endpoint)))
        .replace("__CSRF_HEADER__", &js_json(&json!(csrf_header)))
        .replace("__TOOLS__", &js_json(&Value::Array(tools)))
}

/// JSON safe to put inside a `<script>`: `<`, `>`, and `&` are escaped, so
/// no data can close the script element.
fn js_json(value: &Value) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "null".to_owned())
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

const SCRIPT: &str = r#"// Autumn WebMCP: register this site's MCP tools with in-browser agents.
(() => {
  "use strict";
  const mc = document.modelContext || navigator.modelContext;
  if (!mc || typeof mc.registerTool !== "function") return;
  const endpoint = __ENDPOINT__;
  const csrfHeader = __CSRF_HEADER__;
  const tools = __TOOLS__;
  let nextId = 0;
  const parse = (text) => {
    try {
      return JSON.parse(text);
    } catch (_) {
      const data = text.split("\n").filter((l) => l.startsWith("data:"));
      return JSON.parse(data[data.length - 1].slice(5));
    }
  };
  for (const t of tools) {
    const execute = async (input) => {
      const headers = {
        "content-type": "application/json",
        accept: "application/json, text/event-stream",
        "mcp-protocol-version": "2025-06-18",
      };
      const meta = document.querySelector('meta[name="csrf-token"]');
      if (meta && meta.content) headers[csrfHeader] = meta.content;
      const res = await fetch(endpoint, {
        method: "POST",
        credentials: "same-origin",
        headers,
        body: JSON.stringify({
          jsonrpc: "2.0",
          id: ++nextId,
          method: "tools/call",
          params: { name: t.name, arguments: input || {} },
        }),
      });
      const msg = parse(await res.text());
      if (msg.error) throw new Error(msg.error.message || "MCP error");
      return msg.result;
    };
    try {
      const done = mc.registerTool({ ...t, execute });
      if (done && typeof done.catch === "function") done.catch(() => {});
    } catch (_) {}
  }
})();
"#;

/// `true` for a valid `WebMCP` tool name: 1–128 of `A-Z a-z 0-9 _ . -`.
#[must_use]
pub(crate) fn valid_tool_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Map MCP annotations to `WebMCP` annotations.
#[must_use]
pub(crate) fn webmcp_annotations(mcp: &Value) -> Value {
    let read_only = hint(mcp, "readOnlyHint") == Some(true);
    json!({
        "readOnlyHint": read_only,
        // A write needs the visitor's consent unless it is read-only.
        "consequentialHint": !read_only || hint(mcp, "destructiveHint") == Some(true),
    })
}

/// `true` when the tool is read-only, or says it is not destructive. A
/// missing hint is not safety.
fn marked_safe(mcp: &Value) -> bool {
    hint(mcp, "destructiveHint") != Some(true)
        && (hint(mcp, "readOnlyHint") == Some(true) || hint(mcp, "destructiveHint") == Some(false))
}

fn hint(mcp: &Value, key: &str) -> Option<bool> {
    mcp.get(key).and_then(Value::as_bool)
}

/// `<head>` tags for agents: the ARD link and the `WebMCP` script.
///
/// Pass the request's CSP nonce when the page uses a nonce-based policy.
///
/// ```rust,ignore
/// html! { head { (autumn_web::aeo::webmcp::head_tags(None)) } }
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn head_tags(csp_nonce: Option<&str>) -> maud::Markup {
    maud::html! {
        link rel="ai-catalog" href=(super::AI_CATALOG_PATH) type="application/json";
        script src=(WEBMCP_JS_PATH) defer nonce=[csp_nonce] {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aeo::documents::{McpFacts, ToolFacts};

    fn facts(public: bool) -> SiteFacts {
        SiteFacts {
            mcp: Some(McpFacts {
                path: "/mcp".to_owned(),
                tools: vec![
                    ToolFacts {
                        name: "list_todos".to_owned(),
                        description: Some("List </script> todos".to_owned()),
                        input_schema: json!({"type": "object"}),
                        annotations: json!({"readOnlyHint": true, "destructiveHint": false}),
                    },
                    ToolFacts {
                        name: "bad name!".to_owned(),
                        description: None,
                        input_schema: json!({}),
                        annotations: json!({}),
                    },
                ],
                public_tools: public,
            }),
            ..SiteFacts::default()
        }
    }

    #[test]
    fn script_registers_each_valid_tool() {
        let js = script(&facts(true), "x-csrf-token");
        assert!(js.contains("registerTool"), "{js}");
        assert!(
            js.contains("document.modelContext || navigator.modelContext"),
            "{js}"
        );
        assert!(js.contains("\"list_todos\""), "{js}");
        assert!(!js.contains("bad name!"), "invalid names are skipped: {js}");
        assert!(js.contains("\"/mcp\""), "{js}");
        assert!(js.contains("\"x-csrf-token\""), "{js}");
        assert!(
            !js.contains("</script>"),
            "no raw end tag in the script: {js}"
        );
    }

    #[test]
    fn destructive_tools_are_not_registered() {
        let mut f = facts(true);
        f.mcp.as_mut().unwrap().tools[0].annotations = json!({"destructiveHint": true});
        assert!(!script(&f, "x-csrf-token").contains("list_todos"));
    }

    #[test]
    fn only_tools_marked_safe_are_registered() {
        // A POST tool has no `destructiveHint`: absence is not safety.
        let mut f = facts(true);
        f.mcp.as_mut().unwrap().tools[0].annotations = json!({"readOnlyHint": false});
        assert!(!script(&f, "x-csrf-token").contains("list_todos"));

        f.mcp.as_mut().unwrap().tools[0].annotations =
            json!({"readOnlyHint": false, "destructiveHint": false});
        let js = script(&f, "x-csrf-token");
        assert!(js.contains("list_todos"), "{js}");
        assert!(
            js.contains("\"consequentialHint\":true"),
            "a write needs consent: {js}"
        );
    }

    #[test]
    fn script_lines_never_open_a_string_across_a_newline() {
        // A JS string cannot hold a raw newline. An odd count of `"` on a
        // line means one string runs past the line end.
        for line in script(&facts(true), "x-csrf-token").lines() {
            assert_eq!(line.matches('"').count() % 2, 0, "{line}");
        }
    }

    #[test]
    fn script_is_a_no_op_without_public_tools() {
        for f in [facts(false), SiteFacts::default()] {
            let js = script(&f, "x-csrf-token");
            assert!(js.contains("const tools = [];"), "{js}");
        }
    }

    #[test]
    fn tool_names() {
        assert!(valid_tool_name("search_flights.v2-a"));
        assert!(!valid_tool_name(""));
        assert!(!valid_tool_name("Search flights"));
        assert!(!valid_tool_name(&"a".repeat(129)));
    }

    #[test]
    fn annotations_map() {
        assert_eq!(
            webmcp_annotations(&json!({"readOnlyHint": true, "destructiveHint": true})),
            json!({"readOnlyHint": true, "consequentialHint": true})
        );
        assert_eq!(
            webmcp_annotations(&json!({"readOnlyHint": true})),
            json!({"readOnlyHint": true, "consequentialHint": false})
        );
        assert_eq!(
            webmcp_annotations(&json!({})),
            json!({"readOnlyHint": false, "consequentialHint": true})
        );
    }

    #[cfg(feature = "maud")]
    #[test]
    fn head_tags_render_the_script_and_the_ard_link() {
        let html = head_tags(Some("abc")).into_string();
        assert!(
            html.contains("<script src=\"/_autumn/webmcp.js\" defer nonce=\"abc\">"),
            "{html}"
        );
        assert!(html.contains("rel=\"ai-catalog\""), "{html}");
    }
}
