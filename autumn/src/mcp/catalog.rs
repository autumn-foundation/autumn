//! Progressive tool discovery.
//!
//! In progressive mode, `tools/list` holds two meta-tools and the core tools.
//! `search_tools` finds a tool in the full catalog. `call_tool` runs a tool
//! that the list does not show. A direct `tools/call` by name also works.

use serde_json::{Value, json};

use super::{McpTool, McpToolInfo};

/// The `_meta` key that tells a client to load a tool before any search.
/// Claude Code reads it. Other clients ignore it.
pub const ALWAYS_LOAD: &str = "anthropic/alwaysLoad";

/// The name of the meta-tool that searches the catalog.
pub const SEARCH_TOOL: &str = "search_tools";

/// The name of the meta-tool that runs a tool by name.
pub const CALL_TOOL: &str = "call_tool";

/// The number of hits `search_tools` returns when the call sets no `limit`.
const DEFAULT_LIMIT: u64 = 10;

/// The largest `limit` that `search_tools` accepts.
const MAX_LIMIT: u64 = 50;

/// The longest `query` that `search_tools` accepts, in bytes. This cap and
/// [`MAX_TERMS`] bound the work of one call.
const MAX_QUERY_BYTES: usize = 512;

/// The most distinct words that one `query` can hold.
const MAX_TERMS: usize = 16;

/// Guidance that progressive mode adds to the server instructions.
const GUIDANCE: &str = "The tool list shows the core tools only. \
    Call `search_tools` to find other tools. \
    Call `call_tool` to run a tool that the list does not show.";

/// How `tools/list` presents the tool catalog.
///
/// Set it with [`AppBuilder::mcp_catalog`](crate::app::AppBuilder::mcp_catalog).
///
/// ```rust,no_run
/// use autumn_web::mcp::McpCatalog;
///
/// let app = autumn_web::app()
///     .mcp_catalog(McpCatalog::Progressive)
///     .mount_mcp("/mcp");
/// # let _ = app;
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum McpCatalog {
    /// List every tool. This is the default.
    #[default]
    Full,
    /// List the core tools, `search_tools` and `call_tool`.
    ///
    /// Mark a core tool with `#[api_doc(mcp, core)]` or
    /// [`Route::mcp_core`](crate::route::Route::mcp_core). This mode is not
    /// access control. An agent can find and call each tool.
    Progressive,
}

/// Whether `name` is one of the meta-tools.
pub fn is_meta_tool(name: &str) -> bool {
    name == SEARCH_TOOL || name == CALL_TOOL
}

/// Remove each tool that has a meta-tool name, in progressive mode only.
///
/// The router calls this function before the server and the AEO documents
/// read the tools, so both see the same list.
pub fn drop_reserved(tools: Vec<McpToolInfo>, catalog: McpCatalog) -> Vec<McpToolInfo> {
    if catalog != McpCatalog::Progressive {
        return tools;
    }
    tools
        .into_iter()
        .filter(|tool| {
            let reserved = is_meta_tool(&tool.name);
            if reserved {
                tracing::warn!(
                    tool = %tool.name,
                    "skipping MCP tool: the progressive catalog reserves this name \
                     for a meta-tool (set a distinct operation_id to expose it)"
                );
            }
            !reserved
        })
        .collect()
}

/// The server instructions: the author's text, then the guidance of the
/// catalog mode. `None` when both are empty.
pub fn instructions(author: Option<&str>, catalog: McpCatalog) -> Option<String> {
    let guidance = (catalog == McpCatalog::Progressive).then_some(GUIDANCE);
    match (author.filter(|text| !text.trim().is_empty()), guidance) {
        (Some(text), Some(guidance)) => Some(format!("{text}\n\n{guidance}")),
        (Some(text), None) => Some(text.to_owned()),
        (None, Some(guidance)) => Some(guidance.to_owned()),
        (None, None) => None,
    }
}

/// The `tools/list` entries of the two meta-tools.
pub fn meta_descriptors(tools: &[McpTool]) -> Vec<Value> {
    let search = format!(
        "Search the tool catalog. {} It matches words against tool names, \
         descriptions, tags and argument names. An empty query lists every \
         tool. Use `detail = \"full\"` to get the input schema, then run the \
         tool with `call_tool`.",
        outline(tools)
    );
    vec![
        json!({
            "name": SEARCH_TOOL,
            "description": search,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "maxLength": MAX_QUERY_BYTES,
                        "description": "Words to match. Empty lists every tool.",
                    },
                    "detail": {
                        "type": "string",
                        "enum": ["name", "summary", "full"],
                        "default": "summary",
                        "description": "`name`: names only. `summary`: adds the description and annotations. `full`: adds the input schema.",
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "default": DEFAULT_LIMIT,
                    },
                },
            },
            "annotations": {
                "title": "Search tools",
                "readOnlyHint": true,
                "idempotentHint": true,
                "openWorldHint": false,
            },
            "_meta": { ALWAYS_LOAD: true },
        }),
        json!({
            "name": CALL_TOOL,
            "description": "Run a tool by name. The tool can be one that the \
                list does not show. Get its input schema from `search_tools` \
                first. The call runs with the same checks as a direct call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The tool name." },
                    "arguments": {
                        "type": "object",
                        "description": "The arguments of the tool, as its input schema gives.",
                    },
                },
                "required": ["name"],
            },
            // This descriptor cannot know the inner tool. It states the worst
            // case, so a client can ask the user before the call.
            "annotations": {
                "title": "Call a tool",
                "readOnlyHint": false,
                "destructiveHint": true,
            },
            "_meta": { ALWAYS_LOAD: true },
        }),
    ]
}

/// One sentence with the tool count, and the tags with their tool counts.
fn outline(tools: &[McpTool]) -> String {
    let mut tags: Vec<(&str, usize)> = Vec::new();
    for tag in tools.iter().flat_map(|tool| &tool.tags) {
        match tags.iter_mut().find(|(name, _)| *name == tag.as_str()) {
            Some((_, count)) => *count += 1,
            None => tags.push((tag, 1)),
        }
    }
    let count = tools.len();
    let noun = if count == 1 { "tool" } else { "tools" };
    if tags.is_empty() {
        return format!("The server has {count} {noun}.");
    }
    let tags = tags
        .iter()
        .map(|(name, count)| format!("{name} ({count})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("The server has {count} {noun}. Tags: {tags}.")
}

/// How much of each tool `search_tools` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Detail {
    Name,
    Summary,
    Full,
}

/// The checked arguments of a `search_tools` call.
#[derive(Debug)]
pub struct SearchArgs {
    terms: Vec<String>,
    detail: Detail,
    limit: usize,
}

/// Check the arguments of a `search_tools` call.
pub fn parse_search(arguments: &Value) -> Result<SearchArgs, String> {
    let query = match arguments.get("query") {
        None | Some(Value::Null) => "",
        Some(Value::String(query)) => query.as_str(),
        Some(_) => return Err("`query` must be a string".to_owned()),
    };
    if query.len() > MAX_QUERY_BYTES {
        return Err(format!("`query` must be at most {MAX_QUERY_BYTES} bytes"));
    }
    let mut terms: Vec<String> = Vec::new();
    for word in words(query) {
        if !terms.contains(&word) {
            terms.push(word);
        }
    }
    if terms.len() > MAX_TERMS {
        return Err(format!(
            "`query` must have at most {MAX_TERMS} distinct words"
        ));
    }
    let detail = match arguments.get("detail") {
        None | Some(Value::Null) => Detail::Summary,
        Some(value) => match value.as_str() {
            Some("name") => Detail::Name,
            Some("summary") => Detail::Summary,
            Some("full") => Detail::Full,
            _ => return Err("`detail` must be \"name\", \"summary\" or \"full\"".to_owned()),
        },
    };
    let limit = match arguments.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(value) => match value.as_u64() {
            Some(limit) if (1..=MAX_LIMIT).contains(&limit) => limit,
            _ => return Err(format!("`limit` must be an integer from 1 to {MAX_LIMIT}")),
        },
    };
    Ok(SearchArgs {
        terms,
        detail,
        // `limit` is at most `MAX_LIMIT`, so the conversion cannot fail.
        limit: usize::try_from(limit).unwrap_or(usize::MAX),
    })
}

/// The searchable words of one tool. The server makes them once, at start.
#[derive(Debug)]
pub struct ToolWords {
    name: Vec<String>,
    tags: Vec<String>,
    arguments: Vec<String>,
    /// The words of the title and the description.
    text: Vec<String>,
}

/// The searchable words of each tool, in catalog order.
pub fn index(tools: &[McpTool]) -> Vec<ToolWords> {
    tools
        .iter()
        .map(|tool| {
            let title = tool
                .annotations
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("");
            ToolWords {
                name: words(&tool.name),
                tags: tool.tags.iter().flat_map(|tag| words(tag)).collect(),
                arguments: argument_names(&tool.input_schema)
                    .iter()
                    .flat_map(|name| words(name))
                    .collect(),
                text: words(&format!(
                    "{title} {}",
                    tool.description.as_deref().unwrap_or("")
                )),
            }
        })
        .collect()
}

/// Search `tools`. `index` holds the words of each tool, in the same order.
/// The result holds `total`, the number of hits, and `tools`, the best hits
/// up to the limit.
///
/// A higher score comes first. Equal scores keep the catalog order, so the
/// result is the same on each call.
pub fn search(tools: &[McpTool], index: &[ToolWords], args: &SearchArgs) -> Value {
    let mut hits: Vec<(u32, &McpTool)> = tools
        .iter()
        .zip(index)
        .map(|(tool, words)| (score(words, &args.terms), tool))
        .filter(|(score, _)| args.terms.is_empty() || *score > 0)
        .collect();
    // A stable sort keeps the catalog order for equal scores.
    hits.sort_by_key(|hit| std::cmp::Reverse(hit.0));
    let entries: Vec<Value> = hits
        .iter()
        .take(args.limit)
        .map(|(_, tool)| entry(tool, args.detail))
        .collect();
    json!({ "total": hits.len(), "tools": entries })
}

/// The search result entry of one tool.
fn entry(tool: &McpTool, detail: Detail) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("name".into(), json!(tool.name));
    if detail == Detail::Name {
        return Value::Object(obj);
    }
    if let Some(description) = &tool.description {
        obj.insert("description".into(), json!(description));
    }
    if !tool.tags.is_empty() {
        obj.insert("tags".into(), json!(tool.tags));
    }
    obj.insert("annotations".into(), tool.annotations.clone());
    if detail == Detail::Full {
        obj.insert("inputSchema".into(), tool.input_schema.clone());
    }
    Value::Object(obj)
}

/// Points for a match in each part of a tool. An exact name word counts most.
const NAME_EXACT: u32 = 4;
const NAME_PART: u32 = 2;
const TAG: u32 = 3;
const ARGUMENT: u32 = 2;
const TEXT: u32 = 1;

/// The score of one tool for the query `terms`. Each term adds the points of
/// each part that it matches, once for each part.
fn score(words: &ToolWords, terms: &[String]) -> u32 {
    let any = |part: &[String], term: &str, points: u32| {
        if part.iter().any(|word| matches(word, term)) {
            points
        } else {
            0
        }
    };
    terms
        .iter()
        .map(|term| {
            let name_points = if words.name.iter().any(|word| word == term) {
                NAME_EXACT
            } else {
                any(&words.name, term, NAME_PART)
            };
            name_points
                + any(&words.tags, term, TAG)
                + any(&words.arguments, term, ARGUMENT)
                + any(&words.text, term, TEXT)
        })
        .sum()
}

/// Whether a catalog `word` matches a query `term`. A term of three or more
/// characters also matches the start of a word, so `report` finds `reports`.
fn matches(word: &str, term: &str) -> bool {
    word == term || (term.chars().count() >= 3 && word.starts_with(term))
}

/// The lowercase words of `text`. A character that is not a letter or a
/// digit splits two words. A capital letter after a lowercase letter or a
/// digit also starts a word, so `listInvoices` gives `list` and `invoices`.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut previous: Option<char> = None;
    for c in text.chars() {
        if !c.is_alphanumeric() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            previous = None;
            continue;
        }
        let camel_break =
            c.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_numeric());
        if camel_break && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        current.extend(c.to_lowercase());
        previous = Some(c);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The argument names in an input schema: the top-level properties, and the
/// properties of each `$defs` entry. This function skips the wrapper keys
/// `query` and `body`. Each tool with a query or a body has them.
fn argument_names(schema: &Value) -> Vec<&str> {
    fn keys(node: &Value) -> Vec<&str> {
        node.get("properties")
            .and_then(Value::as_object)
            .map(|props| props.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }
    let mut names: Vec<&str> = keys(schema)
        .into_iter()
        .filter(|key| *key != "query" && *key != "body")
        .collect();
    if let Some(defs) = schema.get("$defs").and_then(Value::as_object) {
        for def in defs.values() {
            names.extend(keys(def));
        }
    }
    names
}

/// Check the arguments of a `call_tool` call. Returns the parameters of the
/// inner `tools/call`.
///
/// The inner call keeps each other key of the outer parameters, such as
/// `_meta`. The `_meta` carries the task capability and the progress token,
/// so a task or a streaming tool works the same through `call_tool`. The
/// inner `arguments` follow the rules of a direct call: absent is `{}`, and
/// a value that is not an object is an error.
pub fn unwrap_call(params: &Value) -> Result<Value, String> {
    let arguments = match params.get("arguments") {
        Some(value) if value.is_object() => value,
        _ => return Err("`call_tool` needs an `arguments` object".to_owned()),
    };
    let Some(name) = arguments.get("name").and_then(Value::as_str) else {
        return Err("`call_tool` needs a string `name`".to_owned());
    };
    if is_meta_tool(name) {
        return Err(format!("unknown tool: {name}"));
    }
    let inner = super::call_arguments(arguments)?;
    let mut out = params.clone();
    out["name"] = json!(name);
    out["arguments"] = inner;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str, tags: &[&str], schema: Value) -> McpTool {
        McpTool {
            name: name.to_owned(),
            description: Some(description.to_owned()),
            input_schema: schema,
            annotations: json!({ "title": description, "readOnlyHint": true }),
            method: "GET".to_owned(),
            path_template: "/".to_owned(),
            path_params: Vec::new(),
            has_body: false,
            has_query: false,
            streams: false,
            empty_body: false,
            task: false,
            agent_authority: None,
            core: false,
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        }
    }

    fn catalog() -> Vec<McpTool> {
        vec![
            tool("list_users", "List users", &["users"], json!({})),
            tool(
                "create_user",
                "Create one account",
                &["users"],
                json!({
                    "properties": { "body": { "$ref": "#/$defs/NewUser" } },
                    "$defs": { "NewUser": { "properties": { "email": {} } } },
                }),
            ),
            tool("health", "Report liveness", &[], json!({})),
        ]
    }

    fn names(found: &Value) -> Vec<&str> {
        found["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect()
    }

    fn run(arguments: &Value) -> Value {
        let tools = catalog();
        search(&tools, &index(&tools), &parse_search(arguments).unwrap())
    }

    #[test]
    fn words_split_on_punctuation_and_lowercase() {
        assert_eq!(
            words("Refund_invoice.v2-Now"),
            ["refund", "invoice", "v2", "now"]
        );
        assert!(words("  ").is_empty());
    }

    #[test]
    fn words_split_camel_case() {
        assert_eq!(words("listInvoices"), ["list", "invoices"]);
        assert_eq!(words("getV2Report"), ["get", "v2", "report"]);
        assert_eq!(words("HTTPServer"), ["httpserver"]);
    }

    #[test]
    fn a_long_term_matches_a_word_prefix_and_a_short_term_does_not() {
        assert!(matches("reports", "report"));
        assert!(matches("id", "id"));
        assert!(!matches("identity", "id"));
        // The rule counts characters, not bytes.
        assert!(!matches("über", "üb"));
        assert!(!matches("日本", "日"));
    }

    #[test]
    fn an_exact_name_word_outranks_a_prefix() {
        // `create_user` has the word `user`. `list_users` has a prefix match
        // only, so it comes second.
        let found = run(&json!({ "query": "user" }));
        assert_eq!(names(&found), ["create_user", "list_users"]);
        let found = run(&json!({ "query": "create" }));
        assert_eq!(names(&found), ["create_user"]);
    }

    #[test]
    fn argument_names_come_from_properties_and_defs() {
        let found = run(&json!({ "query": "email" }));
        assert_eq!(names(&found), ["create_user"]);
        // The `body` wrapper key is not an argument name.
        assert_eq!(run(&json!({ "query": "body" }))["total"], 0);
    }

    #[test]
    fn an_empty_query_keeps_the_catalog_order() {
        let found = run(&json!({ "query": "  " }));
        assert_eq!(found["total"], 3);
        assert_eq!(names(&found), ["list_users", "create_user", "health"]);
    }

    #[test]
    fn the_limit_caps_the_entries_but_not_the_total() {
        let found = run(&json!({ "limit": 1 }));
        assert_eq!(found["total"], 3);
        assert_eq!(names(&found), ["list_users"]);
    }

    #[test]
    fn detail_levels_add_fields() {
        let named = run(&json!({ "query": "health", "detail": "name" }));
        assert_eq!(named["tools"][0], json!({ "name": "health" }));
        let summary = run(&json!({ "query": "health" }));
        assert!(summary["tools"][0].get("tags").is_none(), "no empty tags");
        assert!(summary["tools"][0].get("inputSchema").is_none());
        let full = run(&json!({ "query": "health", "detail": "full" }));
        assert_eq!(full["tools"][0]["inputSchema"], json!({}));
    }

    #[test]
    fn bad_search_arguments_are_refused() {
        for arguments in [
            json!({ "query": [] }),
            json!({ "detail": 1 }),
            json!({ "limit": -1 }),
            json!({ "limit": 1.5 }),
            json!({ "limit": MAX_LIMIT + 1 }),
            json!({ "query": "a".repeat(MAX_QUERY_BYTES + 1) }),
            json!({ "query": (0..=MAX_TERMS).map(|n| format!("w{n}")).collect::<Vec<_>>().join(" ") }),
        ] {
            assert!(parse_search(&arguments).is_err(), "{arguments}");
        }
        assert!(parse_search(&json!({ "query": null, "limit": MAX_LIMIT })).is_ok());
        // A repeated word counts once toward the cap.
        let repeated = vec!["same"; MAX_TERMS * 4].join(" ");
        let args = parse_search(&json!({ "query": repeated })).unwrap();
        assert_eq!(args.terms, ["same"]);
    }

    #[test]
    fn the_outline_counts_tools_and_tags() {
        assert_eq!(
            outline(&catalog()),
            "The server has 3 tools. Tags: users (2)."
        );
        assert_eq!(outline(&catalog()[2..]), "The server has 1 tool.");
    }

    #[test]
    fn instructions_join_the_author_text_and_the_guidance() {
        assert_eq!(instructions(None, McpCatalog::Full), None);
        assert_eq!(instructions(Some(""), McpCatalog::Full), None);
        assert_eq!(instructions(Some("  \n"), McpCatalog::Full), None);
        assert_eq!(
            instructions(Some("Todo tools."), McpCatalog::Full).as_deref(),
            Some("Todo tools.")
        );
        assert_eq!(
            instructions(None, McpCatalog::Progressive).as_deref(),
            Some(GUIDANCE)
        );
        let both = instructions(Some("Todo tools."), McpCatalog::Progressive).unwrap();
        assert_eq!(both, format!("Todo tools.\n\n{GUIDANCE}"));
    }

    #[test]
    fn unwrap_call_keeps_the_other_keys_and_checks_the_arguments() {
        let meta = json!({ "progressToken": 1 });
        let inner = unwrap_call(&json!({
            "name": CALL_TOOL,
            "arguments": { "name": "health" },
            "_meta": meta,
            "requestState": "s1",
        }))
        .unwrap();
        assert_eq!(
            inner,
            json!({
                "name": "health",
                "arguments": {},
                "_meta": meta,
                "requestState": "s1",
            })
        );
        for arguments in [
            json!(null),
            json!({ "name": 1 }),
            json!({ "name": SEARCH_TOOL }),
            json!({ "name": "health", "arguments": [] }),
            json!({ "name": "health", "arguments": null }),
        ] {
            let params = json!({ "name": CALL_TOOL, "arguments": arguments });
            assert!(unwrap_call(&params).is_err(), "{params}");
        }
    }

    #[test]
    fn the_index_holds_the_words_of_each_tool() {
        let tools = catalog();
        let index = index(&tools);
        assert_eq!(index.len(), tools.len());
        assert_eq!(index[1].name, ["create", "user"]);
        assert_eq!(index[1].tags, ["users"]);
        assert_eq!(index[1].arguments, ["email"]);
        assert_eq!(index[2].text, ["report", "liveness", "report", "liveness"]);
    }

    #[test]
    fn meta_tools_are_always_loaded_and_carry_the_outline() {
        let tools = meta_descriptors(&catalog());
        assert_eq!(tools.len(), 2);
        for tool in &tools {
            assert_eq!(tool["_meta"][ALWAYS_LOAD], true, "{tool}");
        }
        let description = tools[0]["description"].as_str().unwrap();
        assert!(description.contains("3 tools"), "{description}");
    }
}
