//! Guard: no plugin crate starts background work from `on_startup`.
//!
//! Every role runs every startup hook and every state initializer. A loop that
//! a plugin spawns there also runs on `web`. A plugin registers the loop with
//! `AppBuilder::background_task`, and the role decides where it runs.
//!
//! The guard reads each plugin crate: a workspace member, not under
//! `examples/` and not `autumn`, whose sources implement `Plugin`. For each
//! `on_startup(...)` and `state_initializer(...)` argument, it looks for a
//! spawn. A spawn is a call to `spawn`, `spawn_local` or `spawn_<name>`, but
//! not `spawn_blocking`. The guard also reads each function of the same crate
//! that the argument calls, to a depth of [`MAX_DEPTH`]. A `Type::f(` call
//! reads only `f` in an `impl Type` block. A bare argument that names a `let`
//! closure reads that closure. Comments, literals, `#[cfg(test)]` items and
//! `tests` files do not count. [`ALLOWED`] names the spawns that are request
//! scoped.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::Path;

use regex::Regex;

/// How many calls deep the guard follows from a hook argument.
const MAX_DEPTH: usize = 8;

/// Spawns that are not background work: `(crate, holder, reason)`.
///
/// The holder is the function that holds the spawn, or the hook name
/// (`on_startup`, `state_initializer`) for a spawn in the argument itself.
/// Add an entry only for work that ends with one request or one call.
const ALLOWED: &[(&str, &str, &str)] = &[];

/// The hooks that every role runs.
const HOOKS: &str = r"\.(on_startup|state_initializer)\s*\(";

/// One spawn that a hook argument reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    /// The crate directory.
    krate: String,
    /// The file of the spawn.
    path: String,
    /// The line of the spawn.
    line: usize,
    /// `file:line` of the hook call.
    hook_at: String,
    /// The hook: `on_startup` or `state_initializer`.
    hook: String,
    /// The spawn call, for example `tokio::spawn`.
    spawn: String,
    /// The functions from the hook argument to the spawn.
    via: Vec<String>,
}

impl Finding {
    /// The key of this spawn in [`ALLOWED`].
    fn holder(&self) -> &str {
        self.via.last().map_or(self.hook.as_str(), String::as_str)
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}: `{}` at {} reaches `{}(`",
            self.path, self.line, self.hook, self.hook_at, self.spawn
        )?;
        if !self.via.is_empty() {
            write!(f, " through {}", self.via.join(" -> "))?;
        }
        write!(
            f,
            ". Register the loop with `AppBuilder::background_task`. For a request-scoped \
             spawn, add (\"{}\", \"{}\", \"<reason>\") to ALLOWED in xtask/src/plugin_spawns.rs",
            self.krate,
            self.holder()
        )
    }
}

/// One source file: its path and its text with comments, literals and test
/// items replaced by spaces. Newlines stay, so offsets give line numbers.
struct Source {
    path: String,
    code: String,
}

impl Source {
    fn new(path: impl Into<String>, text: &str) -> Self {
        Self {
            path: path.into(),
            code: blank_test_items(&mask(text)),
        }
    }

    fn line(&self, offset: usize) -> usize {
        self.code[..offset].matches('\n').count() + 1
    }
}

/// Replace `out[from..to]` with spaces, except newlines.
fn blank(out: &mut [u8], from: usize, to: usize) {
    for byte in &mut out[from..to] {
        if *byte != b'\n' {
            *byte = b' ';
        }
    }
}

/// `text` with every comment, string literal and char literal replaced by
/// spaces. A lifetime stays.
fn mask(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let end = text[i..].find('\n').map_or(bytes.len(), |n| i + n);
                blank(&mut out, i, end);
                i = end;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 0_usize;
                let mut j = i;
                while j < bytes.len() {
                    if bytes[j] == b'/' && bytes.get(j + 1) == Some(&b'*') {
                        depth += 1;
                        j += 2;
                    } else if bytes[j] == b'*' && bytes.get(j + 1) == Some(&b'/') {
                        depth -= 1;
                        j += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        j += 1;
                    }
                }
                let end = j.min(bytes.len());
                blank(&mut out, i, end);
                i = end;
            }
            b'r' if is_raw_string_start(bytes, i) => {
                let hashes = bytes[i + 1..].iter().take_while(|&&b| b == b'#').count();
                let close: String = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let body = i + 2 + hashes;
                let end = text[body..]
                    .find(&close)
                    .map_or(bytes.len(), |n| body + n + close.len());
                blank(&mut out, i, end);
                i = end;
            }
            b'"' => {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += if bytes[j] == b'\\' { 2 } else { 1 };
                }
                let end = (j + 1).min(bytes.len());
                blank(&mut out, i, end);
                i = end;
            }
            b'\'' => {
                // A char literal: `'x'`, `'\n'`, `'\''`. Else a lifetime. An
                // escape has at least one character after the `\`, so the
                // closing quote is at `i + 3` or later.
                let end = if bytes.get(i + 1) == Some(&b'\\') {
                    text.get(i + 3..)
                        .and_then(|rest| rest.find('\''))
                        .map(|n| i + 3 + n + 1)
                } else {
                    text[i + 1..]
                        .chars()
                        .next()
                        .map(|c| i + 1 + c.len_utf8())
                        .filter(|&close| bytes.get(close) == Some(&b'\''))
                        .map(|close| close + 1)
                };
                match end {
                    Some(end) => {
                        blank(&mut out, i, end);
                        i = end;
                    }
                    None => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8(out).expect("masking replaces whole characters with ASCII spaces")
}

/// `r"`, `r#"`, `br"`: the start of a raw string, not an identifier.
fn is_raw_string_start(bytes: &[u8], i: usize) -> bool {
    let after_ident = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
    let prefixed =
        i > 0 && bytes[i - 1] == b'b' && (i == 1 || !bytes[i - 2].is_ascii_alphanumeric());
    let hashes = bytes[i + 1..].iter().take_while(|&&b| b == b'#').count();
    (after_ident || prefixed) && bytes.get(i + 1 + hashes) == Some(&b'"')
}

/// `code` with each `#[cfg(test)]` or `#[cfg(all(test, ...))]` module or
/// function replaced by spaces.
fn blank_test_items(code: &str) -> String {
    let pattern = Regex::new(
        r"#\[cfg\((?:all\(\s*)?test\b[^\]]*\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?(?:(?:async\s+)?fn|mod)\s+\w+[^{;]*[{;]",
    )
    .expect("test item pattern");
    let mut out = code.as_bytes().to_vec();
    for found in pattern.find_iter(code) {
        let last = found.end() - 1;
        let end = if code.as_bytes()[last] == b'{' {
            matching(code, last).unwrap_or(code.len())
        } else {
            found.end()
        };
        blank(&mut out, found.start(), end);
    }
    String::from_utf8(out).expect("blanking writes ASCII spaces")
}

/// The offset after the bracket that closes the one at `open`.
fn matching(code: &str, open: usize) -> Option<usize> {
    let (opener, closer) = match code.as_bytes().get(open)? {
        b'(' => (b'(', b')'),
        b'{' => (b'{', b'}'),
        _ => return None,
    };
    let mut depth = 0_usize;
    for (offset, &byte) in code.as_bytes()[open..].iter().enumerate() {
        if byte == opener {
            depth += 1;
        } else if byte == closer {
            depth -= 1;
            if depth == 0 {
                return Some(open + offset + 1);
            }
        }
    }
    None
}

/// The text from `start` to the first `;` outside brackets.
fn statement_end(code: &str, start: usize) -> usize {
    let mut depth = 0_i32;
    for (offset, byte) in code.as_bytes()[start..].iter().enumerate() {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b';' if depth <= 0 => return start + offset,
            _ => {}
        }
        if depth < 0 {
            return start + offset;
        }
    }
    code.len()
}

/// A call: an optional `Type::` qualifier and the function name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Call {
    ty: Option<String>,
    name: String,
}

/// The patterns of the guard.
struct Patterns {
    hook: Regex,
    spawn: Regex,
    call: Regex,
    path_arg: Regex,
    fn_def: Regex,
    impl_block: Regex,
}

impl Patterns {
    fn new() -> Self {
        let compile = |pattern: &str| Regex::new(pattern).expect("guard pattern");
        Self {
            hook: compile(HOOKS),
            spawn: compile(r"((?:[A-Za-z_][A-Za-z0-9_]*::)*)\b(spawn[a-z0-9_]*)\s*\("),
            call: compile(
                r"(?:\b([A-Za-z_][A-Za-z0-9_]*)\s*::\s*)?\b([a-z_][a-z0-9_]*)\s*(?:::\s*<[^(){};]*>\s*)?\(",
            ),
            path_arg: compile(r"^\s*(?:[A-Za-z_][A-Za-z0-9_]*::)*([a-z_][a-z0-9_]*)\s*,?\s*$"),
            fn_def: compile(r"\bfn\s+([a-z_][a-z0-9_]*)\s*[<(]"),
            impl_block: compile(r"\bimpl\b([^{;]*)\{"),
        }
    }

    /// Each spawn in `code`: offset and call text. `spawn_blocking` is
    /// awaited work, not a loop.
    fn spawns(&self, code: &str) -> Vec<(usize, String)> {
        self.spawn
            .captures_iter(code)
            .filter_map(|caps| {
                let whole = caps.get(0)?;
                let name = caps.get(2)?;
                (name.as_str() != "spawn_blocking")
                    .then(|| (whole.start(), format!("{}{}", &caps[1], name.as_str())))
            })
            .collect()
    }

    /// The calls in `code`. A bare path argument counts as a call.
    fn calls(&self, code: &str) -> BTreeSet<Call> {
        const KEYWORDS: &[&str] = &[
            "if", "while", "match", "for", "loop", "return", "move", "async", "in", "fn", "let",
            "await",
        ];
        let mut calls: BTreeSet<Call> = self
            .call
            .captures_iter(code)
            .filter(|caps| !KEYWORDS.contains(&&caps[2]))
            .map(|caps| Call {
                // A `Type::` qualifier narrows the call. A module path or
                // `Self` does not.
                ty: caps
                    .get(1)
                    .map(|m| m.as_str())
                    .filter(|q| q.starts_with(|c: char| c.is_ascii_uppercase()) && *q != "Self")
                    .map(str::to_owned),
                name: caps[2].to_owned(),
            })
            .collect();
        if let Some(caps) = self.path_arg.captures(code) {
            calls.insert(Call {
                ty: None,
                name: caps[1].to_owned(),
            });
        }
        calls
    }
}

/// A function body: its source, its span and its `impl` type.
struct Body {
    source: usize,
    start: usize,
    end: usize,
    ty: Option<String>,
}

/// The type of an `impl` header: `<T> Trait for Type<T>` gives `Type`.
fn impl_type(header: &str) -> Option<String> {
    let mut rest = header.trim_start();
    if rest.starts_with('<') {
        let mut depth = 0_i32;
        let mut cut = rest.len();
        for (offset, c) in rest.char_indices() {
            match c {
                '<' => depth += 1,
                '>' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                cut = offset + 1;
                break;
            }
        }
        rest = &rest[cut..];
    }
    if let Some(at) = rest.rfind(" for ") {
        rest = &rest[at + 5..];
    }
    let path: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
        .collect();
    path.rsplit("::")
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// Every function body in `sources`, by name.
fn function_bodies(patterns: &Patterns, sources: &[Source]) -> BTreeMap<String, Vec<Body>> {
    let mut bodies: BTreeMap<String, Vec<Body>> = BTreeMap::new();
    for (index, source) in sources.iter().enumerate() {
        let impls: Vec<(usize, usize, Option<String>)> = patterns
            .impl_block
            .captures_iter(&source.code)
            .filter_map(|caps| {
                let whole = caps.get(0)?;
                let end = matching(&source.code, whole.end() - 1)?;
                Some((whole.start(), end, impl_type(&caps[1])))
            })
            .collect();
        for caps in patterns.fn_def.captures_iter(&source.code) {
            let Some(whole) = caps.get(0) else { continue };
            // The body starts at the first `{` outside the parameter list. A
            // `;` first is a declaration with no body.
            let mut depth = 0_i32;
            let mut open = None;
            for (offset, byte) in source.code.as_bytes()[whole.end()..].iter().enumerate() {
                match byte {
                    b'(' | b'[' => depth += 1,
                    b')' | b']' => depth -= 1,
                    b'{' if depth <= 0 => {
                        open = Some(whole.end() + offset);
                        break;
                    }
                    b';' if depth <= 0 => break,
                    _ => {}
                }
            }
            let Some(open) = open else { continue };
            let end = matching(&source.code, open)
                .unwrap_or_else(|| panic!("{}: unbalanced braces", source.path));
            // The innermost `impl` block around the function.
            let ty = impls
                .iter()
                .filter(|(from, to, _)| *from < whole.start() && whole.start() < *to)
                .max_by_key(|(from, _, _)| *from)
                .and_then(|(_, _, ty)| ty.clone());
            bodies.entry(caps[1].to_owned()).or_default().push(Body {
                source: index,
                start: open,
                end,
                ty,
            });
        }
    }
    bodies
}

/// What a scan of one crate gives.
struct Scan {
    findings: Vec<Finding>,
    /// The number of hook calls.
    hooks: usize,
}

/// Every spawn that a hook argument in `files` reaches, except the ones in
/// `allowed`.
fn scan_crate(krate: &str, files: &[(String, String)], allowed: &[(&str, &str, &str)]) -> Scan {
    let patterns = Patterns::new();
    let sources: Vec<Source> = files
        .iter()
        .filter(|(path, _)| !is_test_file(path))
        .map(|(path, text)| Source::new(path.clone(), text))
        .collect();
    let bodies = function_bodies(&patterns, &sources);
    let mut findings = Vec::new();
    let mut hooks = 0;
    for source in &sources {
        for caps in patterns.hook.captures_iter(&source.code) {
            let (Some(whole), Some(hook)) = (caps.get(0), caps.get(1)) else {
                continue;
            };
            hooks += 1;
            let hook = hook.as_str().to_owned();
            let hook_at = format!("{}:{}", source.path, source.line(whole.start()));
            let open = whole.end() - 1;
            let close = matching(&source.code, open)
                .unwrap_or_else(|| panic!("{hook_at}: unbalanced parentheses"));
            let argument = &source.code[open + 1..close - 1];
            let mut push = |path: &str, line: usize, spawn: String, via: Vec<String>| {
                let finding = Finding {
                    krate: krate.to_owned(),
                    path: path.to_owned(),
                    line,
                    hook_at: hook_at.clone(),
                    hook: hook.clone(),
                    spawn,
                    via,
                };
                if !allowed
                    .iter()
                    .any(|(k, holder, _)| *k == krate && *holder == finding.holder())
                {
                    findings.push(finding);
                }
            };
            // The argument, and a `let` closure that a bare argument names.
            let mut roots = vec![(open + 1, argument)];
            if let Some(name) = patterns
                .path_arg
                .captures(argument)
                .map(|c| c[1].to_owned())
                && let Some(binding) = let_binding(&source.code[..whole.start()], &name)
            {
                roots.push(binding);
            }
            let mut queue: VecDeque<(Call, Vec<String>)> = VecDeque::new();
            for (start, code) in roots {
                for (offset, spawn) in patterns.spawns(code) {
                    push(&source.path, source.line(start + offset), spawn, Vec::new());
                }
                queue.extend(
                    patterns
                        .calls(code)
                        .into_iter()
                        .map(|call| (call.clone(), vec![call.name])),
                );
            }
            // Follow the calls, breadth first, to MAX_DEPTH.
            let mut seen = BTreeSet::new();
            while let Some((call, via)) = queue.pop_front() {
                if !seen.insert(call.clone()) {
                    continue;
                }
                let Some(defs) = bodies.get(&call.name) else {
                    continue;
                };
                for body in defs
                    .iter()
                    .filter(|body| call.ty.is_none() || call.ty == body.ty)
                {
                    let owner = &sources[body.source];
                    let code = &owner.code[body.start..body.end];
                    for (offset, spawn) in patterns.spawns(code) {
                        push(
                            &owner.path,
                            owner.line(body.start + offset),
                            spawn,
                            via.clone(),
                        );
                    }
                    if via.len() < MAX_DEPTH {
                        for callee in patterns.calls(code) {
                            let mut next = via.clone();
                            next.push(callee.name.clone());
                            queue.push_back((callee, next));
                        }
                    }
                }
            }
        }
    }
    // One report per spawn, with the shortest path.
    findings.sort_by(|a, b| (&a.path, a.line, a.via.len()).cmp(&(&b.path, b.line, b.via.len())));
    findings.dedup_by(|later, first| later.path == first.path && later.line == first.line);
    Scan { findings, hooks }
}

/// The last `let name = <expr>;` in `code`: the offset and text of `<expr>`.
fn let_binding<'a>(code: &'a str, name: &str) -> Option<(usize, &'a str)> {
    let pattern = Regex::new(&format!(
        r"\blet\s+(?:mut\s+)?{}\b[^=;]*=",
        regex::escape(name)
    ))
    .expect("let pattern");
    let found = pattern.find_iter(code).last()?;
    let end = statement_end(code, found.end());
    Some((found.end(), &code[found.end()..end]))
}

/// A file of tests: a `tests.rs`, or a file under a `tests` directory.
fn is_test_file(path: &str) -> bool {
    path.split('/')
        .any(|part| part == "tests" || part == "tests.rs")
}

/// Every `.rs` file under `dir`, with its path relative to `root`.
fn rust_files(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
    let mut paths: Vec<_> = entries
        .map(|entry| {
            entry
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
                .path()
        })
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            rust_files(root, &path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let relative = path.strip_prefix(root).unwrap_or(&path);
            out.push((relative.display().to_string().replace('\\', "/"), text));
        }
    }
}

/// The plugin crates of the workspace at `root`: each member, not under
/// `examples/` and not `autumn`, whose `src/` implements `Plugin`. With their
/// sources.
fn plugin_crates(root: &Path) -> Vec<(String, Vec<(String, String)>)> {
    let implements = Regex::new(r"\bimpl\b[^{;]*\bPlugin\s+for\b").expect("impl pattern");
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("root Cargo.toml");
    let manifest: toml::Table = manifest.parse().expect("root Cargo.toml parses");
    manifest["workspace"]["members"]
        .as_array()
        .expect("workspace members")
        .iter()
        .filter_map(|member| member.as_str().map(str::to_owned))
        .filter(|member| member != "autumn" && !member.starts_with("examples/"))
        .filter(|member| root.join(member).join("src").is_dir())
        .filter_map(|member| {
            let mut files = Vec::new();
            rust_files(root, &root.join(&member).join("src"), &mut files);
            let is_plugin = files
                .iter()
                .any(|(_, text)| implements.is_match(&mask(text)));
            is_plugin.then_some((member, files))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{ALLOWED, Finding, mask, plugin_crates, scan_crate};

    fn scan(src: &str) -> Vec<Finding> {
        scan_crate(
            "demo",
            &[("demo/src/lib.rs".to_owned(), src.to_owned())],
            &[],
        )
        .findings
    }

    #[test]
    fn plugin_crates_do_not_spawn_from_startup_hooks() {
        let crates = plugin_crates(crate::repo_root());
        let names: Vec<&str> = crates.iter().map(|(name, _)| name.as_str()).collect();
        for expected in [
            "autumn-admin-plugin",
            "autumn-billing",
            "autumn-cache-redis",
            "autumn-media-plugin",
            "autumn-plugin-reference",
            "autumn-search",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} not found in {names:?}"
            );
        }
        let mut findings = Vec::new();
        for (name, files) in &crates {
            let scan = scan_crate(name, files, ALLOWED);
            // A crate that registers a startup hook must show one, or the
            // guard reads nothing.
            if [
                "autumn-billing",
                "autumn-cache-redis",
                "autumn-media-plugin",
                "autumn-plugin-reference",
                "autumn-search",
            ]
            .contains(&name.as_str())
            {
                assert!(scan.hooks > 0, "no startup hook found in {name}");
            }
            findings.extend(scan.findings.iter().map(ToString::to_string));
        }
        assert!(findings.is_empty(), "{}", findings.join("\n"));
    }

    #[test]
    fn every_allowed_entry_names_a_plugin_crate() {
        let crates = plugin_crates(crate::repo_root());
        for (krate, holder, reason) in ALLOWED {
            assert!(!reason.is_empty(), "{krate}::{holder} has no reason");
            assert!(
                crates.iter().any(|(name, _)| name == krate),
                "{krate} is not a plugin crate"
            );
        }
    }

    #[test]
    fn a_spawn_in_the_closure_fails() {
        let findings = scan(
            "fn build(app: A) -> A {\n    app.on_startup(|state| async move {\n        \
             tokio::spawn(async {});\n        Ok(())\n    })\n}\n",
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            (findings[0].path.as_str(), findings[0].line),
            ("demo/src/lib.rs", 3)
        );
        assert_eq!(findings[0].spawn, "tokio::spawn");
        assert_eq!(findings[0].hook_at, "demo/src/lib.rs:2");
        let message = findings[0].to_string();
        assert!(
            message.contains("(\"demo\", \"on_startup\", \"<reason>\")"),
            "{message}"
        );
    }

    #[test]
    fn a_spawn_in_a_state_initializer_fails() {
        let findings = scan(
            "fn build(app: A) -> A {\n    app.state_initializer(|state| {\n        \
             std::thread::spawn(|| ());\n    })\n}\n",
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].hook, "state_initializer");
        assert_eq!(findings[0].spawn, "std::thread::spawn");
    }

    #[test]
    fn a_spawn_in_a_called_helper_fails() {
        let findings = scan(
            "fn build(app: A) -> A {\n    app.on_startup(|state| async move {\n        \
             rearm(state);\n        Ok(())\n    })\n}\nfn rearm(state: S) {\n    \
             helper::run(state);\n}\nfn run(state: S) {\n    let mut set = JoinSet::new();\n    \
             set.spawn(async {});\n}\n",
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].spawn, "spawn");
        assert_eq!(findings[0].via, ["rearm", "run"]);
        assert_eq!(findings[0].line, 12);
    }

    #[test]
    fn a_deep_call_chain_and_a_turbofish_are_followed() {
        let findings = scan(
            "fn build(app: A) -> A {\n    app.on_startup(|s| async move { a::<Vec<u8>>(s); Ok(()) })\n}\n\
             fn a<T>(s: S) { b(s) }\nfn b(s: S) { c(s) }\nfn c(s: S) { d(s) }\n\
             fn d(s: S) { tokio::task::spawn(async {}); }\n",
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].via, ["a", "b", "c", "d"]);
    }

    #[test]
    fn a_function_path_argument_is_followed() {
        let findings = scan(
            "fn build(app: A) -> A {\n    app.on_startup(start_relay)\n}\n\
             async fn start_relay(state: S) -> R {\n    spawn_relay_loop(state);\n    Ok(())\n}\n",
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].spawn, "spawn_relay_loop");
    }

    #[test]
    fn a_closure_in_a_variable_is_followed() {
        let findings = scan(
            "fn build(app: A) -> A {\n    let hook = |s| async move {\n        \
             tokio::spawn(async {});\n        Ok(())\n    };\n    app.on_startup(hook)\n}\n",
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].line, 3);
    }

    #[test]
    fn a_type_call_reads_only_that_impl() {
        let src = "fn build(app: A) -> A {\n    app.on_startup(|s| async move { CALL; Ok(()) })\n}\n\
                   struct Pool;\nimpl Pool {\n    fn new() -> Self { tokio::spawn(async {}); Self }\n}\n";
        assert!(scan(&src.replace("CALL", "Client::new()")).is_empty());
        assert_eq!(scan(&src.replace("CALL", "Pool::new()")).len(), 1);
        assert_eq!(scan(&src.replace("CALL", "pool::new()")).len(), 1);
    }

    #[test]
    fn several_files_are_read() {
        let files = [
            (
                "demo/src/lib.rs".to_owned(),
                "fn build(app: A) -> A { app.on_startup(|s| async move { jobs::arm(s); Ok(()) }) }\n"
                    .to_owned(),
            ),
            (
                "demo/src/jobs.rs".to_owned(),
                "pub fn arm(s: S) { tokio::spawn(async {}); }\n".to_owned(),
            ),
        ];
        let findings = scan_crate("demo", &files, &[]).findings;
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].path, "demo/src/jobs.rs");
    }

    #[test]
    fn a_background_task_passes() {
        let findings = scan(
            "fn build(app: A) -> A {\n    app.on_startup(|s| async move { Ok(()) })\n        \
             .background_task(\"x.loop\", C, |s, t| async move {\n            \
             tokio::spawn(async {});\n            Ok(())\n        })\n}\n",
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn comments_strings_and_blocking_work_pass() {
        let findings = scan(
            "fn build(app: A) -> A {\n    app.on_startup(|s| async move {\n        \
             // Do not tokio::spawn( here.\n        /* tokio::spawn( /* nested */ */\n        \
             let _ = \"tokio::spawn(\";\n        let _ = r#\"spawn(\"#;\n        \
             let _ = b\"spawn(\";\n        let _ = ['x', '\"', '\\'', b'{'];\n        \
             tokio::task::spawn_blocking(|| ()).await;\n        Ok(())\n    })\n}\n",
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn an_escaped_quote_does_not_hide_a_spawn() {
        let findings = scan(
            "fn build(app: A) -> A {\n    let _ = ['\\'', '\"'];\n    \
             app.on_startup(|s| async move { tokio::spawn(async {}); Ok(()) })\n}\n",
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
    }

    #[test]
    fn test_items_and_test_files_pass() {
        let spawning = "app.on_startup(|s| async move { tokio::spawn(async {}); Ok(()) });";
        for src in [
            format!("#[cfg(test)]\nmod tests {{\n    fn t(app: A) {{ {spawning} }}\n}}\n"),
            format!(
                "#[cfg(test)]\n#[allow(dead_code)]\nmod tests {{ fn t(app: A) {{ {spawning} }} }}\n"
            ),
            format!(
                "#[cfg(all(test, feature = \"x\"))]\nmod tests {{ fn t(app: A) {{ {spawning} }} }}\n"
            ),
            format!("#[cfg(test)]\nfn helper(app: A) {{ {spawning} }}\n"),
        ] {
            assert!(scan(&src).is_empty(), "{src}");
        }
        let files = [(
            "demo/src/tests.rs".to_owned(),
            format!("fn t(app: A) {{ {spawning} }}\n"),
        )];
        assert!(scan_crate("demo", &files, &[]).findings.is_empty());
    }

    #[test]
    fn an_allowed_spawn_passes() {
        let src = "fn build(app: A) -> A {\n    app.on_startup(|s| async move { proxy(s); Ok(()) })\n}\n\
                   fn proxy(s: S) {\n    tokio::spawn(async {});\n}\n";
        let files = [("demo/src/lib.rs".to_owned(), src.to_owned())];
        assert_eq!(scan_crate("demo", &files, &[]).findings.len(), 1);
        let allowed = [("demo", "proxy", "ends with the request")];
        assert!(scan_crate("demo", &files, &allowed).findings.is_empty());
        let other = [("other", "proxy", "another crate")];
        assert_eq!(scan_crate("demo", &files, &other).findings.len(), 1);
    }

    #[test]
    fn mask_keeps_lifetimes_and_line_breaks() {
        let masked = mask("fn f<'a>(x: &'a str) {\n    let c = '\\n'; // spawn(\n}\n");
        assert!(masked.contains("<'a>"), "{masked}");
        assert!(!masked.contains("spawn"), "{masked}");
        assert_eq!(masked.matches('\n').count(), 3);
    }
}
