//! Prefab builder with scope-stack semantics.
//!
//! The builder provides a stateful, incremental workflow for assembling DM prefabs:
//!
//! - `builder_init(type_path)` — push root scope for a new prefab
//! - `list_vars` / `var_info` — discover available properties
//! - `set_var(name, value)` — set a property (validated against objtree)
//! - `edit(var_name)` — push sub-scope for a datum property
//! - `validate()` — check readiness
//! - `commit()` — pop scope, fold into parent
//! - `discard()` — pop scope without saving
//!
//! The tool surface is REPLACED on every scope transition.

use std::collections::BTreeMap;
use std::sync::Arc;
use dreammaker::objtree::{ObjectTree, TypeRef};
use dreammaker::constants::Constant;
use serde_json::{json, Value as JsonValue, Map as JsonMap};

use crate::state::ServerState;

// ── Helper: build a Tool schema from a serde_json::Value ─────────────

/// Convert a serde_json::Value (object) into Arc<JsonObject> for Tool::new
fn schema(v: serde_json::Value) -> Arc<serde_json::Map<String, serde_json::Value>> {
    match v {
        serde_json::Value::Object(map) => Arc::new(map),
        _ => Arc::new(serde_json::Map::new()),
    }
}

// ── Public types ─────────────────────────────────────────────────────

/// Result of a builder tool call.
pub struct BuilderResponse {
    /// Text response to send back to the client.
    pub text: String,
    /// Whether this call changed the scope (triggers list_changed notification).
    pub scope_changed: bool,
}

/// A single builder scope — represents one level of the init/edit/commit stack.
pub struct BuilderScope {
    /// The DM type path being built (e.g. "/obj/machinery/door/airlock/engineering")
    pub type_path: String,
    /// Variable overrides set by the user (name → JSON value)
    pub vars: BTreeMap<String, JsonValue>,
    /// If this is a sub-scope, the parent var name we're editing
    pub parent_var: Option<String>,
    /// Breadcrumb path for display (e.g. "airlock > electronics")
    pub breadcrumb: String,
}

impl BuilderScope {
    /// Generate the MCP tool definitions for this scope.
    pub fn tools(&self) -> Vec<rmcp::model::Tool> {
        use rmcp::model::Tool;

        let mut tools = Vec::new();

        // list_vars — enumerate available properties
        tools.push(Tool::new(
            "list_vars",
            format!(
                "List available properties on {} with their types and defaults. Scope: {}",
                self.type_path, self.breadcrumb
            ),
            schema(json!({
                "type": "object",
                "properties": {
                    "filter": {
                        "type": "string",
                        "description": "Optional substring filter for var names"
                    }
                }
            })),
        ));

        // var_info — detailed info on a specific var
        tools.push(Tool::new(
            "var_info",
            format!(
                "Get detailed info on a specific variable of {}. Shows type, default, valid values.",
                self.type_path
            ),
            schema(json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Variable name to inspect"
                    }
                },
                "required": ["name"]
            })),
        ));

        // set_var — set a property value
        tools.push(Tool::new(
            "set_var",
            format!(
                "Set a variable on the {} being built. Validates against the objtree.",
                self.type_path
            ),
            schema(json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Variable name to set"
                    },
                    "value": {
                        "description": "Value to set (string, number, null, or path)"
                    }
                },
                "required": ["name", "value"]
            })),
        ));

        // edit — push sub-scope for a datum property
        tools.push(Tool::new(
            "edit",
            "Push a sub-scope to edit a datum-typed property. REPLACES the tool surface with that datum's tools. Use commit() to return.",
            schema(json!({
                "type": "object",
                "properties": {
                    "var_name": {
                        "type": "string",
                        "description": "Name of the datum-typed variable to edit"
                    },
                    "type_path": {
                        "type": "string",
                        "description": "Type path for the sub-datum (required if var has no default type)"
                    }
                },
                "required": ["var_name"]
            })),
        ));

        // validate — check readiness
        tools.push(Tool::new(
            "validate",
            format!(
                "Check if the {} prefab is ready. Reports missing required vars and current state.",
                self.type_path
            ),
            schema(json!({
                "type": "object",
                "properties": {}
            })),
        ));

        // commit — pop scope
        tools.push(Tool::new(
            "commit",
            "Finalize this scope and return to the parent. If root scope, produces the final prefab.",
            schema(json!({
                "type": "object",
                "properties": {}
            })),
        ));

        // discard — pop scope without saving
        tools.push(Tool::new(
            "discard",
            "Discard this scope's changes and return to the parent without saving.",
            schema(json!({
                "type": "object",
                "properties": {}
            })),
        ));

        // where_am_i — breadcrumb navigation
        tools.push(Tool::new(
            "where_am_i",
            "Show the current scope path (breadcrumb) and what vars have been set.",
            schema(json!({
                "type": "object",
                "properties": {}
            })),
        ));

        tools
    }
}

/// The scope stack — manages the builder lifecycle.
pub struct BuilderScopeStack {
    stack: Vec<BuilderScope>,
}

impl BuilderScopeStack {
    pub fn new() -> Self {
        Self { stack: Vec::new() }
    }

    /// Get the current (top) scope, if any.
    pub fn current(&self) -> Option<&BuilderScope> {
        self.stack.last()
    }

    /// Get the current (top) scope mutably.
    pub fn current_mut(&mut self) -> Option<&mut BuilderScope> {
        self.stack.last_mut()
    }

    /// Push a new scope onto the stack.
    pub fn push(&mut self, scope: BuilderScope) {
        self.stack.push(scope);
    }

    /// Pop the current scope. Returns it.
    pub fn pop(&mut self) -> Option<BuilderScope> {
        self.stack.pop()
    }

    /// Check if we're in a builder context.
    pub fn is_active(&self) -> bool {
        !self.stack.is_empty()
    }

    /// Get the depth (0 = not in builder, 1 = root, 2 = sub-scope, etc.)
    pub fn depth(&self) -> usize {
        self.stack.len()
    }
}

// ── Tool dispatch ────────────────────────────────────────────────────

/// Handle a builder tool call. Returns the response text and whether the scope changed.
pub fn handle_builder_call(
    state: &Arc<ServerState>,
    stack: &mut BuilderScopeStack,
    tool_name: &str,
    args: serde_json::Map<String, JsonValue>,
) -> Result<BuilderResponse, String> {
    match tool_name {
        "builder_init" => handle_init(state, stack, args),
        "list_vars" => handle_list_vars(state, stack, args),
        "var_info" => handle_var_info(state, stack, args),
        "set_var" => handle_set_var(state, stack, args),
        "edit" => handle_edit(state, stack, args),
        "validate" => handle_validate(state, stack, args),
        "commit" => handle_commit(state, stack, args),
        "discard" => handle_discard(state, stack, args),
        "where_am_i" => handle_where_am_i(state, stack, args),
        _ => Err(format!("Unknown builder tool: {}", tool_name)),
    }
}

// ── builder_init ─────────────────────────────────────────────────────

fn handle_init(
    state: &Arc<ServerState>,
    stack: &mut BuilderScopeStack,
    args: JsonMap<String, JsonValue>,
) -> Result<BuilderResponse, String> {
    if stack.is_active() {
        return Err("Builder already active. Use commit() or discard() to finish the current scope first.".into());
    }

    let type_path = args.get("type_path")
        .and_then(|v| v.as_str())
        .ok_or("Missing required parameter: type_path")?;

    // Validate the type exists in the objtree
    let type_ref = find_type(&state.objtree, type_path)
        .ok_or_else(|| format!("Type '{}' not found in the object tree", type_path))?;

    let scope = BuilderScope {
        type_path: type_path.to_string(),
        vars: BTreeMap::new(),
        parent_var: None,
        breadcrumb: short_name(type_path),
    };

    // Count available vars for the summary
    let var_count = count_settable_vars(&state.objtree, type_ref);

    stack.push(scope);

    Ok(BuilderResponse {
        text: format!(
            "Builder initialized for: {}\n\
             {} settable variables available.\n\n\
             Tool surface has been REPLACED with builder tools.\n\
             Use `list_vars` to see available properties, `set_var` to set them.\n\
             Use `commit()` when done, `discard()` to cancel.",
            type_path, var_count
        ),
        scope_changed: true,
    })
}

// ── list_vars ────────────────────────────────────────────────────────

fn handle_list_vars(
    state: &Arc<ServerState>,
    stack: &mut BuilderScopeStack,
    args: JsonMap<String, JsonValue>,
) -> Result<BuilderResponse, String> {
    let scope = stack.current()
        .ok_or("No active builder scope")?;

    let filter = args.get("filter").and_then(|v| v.as_str());

    let type_ref = find_type(&state.objtree, &scope.type_path)
        .ok_or_else(|| format!("Type '{}' not found", scope.type_path))?;

    let mut output = format!("Variables for {}:\n\n", scope.type_path);
    let mut count = 0;

    // Walk the type hierarchy to collect all vars
    let vars = collect_vars(&state.objtree, type_ref);

    for (name, default, declared_on) in &vars {
        // Apply filter
        if let Some(f) = filter {
            if !name.to_lowercase().contains(&f.to_lowercase()) {
                continue;
            }
        }

        count += 1;
        let set_indicator = if scope.vars.contains_key(name.as_str()) { " ✓" } else { "" };
        let default_str = match default {
            Some(c) => format_constant(c),
            None => "null".to_string(),
        };
        let inherited = if *declared_on != scope.type_path {
            format!(" (from {})", short_name(declared_on))
        } else {
            String::new()
        };
        output.push_str(&format!("  {}{} = {}{}\n", name, set_indicator, default_str, inherited));
    }

    output.push_str(&format!("\n{} variables{}", count, 
        if filter.is_some() { " (filtered)" } else { "" }
    ));

    let set_count = scope.vars.len();
    if set_count > 0 {
        output.push_str(&format!(", {} set", set_count));
    }

    Ok(BuilderResponse {
        text: output,
        scope_changed: false,
    })
}

// ── var_info ─────────────────────────────────────────────────────────

fn handle_var_info(
    state: &Arc<ServerState>,
    stack: &mut BuilderScopeStack,
    args: JsonMap<String, JsonValue>,
) -> Result<BuilderResponse, String> {
    let scope = stack.current()
        .ok_or("No active builder scope")?;

    let var_name = args.get("name")
        .and_then(|v| v.as_str())
        .ok_or("Missing required parameter: name")?;

    let type_ref = find_type(&state.objtree, &scope.type_path)
        .ok_or_else(|| format!("Type '{}' not found", scope.type_path))?;

    // Find the var value in the type hierarchy
    let var_value = type_ref.get_value(var_name);

    let mut output = format!("Variable: {}.{}\n\n", short_name(&scope.type_path), var_name);

    // Get the constant value (default)
    let constant = var_value.and_then(|v| v.constant.as_ref());
    output.push_str(&format!("Default: {}\n", match constant {
        Some(c) => format_constant(c),
        None => "null".to_string(),
    }));

    // Check if it's been set in current scope
    if let Some(current) = scope.vars.get(var_name) {
        output.push_str(&format!("Current: {} (set in this scope)\n", current));
    }

    // Try to get declared type info
    let decl = type_ref.get_var_declaration(var_name);
    let type_info = if let Some(d) = decl {
        let tp = &d.var_type.type_path;
        if !tp.is_empty() {
            let path_str: String = tp.iter().map(|ident| ident.as_str()).collect::<Vec<_>>().join("/");
            format!("/{}", path_str)
        } else {
            infer_type_from_constant(constant)
        }
    } else {
        infer_type_from_constant(constant)
    };

    output.push_str(&format!("Type: {}\n", type_info));

    // Check if this is a datum-typed var (can use edit())
    if type_info.starts_with('/') {
        output.push_str("\nThis is a datum-typed variable. Use `edit(var_name)` to configure it as a sub-assembly.\n");
    }

    Ok(BuilderResponse {
        text: output,
        scope_changed: false,
    })
}

// ── set_var ──────────────────────────────────────────────────────────

fn handle_set_var(
    state: &Arc<ServerState>,
    stack: &mut BuilderScopeStack,
    args: JsonMap<String, JsonValue>,
) -> Result<BuilderResponse, String> {
    let scope = stack.current_mut()
        .ok_or("No active builder scope")?;

    let var_name = args.get("name")
        .and_then(|v| v.as_str())
        .ok_or("Missing required parameter: name")?
        .to_string();

    let value = args.get("value")
        .ok_or("Missing required parameter: value")?
        .clone();

    let type_ref = find_type(&state.objtree, &scope.type_path)
        .ok_or_else(|| format!("Type '{}' not found", scope.type_path))?;

    // Verify the var exists on this type
    let var_exists = type_hierarchy_has_var(type_ref, &var_name);
    if !var_exists {
        return Err(format!(
            "Variable '{}' is not declared on {} or any parent type.\n\
             Use `list_vars` to see available variables.",
            var_name, scope.type_path
        ));
    }

    // Check the default
    let var_val = type_ref.get_value(&var_name);
    let default_str = var_val
        .and_then(|v| v.constant.as_ref())
        .map(|c| format_constant(c))
        .unwrap_or_else(|| "null".to_string());

    let old_value = scope.vars.get(&var_name).cloned();
    scope.vars.insert(var_name.clone(), value.clone());

    let mut output = format!("Set {}.{} = {}\n", short_name(&scope.type_path), var_name, value);
    if let Some(old) = old_value {
        output.push_str(&format!("Previous: {}\n", old));
    }
    output.push_str(&format!("Default: {}\n", default_str));
    output.push_str(&format!("\n{} variables set in this scope.", scope.vars.len()));

    Ok(BuilderResponse {
        text: output,
        scope_changed: false,
    })
}

// ── edit ─────────────────────────────────────────────────────────────

fn handle_edit(
    state: &Arc<ServerState>,
    stack: &mut BuilderScopeStack,
    args: JsonMap<String, JsonValue>,
) -> Result<BuilderResponse, String> {
    let var_name = args.get("var_name")
        .and_then(|v| v.as_str())
        .ok_or("Missing required parameter: var_name")?
        .to_string();

    // Get type path either from explicit arg or inferred from the var's declaration
    let explicit_type = args.get("type_path").and_then(|v| v.as_str()).map(|s| s.to_string());

    let (parent_type_path, parent_breadcrumb) = {
        let scope = stack.current()
            .ok_or("No active builder scope")?;
        (scope.type_path.clone(), scope.breadcrumb.clone())
    };

    let type_ref = find_type(&state.objtree, &parent_type_path)
        .ok_or_else(|| format!("Type '{}' not found", parent_type_path))?;

    // Determine the sub-datum type
    let sub_type_path = if let Some(tp) = explicit_type {
        // Verify it exists
        find_type(&state.objtree, &tp)
            .ok_or_else(|| format!("Type '{}' not found in the object tree", tp))?;
        tp
    } else {
        // Try to infer from the var's declared type
        let decl = type_ref.get_var_declaration(&var_name);
        if let Some(d) = decl {
            let tp = &d.var_type.type_path;
            if !tp.is_empty() {
                let path_str = format!("/{}", tp.iter().map(|i| i.as_str()).collect::<Vec<_>>().join("/"));
                // Verify it exists
                find_type(&state.objtree, &path_str)
                    .ok_or_else(|| format!("Inferred type '{}' not found", path_str))?;
                path_str
            } else {
                return Err(format!(
                    "Cannot infer datum type for '{}'. Provide type_path explicitly.\n\
                     Example: edit(var_name=\"{}\", type_path=\"/obj/item/...\")",
                    var_name, var_name
                ));
            }
        } else {
            return Err(format!(
                "Cannot infer datum type for '{}'. Provide type_path explicitly.\n\
                 Example: edit(var_name=\"{}\", type_path=\"/obj/item/...\")",
                var_name, var_name
            ));
        }
    };

    let breadcrumb = format!("{} > {}", parent_breadcrumb, var_name);
    let var_count = {
        let tr = find_type(&state.objtree, &sub_type_path).unwrap();
        count_settable_vars(&state.objtree, tr)
    };

    let scope = BuilderScope {
        type_path: sub_type_path.clone(),
        vars: BTreeMap::new(),
        parent_var: Some(var_name.clone()),
        breadcrumb: breadcrumb.clone(),
    };

    stack.push(scope);

    Ok(BuilderResponse {
        text: format!(
            "Editing sub-datum: {} (for var '{}')\n\
             Scope: {}\n\
             {} settable variables available.\n\n\
             Tool surface REPLACED with {}'s tools.\n\
             Use `commit()` to return to parent, `discard()` to cancel.",
            sub_type_path, var_name, breadcrumb, var_count, short_name(&sub_type_path)
        ),
        scope_changed: true,
    })
}

// ── validate ─────────────────────────────────────────────────────────

fn handle_validate(
    state: &Arc<ServerState>,
    stack: &mut BuilderScopeStack,
    _args: JsonMap<String, JsonValue>,
) -> Result<BuilderResponse, String> {
    let scope = stack.current()
        .ok_or("No active builder scope")?;

    let type_ref = find_type(&state.objtree, &scope.type_path)
        .ok_or_else(|| format!("Type '{}' not found", scope.type_path))?;

    let mut output = format!("Validation for {} (scope: {})\n\n", scope.type_path, scope.breadcrumb);

    // Show set vars
    if scope.vars.is_empty() {
        output.push_str("No variables set.\n");
    } else {
        output.push_str("Set variables:\n");
        for (name, value) in &scope.vars {
            let var_val = type_ref.get_value(name);
            let default_str = var_val
                .and_then(|v| v.constant.as_ref())
                .map(|c| format_constant(c))
                .unwrap_or_else(|| "null".to_string());
            let is_different = format!("{}", value) != default_str;
            output.push_str(&format!("  {} = {} {}\n", name, value,
                if is_different { "(overrides default)" } else { "(same as default — will be omitted)" }
            ));
        }
    }

    // Show what the final prefab would look like (non-default overrides only)
    let overrides = compute_overrides(&state.objtree, type_ref, &scope.vars);

    output.push_str(&format!("\nPrefab preview: {}", scope.type_path));
    if !overrides.is_empty() {
        output.push('{');
        for (i, (name, value)) in overrides.iter().enumerate() {
            if i > 0 { output.push_str("; "); }
            output.push_str(&format!("{} = {}", name, value));
        }
        output.push('}');
    }
    output.push('\n');

    output.push_str("\nReady to commit. Use `commit()` to finalize or continue editing.");

    Ok(BuilderResponse {
        text: output,
        scope_changed: false,
    })
}

// ── commit ───────────────────────────────────────────────────────────

fn handle_commit(
    state: &Arc<ServerState>,
    stack: &mut BuilderScopeStack,
    _args: JsonMap<String, JsonValue>,
) -> Result<BuilderResponse, String> {
    let scope = stack.pop()
        .ok_or("No active builder scope to commit")?;

    let is_root = !stack.is_active();

    if is_root {
        // Root commit — produce the final prefab string
        let type_ref = find_type(&state.objtree, &scope.type_path)
            .ok_or_else(|| format!("Type '{}' not found", scope.type_path))?;

        let overrides = compute_overrides(&state.objtree, type_ref, &scope.vars);

        let mut prefab = scope.type_path.clone();
        if !overrides.is_empty() {
            prefab.push('{');
            for (i, (name, value)) in overrides.iter().enumerate() {
                if i > 0 { prefab.push_str("; "); }
                prefab.push_str(&format!("{} = {}", name, value));
            }
            prefab.push('}');
        }

        Ok(BuilderResponse {
            text: format!(
                "Prefab committed:\n  {}\n\n\
                 Tool surface restored to base map tools.",
                prefab
            ),
            scope_changed: true,
        })
    } else {
        // Sub-scope commit — fold vars into parent as a JSON object
        let parent_var = scope.parent_var.clone()
            .ok_or("Sub-scope has no parent var")?;

        // Store as a structured value in the parent
        let sub_value = json!({
            "_type": scope.type_path,
            "_vars": scope.vars,
        });

        let parent = stack.current_mut()
            .ok_or("No parent scope")?;
        parent.vars.insert(parent_var.clone(), sub_value);

        Ok(BuilderResponse {
            text: format!(
                "Sub-datum '{}' committed with {} overrides.\n\
                 Returned to scope: {}",
                parent_var, scope.vars.len(), parent.breadcrumb
            ),
            scope_changed: true,
        })
    }
}

// ── discard ──────────────────────────────────────────────────────────

fn handle_discard(
    _state: &Arc<ServerState>,
    stack: &mut BuilderScopeStack,
    _args: JsonMap<String, JsonValue>,
) -> Result<BuilderResponse, String> {
    let scope = stack.pop()
        .ok_or("No active builder scope to discard")?;

    let is_root = !stack.is_active();

    if is_root {
        Ok(BuilderResponse {
            text: format!(
                "Discarded {} builder.\n\
                 Tool surface restored to base map tools.",
                scope.type_path
            ),
            scope_changed: true,
        })
    } else {
        let parent = stack.current()
            .ok_or("No parent scope")?;
        Ok(BuilderResponse {
            text: format!(
                "Discarded sub-datum for '{}'.\n\
                 Returned to scope: {}",
                scope.parent_var.unwrap_or_default(),
                parent.breadcrumb,
            ),
            scope_changed: true,
        })
    }
}

// ── where_am_i ───────────────────────────────────────────────────────

fn handle_where_am_i(
    _state: &Arc<ServerState>,
    stack: &mut BuilderScopeStack,
    _args: JsonMap<String, JsonValue>,
) -> Result<BuilderResponse, String> {
    let scope = stack.current()
        .ok_or("No active builder scope")?;

    let mut output = format!("Current scope: {}\n", scope.breadcrumb);
    output.push_str(&format!("Type: {}\n", scope.type_path));
    output.push_str(&format!("Depth: {}\n", stack.depth()));

    if scope.vars.is_empty() {
        output.push_str("No variables set yet.\n");
    } else {
        output.push_str(&format!("{} variables set:\n", scope.vars.len()));
        for (name, value) in &scope.vars {
            output.push_str(&format!("  {} = {}\n", name, value));
        }
    }

    Ok(BuilderResponse {
        text: output,
        scope_changed: false,
    })
}

// ── Objtree helpers ──────────────────────────────────────────────────

/// Find a type in the objtree by its full path.
fn find_type<'a>(objtree: &'a ObjectTree, path: &str) -> Option<TypeRef<'a>> {
    // Navigate the type path through the tree
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let mut current = objtree.root();

    for part in &parts {
        match current.child(part) {
            Some(child) => current = child,
            None => return None,
        }
    }

    // Don't return root if path was empty
    if parts.is_empty() {
        return None;
    }

    Some(current)
}

/// Check if any type in the hierarchy declares a given var.
fn type_hierarchy_has_var(type_ref: TypeRef<'_>, var_name: &str) -> bool {
    // Walk up the hierarchy checking each type's own vars
    let mut current = Some(type_ref);
    while let Some(tr) = current {
        if tr.get().vars.contains_key(var_name) {
            return true;
        }
        current = tr.parent_type();
    }
    false
}

/// Collect all settable vars for a type, walking the inheritance chain.
/// Returns (name, default_constant, declared_on_path).
fn collect_vars<'a>(
    _objtree: &'a ObjectTree,
    type_ref: TypeRef<'a>,
) -> Vec<(String, Option<&'a Constant>, String)> {
    let mut vars = BTreeMap::new();

    // Walk from the type up to root, collecting vars
    let mut chain = Vec::new();
    let mut current = Some(type_ref);
    while let Some(tr) = current {
        chain.push(tr);
        current = tr.parent_type();
    }

    // Process from root → specific so specific overrides land last
    for tr in chain.into_iter().rev() {
        let path = tr.get().path.clone();
        for (name, var) in &tr.get().vars {
            let name_str = name.as_str();
            // Skip special/internal vars
            if name_str.starts_with("__") || name_str == "type" || name_str == "parent_type" || name_str == "tag" {
                continue;
            }
            let default = var.value.constant.as_ref();
            vars.insert(name_str.to_string(), (name_str.to_string(), default, path.clone()));
        }
    }

    vars.into_values().collect()
}

/// Count settable vars (for summary messages).
fn count_settable_vars(objtree: &ObjectTree, type_ref: TypeRef<'_>) -> usize {
    collect_vars(objtree, type_ref).len()
}

/// Try to infer the type of a variable from its constant value.
fn infer_type_from_constant(constant: Option<&Constant>) -> String {
    match constant {
        Some(Constant::Null(_)) => "null".to_string(),
        Some(Constant::String(s)) => format!("string (default: {:?})", truncate(s.as_str(), 40)),
        Some(Constant::Float(f)) => {
            // Check if it's an integer value
            if *f == f.floor() && f.is_finite() {
                format!("number (default: {})", *f as i32)
            } else {
                format!("number (default: {})", f)
            }
        }
        Some(Constant::Prefab(p)) => {
            let path: String = p.path.iter()
                .map(|ident| ident.as_str())
                .collect::<Vec<_>>()
                .join("/");
            format!("prefab (default: /{})", path)
        }
        None => "unknown".to_string(),
        _ => "complex".to_string(),
    }
}

/// Format a Constant for display.
fn format_constant(c: &Constant) -> String {
    match c {
        Constant::Null(_) => "null".to_string(),
        Constant::String(s) => format!("{:?}", truncate(s.as_str(), 60)),
        Constant::Float(f) => {
            if *f == f.floor() && f.is_finite() {
                format!("{}", *f as i32)
            } else {
                format!("{}", f)
            }
        }
        Constant::Prefab(p) => {
            let path: String = p.path.iter()
                .map(|ident| ident.as_str())
                .collect::<Vec<_>>()
                .join("/");
            format!("/{}", path)
        }
        _ => format!("{:?}", c),
    }
}

/// Compute which vars differ from defaults (for the final DMM prefab string).
fn compute_overrides(
    _objtree: &ObjectTree,
    type_ref: TypeRef<'_>,
    vars: &BTreeMap<String, JsonValue>,
) -> Vec<(String, String)> {
    let mut overrides = Vec::new();

    for (name, value) in vars {
        let var_val = type_ref.get_value(name);
        let default_str = var_val
            .and_then(|v| v.constant.as_ref())
            .map(|c| format_constant(c))
            .unwrap_or_else(|| "null".to_string());

        // Format the value for DMM
        let value_str = json_to_dm_value(value);

        // Only include if different from default
        if value_str != default_str {
            overrides.push((name.clone(), value_str));
        }
    }

    overrides
}

/// Convert a JSON value to a DM value string for the DMM prefab format.
fn json_to_dm_value(value: &JsonValue) -> String {
    match value {
        JsonValue::String(s) => format!("{:?}", s),
        JsonValue::Number(n) => format!("{}", n),
        JsonValue::Bool(b) => if *b { "1".to_string() } else { "0".to_string() },
        JsonValue::Null => "null".to_string(),
        JsonValue::Object(obj) => {
            // Sub-datum: extract the type path and vars
            if let Some(tp) = obj.get("_type").and_then(|v| v.as_str()) {
                let mut s = tp.to_string();
                if let Some(vars) = obj.get("_vars").and_then(|v| v.as_object()) {
                    if !vars.is_empty() {
                        s.push('{');
                        for (i, (k, v)) in vars.iter().enumerate() {
                            if i > 0 { s.push_str("; "); }
                            s.push_str(&format!("{} = {}", k, json_to_dm_value(v)));
                        }
                        s.push('}');
                    }
                }
                s
            } else {
                format!("{}", value)
            }
        }
        JsonValue::Array(arr) => {
            let items: Vec<String> = arr.iter().map(json_to_dm_value).collect();
            format!("list({})", items.join(", "))
        }
    }
}

/// Short name for a type path (last component).
fn short_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Truncate a string for display.
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
}
