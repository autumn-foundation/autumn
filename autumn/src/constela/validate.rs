//! Semantic and safety validation of a parsed [`Program`].
//!
//! Parsing proves a document has the right *shape*. Validation proves it has
//! the right *meaning*: that every name it uses is one it declares, that every
//! tag and attribute is on the allowlist in [`policy`](super::policy), and that
//! every step's operands fit its operation. A [`Document`](super::Document) can
//! only be built by passing through here, so the renderer never has to ask
//! whether a reference resolves — it already does.
//!
//! # All the errors, not the first one
//!
//! The walk collects diagnostics and keeps going. That is the difference
//! between a generator that converges in one repair round and one that
//! converges in six, and it is worth the small amount of extra bookkeeping it
//! costs: a caller handing the whole
//! [`ConstelaError::to_json`](super::ConstelaError::to_json) report back to the
//! model gets every fault at once.
//!
//! The one thing that stops the walk early is a component reference cycle,
//! which is detected before the view walk begins — walking a cyclic component
//! graph would not terminate.

// autumn-panic-gate: request-path module — it parses, validates and renders a
// document written by a language model, so every panic in it is reachable by
// hostile input and would be a 500 rather than a diagnostic. Production code
// path must be panic-free. See CONTRIBUTING.md "Request-path panic gate".
// Justify exceptions with #[allow(clippy::<lint>, reason = "…")] at the
// narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::collections::{BTreeMap, BTreeSet};

use super::ast::{
    ActionStep, ComponentDef, Expr, Node, ParamDef, PayloadSpec, Program, Prop, StateType,
    UpdateOperation,
};
use super::error::{ConstelaError, Diagnostic, codes};
use super::policy;

/// The DSL versions this implementation accepts.
pub const SUPPORTED_VERSIONS: &[&str] = &["1.0"];

/// What is in lexical scope at a point in the view tree.
struct Scope<'a> {
    /// Loop bindings currently in scope.
    vars: Vec<&'a str>,
    /// The enclosing component's declared parameters, if inside one.
    params: Option<&'a BTreeMap<String, ParamDef>>,
    /// Whether a `slot` node is legal here.
    in_component: bool,
    /// Whether a `var` naming nothing in [`Self::vars`] is an error.
    ///
    /// In the **view** it is: the only thing that can bind a `var` there is an
    /// enclosing `each`, so a name that resolves to none of them is a typo,
    /// full stop.
    ///
    /// Inside an **action** it is not, and cannot be. An action's `var`s come
    /// from the payload it was invoked with, and that payload is chosen by the
    /// caller at dispatch time — an event handler's `payload`, the event data
    /// a client runtime supplies, or whatever an app hands
    /// [`Document::dispatch`](super::Document::dispatch) directly. None of
    /// those are visible here, so an unbound name there resolves to `null` at
    /// runtime, the way JavaScript's `undefined` would, rather than failing
    /// validation.
    ///
    /// The one unbound name that *is* still an error in an action is one the
    /// action itself binds with a later step's `result`: that is unambiguously
    /// an ordering mistake, not a payload key, and it is caught against
    /// [`Validator::action_results`].
    allow_unbound_vars: bool,
}

impl Scope<'_> {
    /// The scope at the top level of the program's own view.
    const fn top() -> Self {
        Self {
            vars: Vec::new(),
            params: None,
            in_component: false,
            allow_unbound_vars: false,
        }
    }
}

/// Accumulates diagnostics across the whole walk.
struct Validator<'a> {
    program: &'a Program,
    state_types: BTreeMap<&'a str, StateType>,
    action_names: BTreeSet<&'a str>,
    /// Every name the action currently being walked binds with a step
    /// `result`, anywhere in it. Empty while walking the view.
    action_results: BTreeSet<&'a str>,
    diagnostics: Vec<Diagnostic>,
}

/// Whether `name` is usable as a binding, state field, or action name.
///
/// Matches the JavaScript identifier shape upstream generates code for, so a
/// name accepted here cannot become a syntax error in a client build of the
/// same document.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Validate `program`.
///
/// # Errors
///
/// [`ConstelaError::Invalid`] carrying every violation found, in document
/// order. Never returns an empty diagnostic list.
pub fn validate(program: &Program) -> Result<(), ConstelaError> {
    let mut validator = Validator {
        program,
        state_types: BTreeMap::new(),
        action_names: BTreeSet::new(),
        action_results: BTreeSet::new(),
        diagnostics: Vec::new(),
    };

    validator.check_version();
    validator.collect_state();
    validator.collect_actions();

    // Cycles first: the component walk below would not terminate on a cyclic
    // graph, so this is the one check that gates the rest.
    if let Some(cycle) = validator.find_component_cycle() {
        validator.diagnostics.push(Diagnostic::new(
            "components",
            codes::CYCLE,
            format!("components reference each other in a cycle: {cycle}"),
        ));
        return Err(ConstelaError::Invalid(validator.diagnostics));
    }

    validator.check_styles();
    validator.check_route();
    validator.check_lifecycle();
    validator.check_actions();
    validator.check_components();

    let mut scope = Scope::top();
    validator.check_node(&program.view, "view", &mut scope);

    validator.check_island_ids();

    if validator.diagnostics.is_empty() {
        Ok(())
    } else {
        Err(ConstelaError::Invalid(validator.diagnostics))
    }
}

impl<'a> Validator<'a> {
    fn error(&mut self, path: impl Into<String>, code: &'static str, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic::new(path, code, message));
    }

    fn check_version(&mut self) {
        if !SUPPORTED_VERSIONS.contains(&self.program.version.as_str()) {
            let supported = SUPPORTED_VERSIONS.join(", ");
            self.error(
                "version",
                codes::VERSION,
                format!(
                    "version {:?} is not supported; write one of: {supported}",
                    self.program.version
                ),
            );
        }
    }

    fn collect_state(&mut self) {
        let fields: Vec<_> = self.program.state.iter().collect();
        for (name, field) in fields {
            if !is_identifier(name) {
                self.error(
                    format!("state.{name}"),
                    codes::UNKNOWN_REF,
                    "state field names must be identifiers (letters, digits, `_` or `$`, not starting with a digit)",
                );
            }
            if !field.ty.accepts(&field.initial) {
                self.error(
                    format!("state.{name}.initial"),
                    codes::STATE_TYPE,
                    format!(
                        "declared type is {}, but `initial` is {}",
                        field.ty.as_str(),
                        json_type_name(&field.initial)
                    ),
                );
            }
            self.state_types.insert(name.as_str(), field.ty);
        }
    }

    fn collect_actions(&mut self) {
        for (i, action) in self.program.actions.iter().enumerate() {
            if !is_identifier(&action.name) {
                self.error(
                    format!("actions[{i}].name"),
                    codes::UNKNOWN_REF,
                    "action names must be identifiers",
                );
            }
            if !self.action_names.insert(action.name.as_str()) {
                self.error(
                    format!("actions[{i}].name"),
                    codes::DUPLICATE,
                    format!("action {:?} is declared more than once", action.name),
                );
            }
        }
    }

    fn check_styles(&mut self) {
        let presets: Vec<_> = self
            .program
            .styles
            .iter()
            .map(|(name, preset)| (name.clone(), preset.clone()))
            .collect();
        for (name, preset) in presets {
            for (axis, choice) in &preset.default_variants {
                match preset.variants.get(axis) {
                    None => self.error(
                        format!("styles.{name}.defaultVariants.{axis}"),
                        codes::UNKNOWN_REF,
                        format!("style {name:?} has no variant axis {axis:?}"),
                    ),
                    Some(options) if !options.contains_key(choice) => self.error(
                        format!("styles.{name}.defaultVariants.{axis}"),
                        codes::UNKNOWN_REF,
                        format!("style {name:?} axis {axis:?} has no option {choice:?}"),
                    ),
                    Some(_) => {}
                }
            }
        }
    }

    fn check_route(&mut self) {
        let Some(route) = &self.program.route else {
            return;
        };
        // Route metadata is evaluated at the top level: no loop is around it
        // and no component encloses it.
        let scope = Scope::top();
        if let Some(title) = &route.title {
            self.check_expr(title, "route.title", &scope);
        }
        for (key, value) in &route.meta {
            self.check_expr(value, &format!("route.meta.{key}"), &scope);
        }
    }

    fn check_lifecycle(&mut self) {
        let Some(lifecycle) = &self.program.lifecycle else {
            return;
        };
        for (field, hook) in [
            ("onMount", &lifecycle.on_mount),
            ("onUnmount", &lifecycle.on_unmount),
            ("onRouteEnter", &lifecycle.on_route_enter),
            ("onRouteLeave", &lifecycle.on_route_leave),
        ] {
            if let Some(action) = hook {
                self.check_action_ref(action, &format!("lifecycle.{field}"));
            }
        }
    }

    fn check_actions(&mut self) {
        for (i, action) in self.program.actions.iter().enumerate() {
            self.action_results.clear();
            collect_result_names(&action.steps, &mut self.action_results);
            let mut bound: Vec<&'a str> = Vec::new();
            self.check_steps(&action.steps, &format!("actions[{i}].steps"), &mut bound);
        }
        self.action_results.clear();
    }

    /// Walk a step list, threading the `result` names each step binds for the
    /// steps that follow it.
    fn check_steps(&mut self, steps: &'a [ActionStep], path: &str, bound: &mut Vec<&'a str>) {
        for (i, step) in steps.iter().enumerate() {
            self.check_step(step, &format!("{path}[{i}]"), bound);
        }
    }

    fn check_step(&mut self, step: &'a ActionStep, path: &str, bound: &mut Vec<&'a str>) {
        let scope = Scope {
            vars: bound.clone(),
            params: None,
            in_component: false,
            allow_unbound_vars: true,
        };

        match step {
            ActionStep::Set { .. } | ActionStep::Update { .. } | ActionStep::SetPath { .. } => {
                self.check_state_step(step, path, &scope);
            }
            ActionStep::If {
                condition,
                then,
                otherwise,
            } => {
                self.check_expr(condition, &format!("{path}.condition"), &scope);
                // Branches cannot leak bindings to each other or to what
                // follows, so each walks a copy.
                let mut then_bound = bound.clone();
                self.check_steps(then, &format!("{path}.then"), &mut then_bound);
                let mut else_bound = bound.clone();
                self.check_steps(otherwise, &format!("{path}.else"), &mut else_bound);
            }
            ActionStep::Fetch {
                url,
                body,
                result,
                on_success,
                on_error,
                ..
            } => {
                self.check_expr(url, &format!("{path}.url"), &scope);
                self.check_literal_url(url, &format!("{path}.url"));
                if let Some(body) = body {
                    self.check_expr(body, &format!("{path}.body"), &scope);
                }
                self.check_result_and_branches(result.as_ref(), on_success, on_error, path, bound);
            }
            ActionStep::Storage {
                operation,
                key,
                value,
                result,
                on_success,
                on_error,
                ..
            } => {
                self.check_expr(key, &format!("{path}.key"), &scope);
                if matches!(operation, super::ast::StorageOperation::Set) && value.is_none() {
                    self.error(
                        path,
                        codes::STEP_SHAPE,
                        "a `storage` step with operation `set` needs a `value`",
                    );
                }
                if let Some(value) = value {
                    self.check_expr(value, &format!("{path}.value"), &scope);
                }
                self.check_result_and_branches(result.as_ref(), on_success, on_error, path, bound);
            }
            ActionStep::Navigate { url, .. } => {
                self.check_expr(url, &format!("{path}.url"), &scope);
                self.check_literal_url(url, &format!("{path}.url"));
            }
            ActionStep::Delay { ms, then } => {
                self.check_expr(ms, &format!("{path}.ms"), &scope);
                let mut inner = bound.clone();
                self.check_steps(then, &format!("{path}.then"), &mut inner);
            }
            ActionStep::Interval { ms, action } => {
                self.check_expr(ms, &format!("{path}.ms"), &scope);
                self.check_action_ref(action, &format!("{path}.action"));
            }
            ActionStep::Focus { target, operation } => {
                self.check_expr(target, &format!("{path}.target"), &scope);
                self.check_focus_operation(operation, path);
            }
        }
    }

    /// Check that `action` names a declared action.
    fn check_action_ref(&mut self, action: &str, path: &str) {
        if !self.action_names.contains(action) {
            self.error(
                path,
                codes::UNKNOWN_REF,
                format!("no action named {action:?} is declared"),
            );
        }
    }

    /// Check a `focus` step's operation against the three DOM methods it maps
    /// to.
    fn check_focus_operation(&mut self, operation: &str, path: &str) {
        if !["focus", "blur", "select"].contains(&operation) {
            self.error(
                format!("{path}.operation"),
                codes::STEP_SHAPE,
                format!("unknown focus operation {operation:?}; write `focus`, `blur` or `select`"),
            );
        }
    }

    /// Check the three steps that write state directly: their target must be a
    /// declared field, and their operands must fit the operation.
    fn check_state_step(&mut self, step: &'a ActionStep, path: &str, scope: &Scope<'a>) {
        match step {
            ActionStep::Set { target, value } => {
                self.check_state_target(target, path);
                self.check_expr(value, &format!("{path}.value"), scope);
            }
            ActionStep::Update {
                target,
                operation,
                value,
                index,
                delete_count,
            } => {
                self.check_state_target(target, path);
                self.check_update_shape(
                    *operation,
                    target,
                    value.as_ref(),
                    index.as_ref(),
                    delete_count.as_ref(),
                    path,
                );
                if let Some(value) = value {
                    self.check_expr(value, &format!("{path}.value"), scope);
                }
                if let Some(index) = index {
                    self.check_expr(index, &format!("{path}.index"), scope);
                }
                if let Some(count) = delete_count {
                    self.check_expr(count, &format!("{path}.deleteCount"), scope);
                }
            }
            ActionStep::SetPath {
                target,
                path: target_path,
                value,
            } => {
                self.check_state_target(target, path);
                self.check_expr(target_path, &format!("{path}.path"), scope);
                self.check_expr(value, &format!("{path}.value"), scope);
            }
            other => debug_assert!(false, "check_state_step reached a {} step", other.kind()),
        }
    }

    /// Check the `result` binding and the `onSuccess`/`onError` branches the
    /// two request-shaped steps share.
    ///
    /// The binding is in scope for both branches and for every *later* sibling
    /// step, which is why it is pushed onto `bound` last: a step cannot read
    /// the name it is itself about to bind.
    fn check_result_and_branches(
        &mut self,
        result: Option<&'a String>,
        on_success: &'a [ActionStep],
        on_error: &'a [ActionStep],
        path: &str,
        bound: &mut Vec<&'a str>,
    ) {
        let mut inner = bound.clone();
        if let Some(result) = result {
            self.check_binding_name(result, &format!("{path}.result"));
            inner.push(result.as_str());
        }
        self.check_steps(on_success, &format!("{path}.onSuccess"), &mut inner.clone());
        self.check_steps(on_error, &format!("{path}.onError"), &mut inner);
        if let Some(result) = result {
            bound.push(result.as_str());
        }
    }

    /// Check that an `update` step carries the operands its operation needs,
    /// and that its target's declared type can take it.
    fn check_update_shape(
        &mut self,
        operation: UpdateOperation,
        target: &str,
        value: Option<&Expr>,
        index: Option<&Expr>,
        delete_count: Option<&Expr>,
        path: &str,
    ) {
        if let Some(required) = operation.required_target_type()
            && let Some(actual) = self.state_types.get(target).copied()
            && actual != required
        {
            self.error(
                path,
                codes::STEP_SHAPE,
                format!(
                    "operation {:?} needs a {} target, but state field {target:?} is {}",
                    operation.as_str(),
                    required.as_str(),
                    actual.as_str()
                ),
            );
        }

        let needs_value = matches!(
            operation,
            UpdateOperation::Push
                | UpdateOperation::Remove
                | UpdateOperation::Merge
                | UpdateOperation::ReplaceAt
                | UpdateOperation::InsertAt
        );
        if needs_value && value.is_none() {
            self.error(
                path,
                codes::STEP_SHAPE,
                format!("operation {:?} needs a `value`", operation.as_str()),
            );
        }

        let needs_index = matches!(
            operation,
            UpdateOperation::ReplaceAt | UpdateOperation::InsertAt | UpdateOperation::Splice
        );
        if needs_index && index.is_none() {
            self.error(
                path,
                codes::STEP_SHAPE,
                format!("operation {:?} needs an `index`", operation.as_str()),
            );
        }

        if operation == UpdateOperation::Splice && delete_count.is_none() {
            self.error(
                path,
                codes::STEP_SHAPE,
                "operation \"splice\" needs a `deleteCount`",
            );
        }
    }

    fn check_state_target(&mut self, target: &str, path: &str) {
        if !self.state_types.contains_key(target) {
            self.error(
                format!("{path}.target"),
                codes::UNKNOWN_REF,
                format!("no state field named {target:?} is declared"),
            );
        }
    }

    fn check_binding_name(&mut self, name: &str, path: &str) {
        if !is_identifier(name) {
            self.error(
                path,
                codes::UNKNOWN_REF,
                "binding names must be identifiers",
            );
        }
    }

    fn check_components(&mut self) {
        // Collected up front so the walk can take `&mut self` while still
        // holding a `&'a` borrow of each component.
        let components: Vec<(&'a String, &'a ComponentDef)> =
            self.program.components.iter().collect();
        for (name, component) in components {
            let mut scope = Scope {
                vars: Vec::new(),
                params: Some(&component.params),
                in_component: true,
                allow_unbound_vars: false,
            };
            self.check_node(
                &component.view,
                &format!("components.{name}.view"),
                &mut scope,
            );

            // A component's children go into its slot, and the AST gives no
            // way to say *which* children go into *which* named slot. Rather
            // than pick a rule — duplicate the children into every slot, or
            // silently fill only the first — a second slot is rejected, so a
            // document can never be ambiguous about where its children land.
            let slots = count_slots(&component.view);
            if slots > 1 {
                self.error(
                    format!("components.{name}.view"),
                    codes::MISPLACED,
                    format!(
                        "component {name:?} has {slots} `slot` nodes; a component may have at most one, because there is no way to say which children fill which"
                    ),
                );
            }
        }
    }

    /// Find a cycle in the component reference graph, returning it as
    /// `a -> b -> a` for the diagnostic.
    fn find_component_cycle(&self) -> Option<String> {
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        for start in self.program.components.keys() {
            let mut stack: Vec<&str> = Vec::new();
            if let Some(cycle) = self.walk_for_cycle(start, &mut stack, &mut visited) {
                return Some(cycle);
            }
        }
        None
    }

    fn walk_for_cycle<'s>(
        &'s self,
        name: &'s str,
        stack: &mut Vec<&'s str>,
        visited: &mut BTreeSet<&'s str>,
    ) -> Option<String> {
        if let Some(at) = stack.iter().position(|entry| *entry == name) {
            let mut cycle: Vec<&str> = stack.iter().skip(at).copied().collect();
            cycle.push(name);
            return Some(cycle.join(" -> "));
        }
        if !visited.insert(name) {
            return None;
        }
        let component = self.program.components.get(name)?;
        stack.push(name);
        let mut referenced = Vec::new();
        collect_component_refs(&component.view, &mut referenced);
        for next in referenced {
            if let Some(cycle) = self.walk_for_cycle(next, stack, visited) {
                return Some(cycle);
            }
        }
        stack.pop();
        None
    }

    fn check_island_ids(&mut self) {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut ids = Vec::new();
        collect_island_ids(&self.program.view, "view", &mut ids);
        for component in self.program.components.values() {
            collect_island_ids(&component.view, "components", &mut ids);
        }
        for (id, path) in ids {
            if !seen.insert(id) {
                self.error(
                    format!("{path}.id"),
                    codes::DUPLICATE,
                    format!("island id {id:?} is used more than once"),
                );
            }
        }
    }

    /// Walk a view node.
    fn check_node(&mut self, node: &'a Node, path: &str, scope: &mut Scope<'a>) {
        match node {
            Node::Element {
                tag,
                element_ref,
                props,
                children,
            } => self.check_element(tag, element_ref.as_deref(), props, children, path, scope),
            Node::Text { value } => self.check_expr(value, &format!("{path}.value"), scope),
            Node::If {
                condition,
                then,
                otherwise,
            } => {
                self.check_expr(condition, &format!("{path}.condition"), scope);
                self.check_node(then, &format!("{path}.then"), scope);
                if let Some(otherwise) = otherwise {
                    self.check_node(otherwise, &format!("{path}.else"), scope);
                }
            }
            Node::Each {
                items,
                binding,
                index,
                key,
                body,
            } => self.check_each(
                items,
                binding,
                index.as_deref(),
                key.as_ref(),
                body,
                path,
                scope,
            ),
            Node::Component {
                name,
                props,
                children,
            } => self.check_component(name, props, children, path, scope),
            Node::Slot { .. } => {
                if !scope.in_component {
                    self.error(
                        path,
                        codes::MISPLACED,
                        "a `slot` node is only legal inside a component definition",
                    );
                }
            }
            Node::Markdown { content } => {
                #[cfg(not(feature = "markdown"))]
                self.error(
                    path,
                    codes::FEATURE_REQUIRED,
                    "a `markdown` node needs autumn-web's `markdown` Cargo feature, which this build does not have",
                );
                self.check_expr(content, &format!("{path}.content"), scope);
            }
            Node::Code { language, content } => {
                self.check_expr(language, &format!("{path}.language"), scope);
                self.check_expr(content, &format!("{path}.content"), scope);
            }
            Node::Portal { children, .. } => {
                for (i, child) in children.iter().enumerate() {
                    self.check_node(child, &format!("{path}.children[{i}]"), scope);
                }
            }
            Node::Island { id, content, .. } => {
                if id.is_empty() {
                    self.error(
                        format!("{path}.id"),
                        codes::UNKNOWN_REF,
                        "an island needs a non-empty `id`",
                    );
                }
                self.check_node(content, &format!("{path}.content"), scope);
            }
            Node::Suspense {
                fallback, content, ..
            }
            | Node::ErrorBoundary { fallback, content } => {
                self.check_node(fallback, &format!("{path}.fallback"), scope);
                self.check_node(content, &format!("{path}.content"), scope);
            }
        }
    }

    /// Check an element: its tag against the allowlist, its props, its
    /// children.
    fn check_element(
        &mut self,
        tag: &str,
        element_ref: Option<&str>,
        props: &'a BTreeMap<String, Prop>,
        children: &'a [Node],
        path: &str,
        scope: &mut Scope<'a>,
    ) {
        if !policy::is_allowed_tag(tag) {
            self.error(
                path,
                codes::TAG_NOT_ALLOWED,
                format!(
                    "tag {tag:?} is not on the allowlist; see `constela::policy::ALLOWED_TAGS`"
                ),
            );
        }
        if policy::is_void_tag(tag) && !children.is_empty() {
            self.error(
                format!("{path}.children"),
                codes::MISPLACED,
                format!("<{tag}> is a void element and cannot have children"),
            );
        }
        if let Some(name) = element_ref {
            self.check_binding_name(name, &format!("{path}.ref"));
        }
        let mut canonical_seen: BTreeMap<String, &str> = BTreeMap::new();
        for (attr, prop) in props {
            // Two spellings of one attribute (`class` and `className`) would
            // emit it twice, and HTML would silently keep whichever came
            // first. Report it rather than pick.
            if let Prop::Value(_) = prop {
                let canonical = policy::canonical_attr(attr);
                if let Some(previous) = canonical_seen.insert(canonical.clone(), attr) {
                    self.error(
                        format!("{path}.props.{attr}"),
                        codes::DUPLICATE,
                        format!(
                            "{attr:?} and {previous:?} are both the {canonical:?} attribute; write only one"
                        ),
                    );
                }
            }
            self.check_prop(attr, prop, &format!("{path}.props.{attr}"), scope);
        }
        for (i, child) in children.iter().enumerate() {
            self.check_node(child, &format!("{path}.children[{i}]"), scope);
        }
    }

    /// Check an `each`: its list expression, its binding names, and its body
    /// with those bindings added to the scope.
    #[expect(
        clippy::too_many_arguments,
        reason = "the loop's own parts plus the walk state threaded through \
                  every node"
    )]
    fn check_each(
        &mut self,
        items: &'a Expr,
        binding: &'a str,
        index: Option<&'a str>,
        key: Option<&'a Expr>,
        body: &'a Node,
        path: &str,
        scope: &mut Scope<'a>,
    ) {
        self.check_expr(items, &format!("{path}.items"), scope);
        if !is_identifier(binding) {
            self.error(
                format!("{path}.as"),
                codes::UNKNOWN_REF,
                "loop binding names must be identifiers",
            );
        }
        if let Some(index) = index {
            self.check_binding_name(index, &format!("{path}.index"));
        }

        // The bindings are in scope for the key and the body, and nowhere
        // else — hence the truncate rather than a fresh scope, which would
        // also drop the *enclosing* loops' bindings.
        let restore = scope.vars.len();
        scope.vars.push(binding);
        if let Some(index) = index {
            scope.vars.push(index);
        }
        if let Some(key) = key {
            self.check_expr(key, &format!("{path}.key"), scope);
        }
        self.check_node(body, &format!("{path}.body"), scope);
        scope.vars.truncate(restore);
    }

    /// Check a component invocation against the component's declared params.
    fn check_component(
        &mut self,
        name: &str,
        props: &'a BTreeMap<String, Expr>,
        children: &'a [Node],
        path: &str,
        scope: &mut Scope<'a>,
    ) {
        match self.program.components.get(name) {
            None => self.error(
                format!("{path}.name"),
                codes::UNKNOWN_REF,
                format!("no component named {name:?} is declared"),
            ),
            Some(component) => {
                for (param, def) in &component.params {
                    if def.required && !props.contains_key(param) {
                        self.error(
                            format!("{path}.props"),
                            codes::UNKNOWN_REF,
                            format!(
                                "component {name:?} requires a {param:?} prop, which is not supplied"
                            ),
                        );
                    }
                }
                for supplied in props.keys() {
                    if !component.params.contains_key(supplied) {
                        self.error(
                            format!("{path}.props.{supplied}"),
                            codes::UNKNOWN_REF,
                            format!("component {name:?} declares no parameter {supplied:?}"),
                        );
                    }
                }
            }
        }
        // Props and slot children are written in the *caller's* scope, so they
        // are checked here rather than inside the component.
        for (key, value) in props {
            self.check_expr(value, &format!("{path}.props.{key}"), scope);
        }
        for (i, child) in children.iter().enumerate() {
            self.check_node(child, &format!("{path}.children[{i}]"), scope);
        }
    }

    /// Check one entry of an element's `props`.
    fn check_prop(&mut self, attr: &str, prop: &'a Prop, path: &str, scope: &Scope<'a>) {
        match prop {
            Prop::Handler(handler) => {
                self.check_action_ref(&handler.action, &format!("{path}.action"));
                if handler.event.is_empty() {
                    self.error(
                        format!("{path}.event"),
                        codes::STEP_SHAPE,
                        "an event binding needs a non-empty `event` name",
                    );
                }
                match &handler.payload {
                    None => {}
                    Some(PayloadSpec::Single(expr)) => {
                        self.check_expr(expr, &format!("{path}.payload"), scope);
                    }
                    Some(PayloadSpec::Map(props)) => {
                        for (key, expr) in props {
                            self.check_expr(expr, &format!("{path}.payload.{key}"), scope);
                        }
                    }
                }
            }
            Prop::Value(expr) => {
                // Judge the canonical name, so `className` is checked as
                // `class` — the same name the renderer will emit.
                let canonical = policy::canonical_attr(attr);
                if policy::is_reserved_attr(&canonical) {
                    self.error(
                        path,
                        codes::ATTR_NOT_ALLOWED,
                        format!(
                            "{}* is reserved for the renderer; express this with an event binding, a `ref`, or an `island` node instead",
                            policy::RESERVED_ATTR_PREFIX
                        ),
                    );
                } else if !policy::is_allowed_attr(&canonical) {
                    self.error(
                        path,
                        codes::ATTR_NOT_ALLOWED,
                        format!(
                            "attribute {canonical:?} is not on the allowlist; see `constela::policy::ALLOWED_ATTRS`"
                        ),
                    );
                }
                if policy::is_url_attr(&canonical) {
                    self.check_literal_url(expr, path);
                }
                self.check_expr(expr, path, scope);
            }
        }
    }

    /// Reject a URL that is a *literal* with a disallowed scheme.
    ///
    /// Only literals can be judged here; a computed URL is re-checked against
    /// the same [`policy::is_allowed_url`] at render time, where its value is
    /// known. Doing it in both places is deliberate: the static check gives the
    /// author a diagnostic to act on, and the render-time check is the one that
    /// actually holds.
    fn check_literal_url(&mut self, expr: &Expr, path: &str) {
        if let Expr::Lit { value } = expr
            && let Some(url) = value.as_str()
            && !policy::is_allowed_url(url)
        {
            self.error(
                path,
                codes::URL_SCHEME,
                format!(
                    "URL {url:?} uses a scheme that is not allowed; write a relative URL or one of: {}",
                    policy::ALLOWED_URL_SCHEMES.join(", ")
                ),
            );
        }
    }

    /// Walk an expression, checking every name it reads.
    fn check_expr(&mut self, expr: &'a Expr, path: &str, scope: &Scope<'a>) {
        match expr {
            Expr::Lit { .. } | Expr::Route { .. } => {}
            Expr::State { name, .. } => {
                if !self.state_types.contains_key(name.as_str()) {
                    self.error(
                        path,
                        codes::UNKNOWN_REF,
                        format!("no state field named {name:?} is declared"),
                    );
                }
            }
            Expr::Var { name, .. } => {
                if !scope.vars.contains(&name.as_str()) {
                    if !scope.allow_unbound_vars {
                        self.error(
                            path,
                            codes::UNKNOWN_REF,
                            format!(
                                "no binding named {name:?} is in scope here; in a view a `var` reads an enclosing `each` binding"
                            ),
                        );
                    } else if self.action_results.contains(name.as_str()) {
                        self.error(
                            path,
                            codes::UNKNOWN_REF,
                            format!(
                                "{name:?} is bound by a `result` on a *later* step; move that step above this one, or read a payload key instead"
                            ),
                        );
                    }
                }
            }
            Expr::Param { name, .. } => match scope.params {
                None => self.error(
                    path,
                    codes::MISPLACED,
                    "a `param` expression is only legal inside a component definition",
                ),
                Some(params) if !params.contains_key(name) => self.error(
                    path,
                    codes::UNKNOWN_REF,
                    format!("the enclosing component declares no parameter {name:?}"),
                ),
                Some(_) => {}
            },
            Expr::Style { name, variants } => {
                if !self.program.styles.contains_key(name) {
                    self.error(
                        path,
                        codes::UNKNOWN_REF,
                        format!("no style preset named {name:?} is declared"),
                    );
                }
                for (axis, value) in variants {
                    self.check_expr(value, &format!("{path}.variants.{axis}"), scope);
                }
            }
            Expr::Not { operand } => self.check_expr(operand, &format!("{path}.operand"), scope),
            Expr::Bin { left, right, .. } => {
                self.check_expr(left, &format!("{path}.left"), scope);
                self.check_expr(right, &format!("{path}.right"), scope);
            }
            Expr::Cond {
                condition,
                then,
                otherwise,
            } => {
                self.check_expr(condition, &format!("{path}.if"), scope);
                self.check_expr(then, &format!("{path}.then"), scope);
                self.check_expr(otherwise, &format!("{path}.else"), scope);
            }
            Expr::Get { base, .. } => self.check_expr(base, &format!("{path}.base"), scope),
            Expr::Index { base, key } => {
                self.check_expr(base, &format!("{path}.base"), scope);
                self.check_expr(key, &format!("{path}.key"), scope);
            }
            Expr::Concat { items } => {
                for (i, item) in items.iter().enumerate() {
                    self.check_expr(item, &format!("{path}.items[{i}]"), scope);
                }
            }
            Expr::Array { elements } => {
                for (i, element) in elements.iter().enumerate() {
                    self.check_expr(element, &format!("{path}.elements[{i}]"), scope);
                }
            }
            Expr::Obj { props } => {
                for (key, value) in props {
                    self.check_expr(value, &format!("{path}.props.{key}"), scope);
                }
            }
        }
    }
}

/// The JSON type name of `value`, for the state-type diagnostic.
const fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "list",
        serde_json::Value::Object(_) => "object",
    }
}

/// Collect every name an action binds with a step `result`, at any depth.
fn collect_result_names<'a>(steps: &'a [ActionStep], out: &mut BTreeSet<&'a str>) {
    for step in steps {
        match step {
            ActionStep::Fetch {
                result,
                on_success,
                on_error,
                ..
            }
            | ActionStep::Storage {
                result,
                on_success,
                on_error,
                ..
            } => {
                if let Some(result) = result {
                    out.insert(result.as_str());
                }
                collect_result_names(on_success, out);
                collect_result_names(on_error, out);
            }
            ActionStep::If {
                then, otherwise, ..
            } => {
                collect_result_names(then, out);
                collect_result_names(otherwise, out);
            }
            ActionStep::Delay { then, .. } => collect_result_names(then, out),
            ActionStep::Set { .. }
            | ActionStep::Update { .. }
            | ActionStep::SetPath { .. }
            | ActionStep::Navigate { .. }
            | ActionStep::Interval { .. }
            | ActionStep::Focus { .. } => {}
        }
    }
}

/// Count the `slot` nodes in a component's own view.
///
/// Stops at a nested [`Node::Component`]'s children: those are slot content
/// for *that* invocation and belong to its component's count, not this one's.
fn count_slots(node: &Node) -> usize {
    match node {
        Node::Slot { .. } => 1,
        Node::Element { children, .. } | Node::Portal { children, .. } => {
            children.iter().map(count_slots).sum()
        }
        Node::If {
            then, otherwise, ..
        } => count_slots(then).saturating_add(otherwise.as_deref().map_or(0, count_slots)),
        Node::Each { body, .. } | Node::Island { content: body, .. } => count_slots(body),
        Node::Suspense {
            fallback, content, ..
        }
        | Node::ErrorBoundary { fallback, content } => {
            count_slots(fallback).saturating_add(count_slots(content))
        }
        // A nested component's children are slot content for *that*
        // invocation, so they count against its component, not this one.
        // The rest hold no nodes at all.
        Node::Component { .. } | Node::Text { .. } | Node::Markdown { .. } | Node::Code { .. } => 0,
    }
}

/// Collect the names of every component a node tree invokes.
fn collect_component_refs<'a>(node: &'a Node, out: &mut Vec<&'a str>) {
    match node {
        Node::Component { name, children, .. } => {
            out.push(name.as_str());
            for child in children {
                collect_component_refs(child, out);
            }
        }
        Node::Element { children, .. } | Node::Portal { children, .. } => {
            for child in children {
                collect_component_refs(child, out);
            }
        }
        Node::If {
            then, otherwise, ..
        } => {
            collect_component_refs(then, out);
            if let Some(otherwise) = otherwise {
                collect_component_refs(otherwise, out);
            }
        }
        Node::Each { body, .. } | Node::Island { content: body, .. } => {
            collect_component_refs(body, out);
        }
        Node::Suspense {
            fallback, content, ..
        }
        | Node::ErrorBoundary { fallback, content } => {
            collect_component_refs(fallback, out);
            collect_component_refs(content, out);
        }
        Node::Text { .. } | Node::Slot { .. } | Node::Markdown { .. } | Node::Code { .. } => {}
    }
}

/// Collect every island id in a node tree, with the path that reached it.
fn collect_island_ids<'a>(node: &'a Node, path: &str, out: &mut Vec<(&'a str, String)>) {
    match node {
        Node::Island { id, content, .. } => {
            out.push((id.as_str(), path.to_string()));
            collect_island_ids(content, &format!("{path}.content"), out);
        }
        Node::Element { children, .. }
        | Node::Portal { children, .. }
        | Node::Component { children, .. } => {
            for (i, child) in children.iter().enumerate() {
                collect_island_ids(child, &format!("{path}.children[{i}]"), out);
            }
        }
        Node::If {
            then, otherwise, ..
        } => {
            collect_island_ids(then, &format!("{path}.then"), out);
            if let Some(otherwise) = otherwise {
                collect_island_ids(otherwise, &format!("{path}.else"), out);
            }
        }
        Node::Each { body, .. } => collect_island_ids(body, &format!("{path}.body"), out),
        Node::Suspense {
            fallback, content, ..
        }
        | Node::ErrorBoundary { fallback, content } => {
            collect_island_ids(fallback, &format!("{path}.fallback"), out);
            collect_island_ids(content, &format!("{path}.content"), out);
        }
        Node::Text { .. } | Node::Slot { .. } | Node::Markdown { .. } | Node::Code { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constela::parse::{Limits, parse_program};

    fn diagnostics(source: &str) -> Vec<Diagnostic> {
        let program = parse_program(source, &Limits::default()).expect("parses");
        match validate(&program) {
            Ok(()) => Vec::new(),
            Err(err) => err.diagnostics().to_vec(),
        }
    }

    fn codes_of(source: &str) -> Vec<&'static str> {
        diagnostics(source).into_iter().map(|d| d.code).collect()
    }

    #[test]
    fn a_minimal_document_validates() {
        assert!(
            diagnostics(r#"{"version":"1.0","view":{"kind":"element","tag":"div"}}"#).is_empty()
        );
    }

    #[test]
    fn rejects_an_unsupported_version() {
        assert!(
            codes_of(r#"{"version":"2.0","view":{"kind":"element","tag":"div"}}"#)
                .contains(&codes::VERSION)
        );
    }

    #[test]
    fn reports_every_fault_not_just_the_first() {
        let found = diagnostics(
            r#"{"version":"1.0","view":{"kind":"element","tag":"script","props":{
                "onclick":{"expr":"lit","value":"x"},
                "href":{"expr":"lit","value":"javascript:alert(1)"}}}}"#,
        );
        let codes: Vec<_> = found.iter().map(|d| d.code).collect();
        assert!(codes.contains(&codes::TAG_NOT_ALLOWED), "{found:?}");
        assert!(codes.contains(&codes::ATTR_NOT_ALLOWED), "{found:?}");
        assert!(codes.contains(&codes::URL_SCHEME), "{found:?}");
    }

    #[test]
    fn diagnostics_point_at_the_submitted_json() {
        let found = diagnostics(
            r#"{"version":"1.0","view":{"kind":"element","tag":"div","children":[
                {"kind":"element","tag":"span"},
                {"kind":"element","tag":"a","props":{"href":{"expr":"lit","value":"javascript:x"}}}]}}"#,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "view.children[1].props.href");
    }

    #[test]
    fn state_initial_must_inhabit_its_declared_type() {
        let found = diagnostics(
            r#"{"version":"1.0","state":{"count":{"type":"number","initial":"zero"}},
                "view":{"kind":"element","tag":"div"}}"#,
        );
        assert_eq!(found[0].code, codes::STATE_TYPE);
        assert_eq!(found[0].path, "state.count.initial");
    }

    #[test]
    fn unknown_state_and_action_references_are_reported() {
        let codes = codes_of(
            r#"{"version":"1.0","view":{"kind":"element","tag":"button","props":{
                "onClick":{"event":"click","action":"nope"}},
                "children":[{"kind":"text","value":{"expr":"state","name":"missing"}}]}}"#,
        );
        assert_eq!(codes, vec![codes::UNKNOWN_REF, codes::UNKNOWN_REF]);
    }

    #[test]
    fn a_var_must_be_bound_by_an_enclosing_each() {
        assert!(
            codes_of(
                r#"{"version":"1.0","view":{"kind":"text","value":{"expr":"var","name":"item"}}}"#
            )
            .contains(&codes::UNKNOWN_REF)
        );

        assert!(
            diagnostics(
                r#"{"version":"1.0","state":{"items":{"type":"list","initial":[]}},
                    "view":{"kind":"each","items":{"expr":"state","name":"items"},"as":"item",
                            "body":{"kind":"text","value":{"expr":"var","name":"item"}}}}"#
            )
            .is_empty()
        );
    }

    #[test]
    fn a_loop_binding_does_not_escape_its_body() {
        let codes = codes_of(
            r#"{"version":"1.0","state":{"items":{"type":"list","initial":[]}},
                "view":{"kind":"element","tag":"div","children":[
                  {"kind":"each","items":{"expr":"state","name":"items"},"as":"item",
                   "body":{"kind":"text","value":{"expr":"var","name":"item"}}},
                  {"kind":"text","value":{"expr":"var","name":"item"}}]}}"#,
        );
        assert_eq!(codes, vec![codes::UNKNOWN_REF]);
    }

    #[test]
    fn param_and_slot_are_component_only() {
        let codes = codes_of(
            r#"{"version":"1.0","view":{"kind":"element","tag":"div","children":[
                {"kind":"text","value":{"expr":"param","name":"label"}},
                {"kind":"slot"}]}}"#,
        );
        assert_eq!(codes, vec![codes::MISPLACED, codes::MISPLACED]);
    }

    #[test]
    fn component_props_are_checked_against_declared_params() {
        let found = diagnostics(
            r#"{"version":"1.0",
                "components":{"Badge":{"params":{"label":{"type":"string"}},
                              "view":{"kind":"text","value":{"expr":"param","name":"label"}}}},
                "view":{"kind":"component","name":"Badge","props":{"text":{"expr":"lit","value":"hi"}}}}"#,
        );
        let messages: Vec<_> = found.iter().map(|d| d.message.as_str()).collect();
        assert!(
            messages.iter().any(|m| m.contains("requires a \"label\"")),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("declares no parameter \"text\"")),
            "{messages:?}"
        );
    }

    #[test]
    fn component_cycles_are_caught_before_the_walk() {
        let found = diagnostics(
            r#"{"version":"1.0",
                "components":{
                  "A":{"view":{"kind":"component","name":"B"}},
                  "B":{"view":{"kind":"component","name":"A"}}},
                "view":{"kind":"component","name":"A"}}"#,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].code, codes::CYCLE);
        assert!(found[0].message.contains("->"), "{:?}", found[0].message);
    }

    #[test]
    fn a_component_may_reference_another_acyclically() {
        assert!(
            diagnostics(
                r#"{"version":"1.0",
                    "components":{
                      "Inner":{"view":{"kind":"text","value":{"expr":"lit","value":"x"}}},
                      "Outer":{"view":{"kind":"component","name":"Inner"}}},
                    "view":{"kind":"component","name":"Outer"}}"#
            )
            .is_empty()
        );
    }

    #[test]
    fn update_operations_must_fit_their_target_type() {
        let found = diagnostics(
            r#"{"version":"1.0","state":{"name":{"type":"string","initial":""}},
                "actions":[{"name":"bump","steps":[{"do":"update","target":"name","operation":"increment"}]}],
                "view":{"kind":"element","tag":"div"}}"#,
        );
        assert_eq!(found[0].code, codes::STEP_SHAPE);
        assert!(found[0].message.contains("number target"), "{found:?}");
    }

    #[test]
    fn update_operations_must_carry_their_operands() {
        let found = diagnostics(
            r#"{"version":"1.0","state":{"items":{"type":"list","initial":[]}},
                "actions":[{"name":"a","steps":[
                  {"do":"update","target":"items","operation":"push"},
                  {"do":"update","target":"items","operation":"splice"}]}],
                "view":{"kind":"element","tag":"div"}}"#,
        );
        let messages: Vec<_> = found.iter().map(|d| d.message.as_str()).collect();
        assert!(messages.iter().any(|m| m.contains("needs a `value`")));
        assert!(messages.iter().any(|m| m.contains("needs an `index`")));
        assert!(messages.iter().any(|m| m.contains("needs a `deleteCount`")));
    }

    #[test]
    fn an_action_may_read_a_payload_key_the_document_cannot_declare() {
        // A payload is chosen by whoever dispatches the action, so an unbound
        // `var` in an action is a possibility, not a typo. In a *view* the
        // same expression is still an error — see the test above.
        assert!(
            diagnostics(
                r#"{"version":"1.0","state":{"q":{"type":"string","initial":""}},
                    "actions":[{"name":"search","steps":[
                      {"do":"set","target":"q","value":{"expr":"var","name":"value"}}]}],
                    "view":{"kind":"element","tag":"input"}}"#
            )
            .is_empty()
        );
    }

    #[test]
    fn a_step_result_is_in_scope_for_later_steps_only() {
        let ok = diagnostics(
            r#"{"version":"1.0","state":{"data":{"type":"object","initial":{}}},
                "actions":[{"name":"load","steps":[
                  {"do":"fetch","url":{"expr":"lit","value":"/api"},"result":"res"},
                  {"do":"set","target":"data","value":{"expr":"var","name":"res"}}]}],
                "view":{"kind":"element","tag":"div"}}"#,
        );
        assert!(ok.is_empty(), "{ok:?}");

        // Reading a name the action binds on a *later* step is an ordering
        // mistake, and is caught even though an unbound name that the action
        // never binds would be allowed as a payload key.
        let bad = diagnostics(
            r#"{"version":"1.0","state":{"data":{"type":"object","initial":{}}},
                "actions":[{"name":"load","steps":[
                  {"do":"set","target":"data","value":{"expr":"var","name":"res"}},
                  {"do":"fetch","url":{"expr":"lit","value":"/api"},"result":"res"}]}],
                "view":{"kind":"element","tag":"div"}}"#,
        );
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert_eq!(bad[0].code, codes::UNKNOWN_REF);
        assert!(bad[0].message.contains("later"), "{:?}", bad[0].message);
    }

    #[test]
    fn duplicate_action_names_and_island_ids_are_reported() {
        let codes = codes_of(
            r#"{"version":"1.0",
                "actions":[{"name":"go","steps":[]},{"name":"go","steps":[]}],
                "view":{"kind":"element","tag":"div","children":[
                  {"kind":"island","id":"a","content":{"kind":"element","tag":"span"}},
                  {"kind":"island","id":"a","content":{"kind":"element","tag":"span"}}]}}"#,
        );
        assert_eq!(codes, vec![codes::DUPLICATE, codes::DUPLICATE]);
    }

    #[test]
    fn the_renderer_attribute_namespace_is_reserved() {
        let found = diagnostics(
            r#"{"version":"1.0","view":{"kind":"element","tag":"div","props":{
                "data-constela-on-click":{"expr":"lit","value":"forged"}}}}"#,
        );
        assert_eq!(found[0].code, codes::ATTR_NOT_ALLOWED);
        assert!(found[0].message.contains("reserved"), "{found:?}");
    }

    #[test]
    fn two_spellings_of_one_attribute_are_reported() {
        let found = diagnostics(
            r#"{"version":"1.0","view":{"kind":"element","tag":"div","props":{
                "class":{"expr":"lit","value":"a"},
                "className":{"expr":"lit","value":"b"}}}}"#,
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].code, codes::DUPLICATE);
    }

    #[test]
    fn void_elements_may_not_have_children() {
        let found = diagnostics(
            r#"{"version":"1.0","view":{"kind":"element","tag":"br","children":[
                {"kind":"text","value":{"expr":"lit","value":"x"}}]}}"#,
        );
        assert_eq!(found[0].code, codes::MISPLACED);
    }

    #[test]
    fn navigate_and_fetch_urls_are_scheme_checked() {
        let codes = codes_of(
            r#"{"version":"1.0",
                "actions":[{"name":"go","steps":[
                  {"do":"navigate","url":{"expr":"lit","value":"javascript:alert(1)"}},
                  {"do":"fetch","url":{"expr":"lit","value":"file:///etc/passwd"}}]}],
                "view":{"kind":"element","tag":"div"}}"#,
        );
        assert_eq!(codes, vec![codes::URL_SCHEME, codes::URL_SCHEME]);
    }

    #[test]
    fn lifecycle_hooks_must_name_declared_actions() {
        let codes = codes_of(
            r#"{"version":"1.0","lifecycle":{"onMount":"nope"},
                "view":{"kind":"element","tag":"div"}}"#,
        );
        assert_eq!(codes, vec![codes::UNKNOWN_REF]);
    }

    #[test]
    fn style_presets_and_their_defaults_are_checked() {
        let codes = codes_of(
            r#"{"version":"1.0",
                "styles":{"btn":{"base":"b","variants":{"size":{"sm":"s"}},"defaultVariants":{"tone":"x"}}},
                "view":{"kind":"element","tag":"div","props":{"class":{"expr":"style","name":"missing"}}}}"#,
        );
        assert_eq!(codes, vec![codes::UNKNOWN_REF, codes::UNKNOWN_REF]);
    }

    #[test]
    fn identifier_rules() {
        assert!(is_identifier("count"));
        assert!(is_identifier("_x9"));
        assert!(is_identifier("$item"));
        assert!(!is_identifier(""));
        assert!(!is_identifier("9lives"));
        assert!(!is_identifier("has-dash"));
        assert!(!is_identifier("has space"));
    }
}
