//! Static analysis of update_overlays() procs to enumerate overlay sprites.
//!
//! Walks the AST to find `. +=` statements that add overlays, extracting
//! the icon_state names (including interpolated strings). Handles:
//! - Literal strings: `. += "smes-op1"`
//! - mutable_appearance calls: `. += mutable_appearance(icon, "state")`
//! - String interpolation: `. += mutable_appearance(icon, "apcox-[locked]")`
//! - Variable references: `. += icon_keyboard` (resolved from type vars)
//! - Conditional branches (if/switch) — collects all reachable overlay combos

use std::collections::{HashMap, HashSet};

use dreammaker::ast::*;
use dreammaker::constants::Constant;
use dreammaker::objtree::TypeRef;
use serde::Serialize;
use tracing::debug;

/// A single overlay state name that could appear on a type
#[derive(Debug, Clone, Serialize)]
pub struct OverlaySpec {
    /// The icon_state name for this overlay (may be a pattern with wildcards)
    pub state_name: String,
    /// The icon file path (if different from the type's default icon)
    pub icon_override: Option<String>,
    /// Whether this is conditional (inside an if/switch)
    pub conditional: bool,
    /// A description of the condition (for metadata)
    pub condition_desc: Option<String>,
}

/// A visual state represents one "mode" the object can be in at runtime.
/// For airlocks: closed, open, opening, closing each get a VisualState.
/// The base icon_state is overridden per visual state, and overlays are
/// resolved with the state's variable bindings.
#[derive(Debug, Clone, Serialize)]
pub struct VisualState {
    /// Human-readable name for this state (e.g. "closed", "open")
    pub name: String,
    /// Override for the base icon_state (None = use type's default)
    pub base_icon_state: Option<String>,
    /// Overlay state names to composite on this visual state
    pub overlays: Vec<String>,
    /// Description of what conditions produce this state
    pub condition_desc: Option<String>,
}

/// All overlay information extracted from a type's update_overlays() proc
#[derive(Debug, Clone, Serialize)]
pub struct OverlayInfo {
    pub type_path: String,
    /// Individual overlay specs found
    pub overlays: Vec<OverlaySpec>,
    /// Enumerated combinations of overlays that can appear together
    /// Each combo is a set of state names to composite
    pub combos: Vec<Vec<String>>,
    /// Visual states — state-machine-aware compositing targets.
    /// When present, the compositor should use these instead of combos.
    /// Each visual state specifies a base icon_state override + overlays.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub visual_states: Vec<VisualState>,
    /// Whether analysis was complete (false if we hit unknown patterns)
    pub complete: bool,
    /// Whether this was inherited from a parent type
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub inherited: bool,
}

/// Analyze a type's update_overlays proc and extract overlay state names.
/// Resolves variable references against the type's var declarations.
pub fn analyze_overlays(type_ref: &TypeRef) -> Option<OverlayInfo> {
    // Find the update_overlays proc
    let proc = type_ref.get().procs.get("update_overlays")?;
    let type_path = type_ref.get().path.clone();
    debug!("Found update_overlays proc on {}", type_path);
    let proc_value = proc.value.last()?;
    debug!("  proc_value found, code present: {}", proc_value.code.is_some());
    let code = proc_value.code.as_ref()?;

    debug!("Analyzing overlays for {}", type_path);

    let mut ctx = AnalysisContext::new(type_ref);
    walk_block(code, &mut ctx);

    if ctx.overlays.is_empty() {
        return None;
    }

    // Generate overlay combinations from the branch structure
    let combos = ctx.enumerate_combos();

    // Detect state-machine patterns and generate visual states
    let visual_states = ctx.enumerate_visual_states();

    Some(OverlayInfo {
        type_path,
        overlays: ctx.overlays,
        combos,
        visual_states,
        complete: ctx.complete,
        inherited: false,
    })
}

/// Create an inherited OverlayInfo for a child type that doesn't override update_overlays.
/// Substitutes the child's own var values into any pattern overlays.
pub fn inherit_overlays(type_ref: &TypeRef, parent_info: &OverlayInfo) -> Option<OverlayInfo> {
    let type_path = type_ref.get().path.clone();

    // Clone the parent's overlay info with the child's path
    let mut info = OverlayInfo {
        type_path: type_path.clone(),
        overlays: parent_info.overlays.clone(),
        combos: parent_info.combos.clone(),
        visual_states: parent_info.visual_states.clone(),
        complete: parent_info.complete,
        inherited: true,
    };

    // Try to resolve any interpolated patterns using the child type's vars
    for overlay in &mut info.overlays {
        if overlay.state_name.contains('[') {
            // Try to resolve variables in the pattern against this type's vars
            if let Some(resolved) = try_resolve_pattern_with_vars(&overlay.state_name, type_ref) {
                overlay.state_name = resolved;
            }
        }
    }

    // Rebuild combos with resolved names
    info.combos = info.combos.iter().map(|combo| {
        combo.iter().map(|name| {
            if name.contains('[') {
                try_resolve_pattern_with_vars(name, type_ref).unwrap_or_else(|| name.clone())
            } else {
                name.clone()
            }
        }).collect()
    }).collect();

    // Rebuild visual states with resolved names
    for vs in &mut info.visual_states {
        for overlay in &mut vs.overlays {
            if overlay.contains('[') {
                if let Some(resolved) = try_resolve_pattern_with_vars(overlay, type_ref) {
                    *overlay = resolved;
                }
            }
        }
    }

    Some(info)
}

/// Try to resolve a pattern like "[icon_keyboard]_off" using a type's var values
fn try_resolve_pattern_with_vars(pattern: &str, type_ref: &TypeRef) -> Option<String> {
    let mut result = String::new();
    let mut remaining = pattern;
    let mut any_resolved = false;

    while let Some(bracket_start) = remaining.find('[') {
        result.push_str(&remaining[..bracket_start]);

        if let Some(bracket_end) = remaining[bracket_start..].find(']') {
            let var_name = &remaining[bracket_start + 1..bracket_start + bracket_end];
            // Try to resolve the variable — strip field access like "src.var"
            let clean_var = var_name.split('.').last().unwrap_or(var_name);

            if let Some(value) = resolve_var_string(type_ref, clean_var) {
                result.push_str(&value);
                any_resolved = true;
            } else {
                // Can't resolve — keep the pattern
                result.push_str(&remaining[bracket_start..bracket_start + bracket_end + 1]);
            }
            remaining = &remaining[bracket_start + bracket_end + 1..];
        } else {
            // Malformed
            result.push_str(remaining);
            return None;
        }
    }
    result.push_str(remaining);

    if any_resolved {
        Some(result)
    } else {
        None
    }
}

/// Resolve a string var through the type inheritance chain
fn resolve_var_string(type_ref: &TypeRef, var_name: &str) -> Option<String> {
    let mut current = Some(*type_ref);
    while let Some(ty) = current {
        if let Some(var_val) = ty.get().vars.get(var_name) {
            if let Some(ref constant) = var_val.value.constant {
                match constant {
                    Constant::String(s) => return Some(s.to_string()),
                    Constant::Resource(r) => return Some(r.to_string()),
                    Constant::Null(_) => return None,
                    _ => {}
                }
            }
        }
        current = ty.parent_type();
    }
    None
}

/// Tracks a local variable that takes different values in different branches.
/// Used to detect state-machine patterns like `frame_state = "closed"` in one
/// switch arm and `frame_state = "open"` in another.
#[derive(Debug, Clone)]
struct BranchedVarAssignment {
    /// The branch path where this assignment occurred
    branch_path: String,
    /// The value assigned
    value: String,
    /// Condition description for this branch
    condition_desc: Option<String>,
}

/// Context for walking the AST
struct AnalysisContext<'a> {
    /// Type reference for variable resolution
    type_ref: &'a TypeRef<'a>,
    /// All overlays found
    overlays: Vec<OverlaySpec>,
    /// Current branch stack — tracks if we're inside conditionals
    branch_depth: usize,
    /// Current condition description
    current_condition: Option<String>,
    /// Whether we fully understood all overlay additions
    complete: bool,
    /// Overlay groups by branch path — for combo enumeration
    /// Key: branch path (e.g. "if_0:arm_1"), Value: overlay indices
    branch_groups: HashMap<String, Vec<usize>>,
    /// Current branch path
    branch_path: String,
    /// Unconditional overlays (always present)
    unconditional: Vec<usize>,
    /// Local variable assignments tracked during proc walk
    local_vars: HashMap<String, String>,
    /// Branched variable assignments — tracks vars that get different values
    /// in different switch/if arms. Key: var name, Value: list of assignments.
    branched_vars: HashMap<String, Vec<BranchedVarAssignment>>,
    /// Overlay specs that reference branched variables (contain interpolated patterns).
    /// These overlays need to be resolved per-visual-state.
    /// Stored as (overlay_spec_template, branch_path_where_added)
    stateful_overlays: Vec<(OverlaySpec, String)>,
}

impl<'a> AnalysisContext<'a> {
    fn new(type_ref: &'a TypeRef<'a>) -> Self {
        Self {
            type_ref,
            overlays: Vec::new(),
            branch_depth: 0,
            current_condition: None,
            complete: true,
            branch_groups: HashMap::new(),
            branch_path: String::new(),
            unconditional: Vec::new(),
            local_vars: HashMap::new(),
            branched_vars: HashMap::new(),
            stateful_overlays: Vec::new(),
        }
    }

    fn add_overlay(&mut self, state: String, icon_override: Option<String>) {
        let idx = self.overlays.len();
        let conditional = self.branch_depth > 0;

        self.overlays.push(OverlaySpec {
            state_name: state,
            icon_override,
            conditional,
            condition_desc: self.current_condition.clone(),
        });

        if conditional {
            self.branch_groups
                .entry(self.branch_path.clone())
                .or_default()
                .push(idx);
        } else {
            self.unconditional.push(idx);
        }
    }

    /// Try to resolve a variable name to a string value.
    /// Checks local proc vars first, then type vars via inheritance chain.
    ///
    /// IMPORTANT: If a variable has been assigned different values in different
    /// branches (detected via branched_vars), and we're currently OUTSIDE those
    /// branches (branch_depth is lower), return a `[var_name]` pattern instead
    /// of the last concrete value. This preserves the state-machine structure
    /// for later visual state enumeration.
    fn resolve_var(&self, name: &str) -> Option<String> {
        // Check if this variable has branched assignments (state machine pattern)
        if let Some(assignments) = self.branched_vars.get(name) {
            let unique_values: HashSet<&str> = assignments.iter()
                .map(|a| a.value.as_str())
                .collect();
            if unique_values.len() >= 2 {
                // This is a state variable — return pattern placeholder
                // so visual state enumeration can substitute each value
                return Some(format!("[{}]", name));
            }
        }

        // Check local vars first
        if let Some(val) = self.local_vars.get(name) {
            return Some(val.clone());
        }
        // Then type vars
        resolve_var_string(self.type_ref, name)
    }

    /// Enumerate all reachable overlay combinations
    fn enumerate_combos(&self) -> Vec<Vec<String>> {
        // Start with unconditional overlays
        let base: Vec<String> = self.unconditional.iter()
            .filter_map(|&i| self.overlays.get(i))
            .map(|o| o.state_name.clone())
            .collect();

        if self.branch_groups.is_empty() {
            if base.is_empty() {
                return Vec::new();
            }
            return vec![base];
        }

        // Group branches by their parent branch (same if/switch statement)
        let groups: Vec<Vec<String>> = self.branch_groups.values()
            .map(|indices| {
                indices.iter()
                    .filter_map(|&i| self.overlays.get(i))
                    .map(|o| o.state_name.clone())
                    .collect()
            })
            .collect();

        // Generate combos: base + each branch group independently
        let mut combos = Vec::new();

        // Base only (no conditional overlays active)
        if !base.is_empty() {
            combos.push(base.clone());
        }

        // Base + each branch group
        for group in &groups {
            let mut combo = base.clone();
            combo.extend(group.iter().cloned());
            combos.push(combo);
        }

        // Cap at a reasonable number to avoid explosion
        if combos.len() > 64 {
            combos.truncate(64);
        }

        combos
    }

    /// Detect state-machine patterns and enumerate visual states.
    ///
    /// Pattern detected: a local variable (like `frame_state`) is assigned different
    /// string values in different switch/if arms, AND that variable appears interpolated
    /// in overlay state names. When found, we generate one VisualState per unique value,
    /// with the base icon_state overridden to match.
    ///
    /// For airlocks: `frame_state` = "closed"|"open"|"opening"|"closing"
    /// Overlay templates: "fill_[frame_state]", "[frame_state]" (base frame)
    /// → generates 4 visual states, each with resolved overlay names and base override.
    fn enumerate_visual_states(&self) -> Vec<VisualState> {
        // Find branched variables that have multiple distinct values
        let mut state_vars: Vec<(&str, Vec<&BranchedVarAssignment>)> = Vec::new();

        for (var_name, assignments) in &self.branched_vars {
            // Collect unique values
            let unique_values: HashSet<&str> = assignments.iter()
                .map(|a| a.value.as_str())
                .collect();

            // Need at least 2 different values to be a state variable
            if unique_values.len() < 2 {
                continue;
            }

            // Check if this variable appears in any overlay interpolation patterns
            let var_pattern = format!("[{}]", var_name);
            let used_in_overlays = self.overlays.iter().any(|o| {
                o.state_name.contains(&var_pattern)
                    || contains_var_in_bracket(&o.state_name, var_name)
            });

            if used_in_overlays {
                state_vars.push((var_name.as_str(), assignments.iter().collect()));
            }
        }

        if state_vars.is_empty() {
            return Vec::new();
        }

        // When multiple state variables exist, pick the one referenced by more overlays.
        // For airlocks: frame_state appears in ~6 overlays, light_state in ~1.
        // The variable with more overlay references is the "primary" state dimension.
        if state_vars.len() > 1 {
            state_vars.sort_by(|(name_a, _), (name_b, _)| {
                let pattern_a = format!("[{}]", name_a);
                let pattern_b = format!("[{}]", name_b);
                let count_a = self.overlays.iter().filter(|o| o.state_name.contains(&pattern_a)).count();
                let count_b = self.overlays.iter().filter(|o| o.state_name.contains(&pattern_b)).count();
                count_b.cmp(&count_a) // Descending — most-referenced first
            });
            debug!("Multiple state variables detected ({:?}), using '{}' (most overlay refs)",
                state_vars.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
                state_vars[0].0);
        }

        let (state_var_name, assignments) = &state_vars[0];
        let var_pattern = format!("[{}]", state_var_name);

        // Deduplicate assignments by value (keep first condition desc)
        let mut seen_values: HashMap<String, Option<String>> = HashMap::new();
        for assignment in assignments {
            seen_values.entry(assignment.value.clone())
                .or_insert_with(|| assignment.condition_desc.clone());
        }

        // Collect the unconditional overlays (always present regardless of state)
        let unconditional_overlays: Vec<&OverlaySpec> = self.unconditional.iter()
            .filter_map(|&i| self.overlays.get(i))
            .collect();

        // Also collect conditional overlays that DON'T use the state variable
        // (they might be in sub-branches like welded/panel_open)

        let mut visual_states = Vec::new();

        for (value, condition_desc) in &seen_values {
            let mut state_overlays: Vec<String> = Vec::new();

            // Process all overlays, resolving the state variable
            for overlay in &self.overlays {
                let resolved = resolve_state_var_in_overlay(
                    &overlay.state_name, state_var_name, value, self.type_ref
                );
                if let Some(resolved) = resolved {
                    state_overlays.push(resolved);
                }
            }

            // Also add unconditional overlays that don't use the state variable at all
            let simple_pattern = format!("[{}]", state_var_name);
            for overlay in &unconditional_overlays {
                let has_var = overlay.state_name.contains(&simple_pattern)
                    || contains_var_in_bracket(&overlay.state_name, state_var_name);
                if !has_var {
                    state_overlays.push(overlay.state_name.clone());
                }
            }

            // The base icon_state for this visual state = the frame_state value itself.
            // This is the key insight: for airlocks, frame_state "closed" means both
            // the base icon is "closed" AND overlays use "fill_closed", "lights_closed", etc.
            // We also check if the first overlay added is just the bare frame_state
            // (`. += get_airlock_overlay(frame_state, icon, ...)`), which IS the base frame.
            let base_icon_state = Some(value.clone());

            visual_states.push(VisualState {
                name: value.clone(),
                base_icon_state,
                overlays: state_overlays,
                condition_desc: condition_desc.clone(),
            });
        }

        // Sort by name for deterministic output
        visual_states.sort_by(|a, b| a.name.cmp(&b.name));

        debug!("Generated {} visual states from state var '{}'",
            visual_states.len(), state_var_name);

        visual_states
    }
}

fn walk_block(block: &Block, ctx: &mut AnalysisContext) {
    for stmt in block.iter() {
        walk_statement(&stmt.elem, ctx);
    }
}

fn walk_statement(stmt: &Statement, ctx: &mut AnalysisContext) {
    match stmt {
        // `. += expr` — overlay addition
        Statement::Expr(Expression::AssignOp { op: AssignOp::AddAssign, lhs, rhs }) => {
            // Check that lhs is `.` (the return value)
            if is_dot_expr(lhs) {
                extract_overlay_from_expr(rhs, ctx);
            }
        }

        // `var = expr` — track local variable assignments for later resolution
        Statement::Expr(Expression::AssignOp { op: AssignOp::Assign, lhs, rhs }) => {
            if let Some(var_name) = extract_ident(lhs) {
                if let Some(value) = extract_string_from_expr(rhs) {
                    // If we're inside a branch, record as a branched assignment
                    if ctx.branch_depth > 0 {
                        ctx.branched_vars
                            .entry(var_name.clone())
                            .or_default()
                            .push(BranchedVarAssignment {
                                branch_path: ctx.branch_path.clone(),
                                value: value.clone(),
                                condition_desc: ctx.current_condition.clone(),
                            });
                    }
                    ctx.local_vars.insert(var_name, value);
                }
            }
        }

        // `var/name = expr` — variable declaration with assignment
        Statement::Var(var_stmt) => {
            if let Some(ref expr) = var_stmt.value {
                if let Some(value) = extract_string_from_expr(expr) {
                    // Declarations outside branches are just local vars
                    ctx.local_vars.insert(var_stmt.name.to_string(), value);
                }
            }
        }

        // `return` / `return ..()` — early return means conditional overlays stop
        Statement::Return(_) => {
            // Early returns naturally partition the overlay space
        }

        // if/else chains
        Statement::If { arms, else_arm } => {
            let old_depth = ctx.branch_depth;
            let old_path = ctx.branch_path.clone();
            let old_cond = ctx.current_condition.clone();
            ctx.branch_depth += 1;

            for (i, (cond, block)) in arms.iter().enumerate() {
                ctx.branch_path = format!("{}if{}:arm{}", old_path, old_depth, i);
                ctx.current_condition = Some(summarize_condition(&cond.elem));
                walk_block(block, ctx);
            }

            if let Some(else_block) = else_arm {
                ctx.branch_path = format!("{}if{}:else", old_path, old_depth);
                ctx.current_condition = Some("else".to_string());
                walk_block(else_block, ctx);
            }

            ctx.branch_depth = old_depth;
            ctx.branch_path = old_path;
            ctx.current_condition = old_cond;
        }

        // switch statements
        Statement::Switch { input, cases, default } => {
            let old_depth = ctx.branch_depth;
            let old_path = ctx.branch_path.clone();
            let old_cond = ctx.current_condition.clone();
            ctx.branch_depth += 1;

            let input_desc = summarize_expr(input);

            for (i, (case_values, block)) in cases.iter().enumerate() {
                ctx.branch_path = format!("{}sw{}:case{}", old_path, old_depth, i);
                ctx.current_condition = Some(format!("{}={:?}", input_desc, 
                    case_values.elem.iter().map(|c| summarize_case(c)).collect::<Vec<_>>()));
                walk_block(block, ctx);
            }

            if let Some(default_block) = default {
                ctx.branch_path = format!("{}sw{}:default", old_path, old_depth);
                ctx.current_condition = Some(format!("{} (default)", input_desc));
                walk_block(default_block, ctx);
            }

            ctx.branch_depth = old_depth;
            ctx.branch_path = old_path;
            ctx.current_condition = old_cond;
        }

        // for-in loops — can add overlays in loop body
        Statement::ForList(for_list) => {
            let old_depth = ctx.branch_depth;
            let old_path = ctx.branch_path.clone();
            let old_cond = ctx.current_condition.clone();
            ctx.branch_depth += 1;
            ctx.branch_path = format!("{}for{}:", old_path, old_depth);
            ctx.current_condition = Some("for loop iteration".to_string());
            walk_block(&for_list.block, ctx);
            ctx.branch_depth = old_depth;
            ctx.branch_path = old_path;
            ctx.current_condition = old_cond;
        }

        // Recurse into other statement types that might contain overlay adds
        _ => {}
    }
}

/// Extract overlay state names from an expression being added to `.`
fn extract_overlay_from_expr(expr: &Expression, ctx: &mut AnalysisContext) {
    match expr {
        // Literal string: `. += "smes-op1"`
        Expression::Base { term, follow } if follow.is_empty() => {
            match &term.elem {
                Term::String(s) => {
                    ctx.add_overlay(s.clone(), None);
                }

                // Interpolated string: `. += "[base_state]_suffix"`
                Term::InterpString(prefix, parts) => {
                    let pattern = resolve_interp_string(prefix, parts, ctx);
                    ctx.add_overlay(pattern, None);
                }

                // mutable_appearance(icon, "state") or mutable_appearance(icon, "[interp]")
                Term::Call(name, args) if name.as_str() == "mutable_appearance" => {
                    extract_mutable_appearance(args, ctx);
                }

                // emissive_appearance(icon, "state", src) — emissive overlays
                Term::Call(name, _args) if name.as_str() == "emissive_appearance" => {
                    // Skip emissive overlays — they're glow effects, not visual sprites
                }

                // get_airlock_overlay("state", ...) — airlock helper
                Term::Call(name, args) if name.as_str() == "get_airlock_overlay" => {
                    if let Some(first_arg) = args.first() {
                        if let Some(state) = extract_string_or_var(first_arg, ctx) {
                            ctx.add_overlay(state, None);
                        }
                    }
                }

                // image(...) calls — common overlay creation pattern
                Term::Call(name, args) if name.as_str() == "image" => {
                    extract_image_call(args, ctx);
                }

                // Variable reference: `. += icon_keyboard` where var holds a string
                Term::Ident(name) => {
                    if let Some(value) = ctx.resolve_var(name) {
                        debug!("Resolved var {} = {:?}", name, value);
                        ctx.add_overlay(value, None);
                    } else {
                        // Variable exists but we can't resolve it — mark with pattern
                        debug!("Unresolvable var overlay: {}", name);
                        ctx.add_overlay(format!("[{}]", name), None);
                        ctx.complete = false;
                    }
                }

                _ => {
                    debug!("Unknown overlay expression term: {:?}", term.elem);
                    ctx.complete = false;
                }
            }
        }

        // Function call with follows: something.method() result added as overlay
        Expression::Base { term, follow: _ } => {
            match &term.elem {
                Term::Call(name, args) if name.as_str() == "mutable_appearance" => {
                    extract_mutable_appearance(args, ctx);
                }
                Term::Call(name, args) if name.as_str() == "get_airlock_overlay" => {
                    if let Some(first_arg) = args.first() {
                        if let Some(state) = extract_string_or_var(first_arg, ctx) {
                            ctx.add_overlay(state, None);
                        }
                    }
                }
                Term::Call(name, args) if name.as_str() == "image" => {
                    extract_image_call(args, ctx);
                }
                _ => {
                    debug!("Unknown overlay base+follow: {:?}", expr);
                    ctx.complete = false;
                }
            }
        }

        _ => {
            debug!("Unknown overlay expression: {:?}", expr);
            ctx.complete = false;
        }
    }
}

/// Extract state name from mutable_appearance(icon, state, ...) call
fn extract_mutable_appearance(args: &[Expression], ctx: &mut AnalysisContext) {
    if args.len() < 2 {
        ctx.complete = false;
        return;
    }

    // First arg is icon file (might be `icon` variable or a resource literal)
    let icon_override = extract_resource_from_expr(&args[0]);

    // Second arg is the icon_state — try string first, then var resolution
    if let Some(state) = extract_string_or_var(&args[1], ctx) {
        ctx.add_overlay(state, icon_override);
    } else {
        debug!("Could not extract state from mutable_appearance arg: {:?}", args[1]);
        ctx.complete = false;
    }
}

/// Extract state info from image(icon, ..., icon_state, ...) calls
fn extract_image_call(args: &[Expression], ctx: &mut AnalysisContext) {
    // image() can be called as:
    //   image(icon, loc, icon_state, layer, dir)
    //   image(icon = ..., icon_state = ...) — named args
    // Most common: image('icon.dmi', src, "state")
    if args.len() >= 3 {
        let icon_override = extract_resource_from_expr(&args[0]);
        // Third arg (index 2) is typically icon_state
        if let Some(state) = extract_string_or_var(&args[2], ctx) {
            ctx.add_overlay(state, icon_override);
            return;
        }
    }
    if args.len() >= 2 {
        let icon_override = extract_resource_from_expr(&args[0]);
        if let Some(state) = extract_string_or_var(&args[1], ctx) {
            ctx.add_overlay(state, icon_override);
            return;
        }
    }
    debug!("Could not extract state from image() call");
    ctx.complete = false;
}

/// Try to extract a string value from an expression, falling back to variable resolution
fn extract_string_or_var(expr: &Expression, ctx: &AnalysisContext) -> Option<String> {
    // Try direct string extraction first
    if let Some(s) = extract_string_from_expr(expr) {
        return Some(s);
    }

    // Try variable resolution
    match expr {
        Expression::Base { term, follow } if follow.is_empty() => {
            match &term.elem {
                Term::Ident(name) => ctx.resolve_var(name),
                Term::InterpString(prefix, parts) => {
                    Some(resolve_interp_string(prefix, parts, ctx))
                }
                _ => None,
            }
        }
        // Handle src.var_name
        Expression::Base { term, follow } if follow.len() == 1 => {
            if let Term::Ident(base) = &term.elem {
                if base.as_str() == "src" {
                    if let Follow::Field(_, field_name) = &follow[0].elem {
                        return ctx.resolve_var(field_name);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Try to extract a string value from an expression (literal only, no var resolution)
fn extract_string_from_expr(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Base { term, follow } if follow.is_empty() => {
            match &term.elem {
                Term::String(s) => Some(s.clone()),
                Term::InterpString(_, _) => None, // Don't return patterns here; caller should use extract_string_or_var
                _ => None,
            }
        }
        _ => None,
    }
}

/// Extract an identifier name from an expression
fn extract_ident(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Base { term, follow } if follow.is_empty() => {
            match &term.elem {
                Term::Ident(name) => Some(name.to_string()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Try to extract a resource path from an expression  
fn extract_resource_from_expr(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Base { term, follow } if follow.is_empty() => {
            match &term.elem {
                Term::Resource(r) => Some(r.clone()),
                Term::Ident(id) if id.as_str() == "icon" => None, // `icon` = use type's default
                _ => None,
            }
        }
        _ => None,
    }
}

/// Resolve an interpolated string, replacing variable references with resolved values or wildcards
fn resolve_interp_string(prefix: &str, parts: &[(Option<Expression>, Box<str>)], ctx: &AnalysisContext) -> String {
    let mut result = prefix.to_string();

    for (expr, suffix) in parts {
        if let Some(ref expr) = expr {
            // Try to resolve the expression to a concrete value
            if let Some(value) = extract_string_or_var(expr, ctx) {
                result.push_str(&value);
            } else {
                // Fall back to wildcard with var name
                let var_name = summarize_expr(expr);
                result.push_str(&format!("[{}]", var_name));
            }
        }
        result.push_str(suffix);
    }

    result
}

/// Check if an expression is just `.` (the implicit return value)
fn is_dot_expr(expr: &Expression) -> bool {
    match expr {
        Expression::Base { term, follow } if follow.is_empty() => {
            matches!(&term.elem, Term::Ident(id) if id.as_str() == ".")
        }
        _ => false,
    }
}

/// Summarize a condition expression for human-readable descriptions
fn summarize_condition(expr: &Expression) -> String {
    summarize_expr(expr)
}

/// Summarize an expression to a human-readable string
fn summarize_expr(expr: &Expression) -> String {
    match expr {
        Expression::Base { term, follow } => {
            let mut s = match &term.elem {
                Term::Ident(id) => id.to_string(),
                Term::String(s) => format!("\"{}\"", s),
                Term::Int(i) => i.to_string(),
                Term::Float(f) => f.to_string(),
                Term::Null => "null".to_string(),
                _ => "...".to_string(),
            };
            for f in follow.iter() {
                match &f.elem {
                    Follow::Field(_, name) => {
                        s.push('.');
                        s.push_str(name);
                    }
                    Follow::Call(_, name, _) => {
                        s.push('.');
                        s.push_str(name);
                        s.push_str("()");
                    }
                    Follow::Index(_, _) => s.push_str("[...]"),
                    Follow::StaticField(name) => {
                        s.push_str("::");
                        s.push_str(name);
                    }
                    Follow::ProcReference(name) => {
                        s.push_str("::proc/");
                        s.push_str(name);
                    }
                    Follow::Unary(op) => {
                        s.push_str(&format!("{:?}", op));
                    }
                }
            }
            s
        }
        Expression::BinaryOp { op, lhs, rhs } => {
            format!("{} {:?} {}", summarize_expr(lhs), op, summarize_expr(rhs))
        }
        _ => "...".to_string(),
    }
}

fn summarize_case(case: &Case) -> String {
    match case {
        Case::Exact(expr) => summarize_expr(expr),
        Case::Range(a, b) => format!("{} to {}", summarize_expr(a), summarize_expr(b)),
    }
}

/// Check if a bracket expression `[...]` contains a reference to a variable name.
/// Handles patterns like `[frame_state Add fill_state_suffix]` or `[frame_state]`.
fn contains_var_in_bracket(overlay_name: &str, var_name: &str) -> bool {
    let mut remaining = overlay_name;
    while let Some(start) = remaining.find('[') {
        if let Some(end) = remaining[start..].find(']') {
            let bracket_content = &remaining[start + 1..start + end];
            // Check if var_name appears as a word in the bracket content
            // (not as substring of another var name)
            if bracket_content == var_name
                || bracket_content.starts_with(&format!("{} ", var_name))
                || bracket_content.contains(&format!(" {} ", var_name))
                || bracket_content.ends_with(&format!(" {}", var_name))
            {
                return true;
            }
            remaining = &remaining[start + end + 1..];
        } else {
            break;
        }
    }
    false
}

/// Resolve a state variable in an overlay name pattern.
/// Handles simple patterns like `[frame_state]` and complex ones like
/// `[frame_state Add fill_state_suffix]` by substituting the value and
/// attempting to simplify the expression.
///
/// Returns Some(resolved_name) if the overlay references the state variable,
/// or None if it doesn't.
fn resolve_state_var_in_overlay(
    overlay_name: &str,
    state_var_name: &str,
    value: &str,
    type_ref: &TypeRef,
) -> Option<String> {
    let simple_pattern = format!("[{}]", state_var_name);

    // Simple case: exact match like [frame_state]
    if overlay_name.contains(&simple_pattern) {
        return Some(overlay_name.replace(&simple_pattern, value));
    }

    // Complex case: bracket contains the variable name mixed with other things
    // e.g., [frame_state Add fill_state_suffix]
    if !contains_var_in_bracket(overlay_name, state_var_name) {
        return None;
    }

    // Walk through brackets and try to resolve
    let mut result = String::new();
    let mut remaining = overlay_name;

    while let Some(start) = remaining.find('[') {
        result.push_str(&remaining[..start]);

        if let Some(end) = remaining[start..].find(']') {
            let bracket_content = &remaining[start + 1..start + end];

            // Try to resolve the bracket expression
            if let Some(resolved) = resolve_bracket_expr(bracket_content, state_var_name, value, type_ref) {
                result.push_str(&resolved);
            } else {
                // Keep the bracket as-is
                result.push_str(&remaining[start..start + end + 1]);
            }
            remaining = &remaining[start + end + 1..];
        } else {
            result.push_str(remaining);
            return Some(result);
        }
    }
    result.push_str(remaining);
    Some(result)
}

/// Resolve a bracket expression like "frame_state Add fill_state_suffix"
/// by substituting the state variable value and simplifying.
fn resolve_bracket_expr(
    expr: &str,
    state_var_name: &str,
    value: &str,
    type_ref: &TypeRef,
) -> Option<String> {
    // Simple case: just the variable name
    if expr == state_var_name {
        return Some(value.to_string());
    }

    // Pattern: "var_a Add var_b" — string concatenation
    if let Some(rest) = expr.strip_prefix(state_var_name) {
        let rest = rest.trim();
        if let Some(other_var) = rest.strip_prefix("Add ") {
            let other_var = other_var.trim();
            // Try to resolve the other variable
            let other_value = resolve_var_string(type_ref, other_var)
                .unwrap_or_default(); // null/unset → empty string
            return Some(format!("{}{}", value, other_value));
        }
    }

    // Pattern: "var_b Add var_a" (reversed)
    if expr.contains(&format!("Add {}", state_var_name)) {
        let parts: Vec<&str> = expr.splitn(2, " Add ").collect();
        if parts.len() == 2 && parts[1].trim() == state_var_name {
            let other_var = parts[0].trim();
            let other_value = resolve_var_string(type_ref, other_var)
                .unwrap_or_default();
            return Some(format!("{}{}", other_value, value));
        }
    }

    None
}
