//! JavaScript rule DSL engine.
//!
//! Embeds QuickJS to evaluate map validation rules written in JavaScript.
//! Rules declare an anchor type and a check function. The engine only visits
//! tiles containing the anchor type, making evaluation O(anchor_count) per rule.
//!
//! # Rule format
//!
//! ```javascript
//! // rules/power.js
//! rule("apc-needs-terminal", {
//!   anchor: "/obj/machinery/power/apc",
//!   severity: "error",  // "error" | "warning" | "info"
//!   message: "APC has no adjacent terminal",
//!   check(obj, ctx) {
//!     return ctx.adjacent(obj.x, obj.y).some(o => o.path.startsWith("/obj/machinery/power/terminal"));
//!   }
//! });
//! ```

use std::path::PathBuf;
use anyhow::Result;
use rquickjs::context::EvalOptions;
use serde::{Deserialize, Serialize};

use crate::index::SpatialIndex;

/// A violation found by a rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Violation {
    /// Rule ID that triggered this violation
    pub rule_id: String,
    /// Severity level
    pub severity: String,
    /// Human-readable message
    pub message: String,
    /// Position of the anchor object that failed
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// Type path of the anchor object
    pub anchor_path: String,
}

/// A parsed rule definition from JavaScript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDef {
    /// Unique rule identifier
    pub id: String,
    /// Anchor type path — the engine only visits tiles with this type
    pub anchor: String,
    /// Severity: "error", "warning", or "info"
    pub severity: String,
    /// Default message template
    pub message: String,
    /// Source file this rule was loaded from
    pub source_file: String,
}

/// Result of evaluating all rules against a map.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    pub rules_evaluated: usize,
    pub anchors_checked: usize,
    pub violations: Vec<Violation>,
    pub errors: Vec<String>,
}

/// The rule engine — loads JS rules and evaluates them against a spatial index.
pub struct RuleEngine {
    rules_dir: PathBuf,
}

impl RuleEngine {
    pub fn new(rules_dir: PathBuf) -> Self {
        Self { rules_dir }
    }

    /// Discover all .js rule files in the rules directory.
    pub fn discover_rule_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        if !self.rules_dir.exists() {
            tracing::warn!("Rules directory does not exist: {}", self.rules_dir.display());
            return Ok(files);
        }
        for entry in std::fs::read_dir(&self.rules_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map(|e| e == "js").unwrap_or(false) {
                files.push(path);
            }
        }
        files.sort();
        Ok(files)
    }

    /// Load and evaluate all rules against the spatial index.
    pub fn evaluate(&self, index: &SpatialIndex) -> Result<ValidationResult> {
        let rule_files = self.discover_rule_files()?;
        if rule_files.is_empty() {
            return Ok(ValidationResult {
                rules_evaluated: 0,
                anchors_checked: 0,
                violations: Vec::new(),
                errors: vec!["No rule files found".into()],
            });
        }

        // Create the QuickJS runtime
        let rt = rquickjs::Runtime::new()?;
        let ctx = rquickjs::Context::full(&rt)?;

        let mut all_violations = Vec::new();
        let mut all_errors = Vec::new();
        let mut total_rules = 0;
        let mut total_anchors = 0;

        ctx.with(|ctx| -> Result<()> {
            // Install the global API
            install_globals(&ctx, index)?;

            for rule_file in &rule_files {
                let source = match std::fs::read_to_string(rule_file) {
                    Ok(s) => s,
                    Err(e) => {
                        all_errors.push(format!("Failed to read {}: {}", rule_file.display(), e));
                        continue;
                    }
                };

                let filename = rule_file.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();

                // Wrap user code so rule() calls register into __rules array
                let wrapped = format!(
                    r#"
                    (function() {{
                        const __rules = [];
                        function rule(id, def) {{
                            __rules.push({{
                                id: id,
                                anchor: def.anchor,
                                severity: def.severity || "warning",
                                message: def.message || ("Rule " + id + " failed"),
                                check: def.check,
                                source_file: "{}",
                            }});
                        }}
                        {};
                        return __rules;
                    }})()
                    "#,
                    filename.replace('\\', "\\\\").replace('"', "\\\""),
                    source
                );

                let mut eval_opts = EvalOptions::default();
                eval_opts.global = true;
                eval_opts.strict = true;
                eval_opts.backtrace_barrier = true;

                let rules_val: rquickjs::Value = match ctx.eval_with_options(
                    wrapped.as_bytes(),
                    eval_opts,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        all_errors.push(format!("Error loading {}: {}", filename, e));
                        continue;
                    }
                };

                // rules_val should be an array of rule objects
                let rules_arr = match rules_val.as_array() {
                    Some(a) => a,
                    None => {
                        all_errors.push(format!("{}: rules did not return an array", filename));
                        continue;
                    }
                };

                let rule_count = rules_arr.len();
                tracing::info!("Loaded {} rules from {}", rule_count, filename);

                for i in 0..rule_count {
                    let rule_obj: rquickjs::Value = match rules_arr.get(i) {
                        Ok(v) => v,
                        Err(e) => {
                            all_errors.push(format!("{}: failed to get rule {}: {}", filename, i, e));
                            continue;
                        }
                    };

                    let obj = match rule_obj.as_object() {
                        Some(o) => o,
                        None => {
                            all_errors.push(format!("{}: rule {} is not an object", filename, i));
                            continue;
                        }
                    };

                    // Extract rule metadata
                    let rule_id: String = match obj.get("id") {
                        Ok(v) => v,
                        Err(_) => format!("{}-rule-{}", filename, i),
                    };
                    let anchor: String = match obj.get("anchor") {
                        Ok(v) => v,
                        Err(e) => {
                            all_errors.push(format!("Rule '{}': missing anchor: {}", rule_id, e));
                            continue;
                        }
                    };
                    let severity: String = obj.get("severity").unwrap_or_else(|_| "warning".into());
                    let default_message: String = obj.get("message")
                        .unwrap_or_else(|_| format!("Rule '{}' failed", rule_id));
                    let check_fn: rquickjs::Function = match obj.get("check") {
                        Ok(v) => v,
                        Err(e) => {
                            all_errors.push(format!("Rule '{}': missing check function: {}", rule_id, e));
                            continue;
                        }
                    };

                    total_rules += 1;

                    // Area-anchor rules iterate over AREAS instead of object instances.
                    // An anchor starting with "/area/" matches every area whose path is
                    // a subtype of the anchor. The "object" passed to check() is a
                    // descriptor of the area with tile list and centroid.
                    let is_area_rule = anchor.starts_with("/area");
                    if is_area_rule {
                        // Collect matching areas (prefix match, like type anchors)
                        let area_paths: Vec<String> = index.all_areas()
                            .into_iter()
                            .filter(|p| p.starts_with(&anchor))
                            .map(|s| s.to_string())
                            .collect();
                        total_anchors += area_paths.len();

                        for area_path in area_paths {
                            let tiles = index.area_tiles(&area_path);
                            if tiles.is_empty() { continue; }

                            // Compute centroid + bbox + a representative z
                            let n = tiles.len() as f64;
                            let mut sx = 0i64; let mut sy = 0i64;
                            let (mut minx, mut miny) = (i32::MAX, i32::MAX);
                            let (mut maxx, mut maxy) = (i32::MIN, i32::MIN);
                            let mut z_rep = 1i32;
                            for &(x, y, z) in tiles {
                                sx += x as i64; sy += y as i64;
                                if x < minx { minx = x; } if x > maxx { maxx = x; }
                                if y < miny { miny = y; } if y > maxy { maxy = y; }
                                z_rep = z;
                            }
                            let cx = (sx as f64 / n).round() as i32;
                            let cy = (sy as f64 / n).round() as i32;

                            let area_json = serde_json::json!({
                                "path": area_path,
                                "tiles": tiles.iter().map(|&(x,y,z)| serde_json::json!([x,y,z])).collect::<Vec<_>>(),
                                "tile_count": tiles.len(),
                                "centroid": { "x": cx, "y": cy, "z": z_rep },
                                "bbox": { "x1": minx, "y1": miny, "x2": maxx, "y2": maxy },
                                "z": z_rep,
                                "x": cx, "y": cy,  // for {x}/{y}/{z} message substitution
                            });
                            let obj_val = json_to_js(&ctx, &area_json)?;
                            let ctx_val: rquickjs::Value = ctx.globals().get("__ctx")?;
                            let result: rquickjs::Value = match check_fn.call((obj_val, ctx_val)) {
                                Ok(v) => v,
                                Err(e) => {
                                    all_errors.push(format!(
                                        "Rule '{}' threw on area '{}': {}",
                                        rule_id, area_path, e
                                    ));
                                    continue;
                                }
                            };
                            let violation_message = interpret_check_result(&result, &default_message);
                            if let Some(msg) = violation_message {
                                all_violations.push(Violation {
                                    rule_id: rule_id.clone(),
                                    severity: severity.clone(),
                                    message: msg.replace("{x}", &cx.to_string())
                                        .replace("{y}", &cy.to_string())
                                        .replace("{z}", &z_rep.to_string())
                                        .replace("{area}", &area_path),
                                    x: cx, y: cy, z: z_rep,
                                    anchor_path: area_path.clone(),
                                });
                            }
                        }
                        continue;
                    }

                    // Get all anchor instances from the spatial index
                    let instances = index.instances_of(&anchor);
                    total_anchors += instances.len();

                    for inst in instances {
                        // Build the object descriptor for JS
                        let obj_json = serde_json::json!({
                            "path": inst.prefab.path,
                            "x": inst.x,
                            "y": inst.y,
                            "z": inst.z,
                            "vars": prefab_vars_to_json(&inst.prefab),
                        });

                        let obj_val = json_to_js(&ctx, &obj_json)?;

                        // Call check(obj, ctx) — ctx is the global __ctx
                        let ctx_val: rquickjs::Value = ctx.globals().get("__ctx")?;
                        let result: rquickjs::Value = match check_fn.call((obj_val, ctx_val)) {
                            Ok(v) => v,
                            Err(e) => {
                                all_errors.push(format!(
                                    "Rule '{}' threw at ({},{},{}): {}",
                                    rule_id, inst.x, inst.y, inst.z, e
                                ));
                                continue;
                            }
                        };

                        // Interpret result:
                        // - true/undefined/null → pass
                        // - false → fail with default message
                        // - string → fail with that message
                        let violation_message = interpret_check_result(&result, &default_message);
                        if let Some(msg) = violation_message {
                            all_violations.push(Violation {
                                rule_id: rule_id.clone(),
                                severity: severity.clone(),
                                message: msg.replace("{x}", &inst.x.to_string())
                                    .replace("{y}", &inst.y.to_string())
                                    .replace("{z}", &inst.z.to_string()),
                                x: inst.x,
                                y: inst.y,
                                z: inst.z,
                                anchor_path: inst.prefab.path.clone(),
                            });
                        }
                    }
                }
            }

            Ok(())
        })?;

        Ok(ValidationResult {
            rules_evaluated: total_rules,
            anchors_checked: total_anchors,
            violations: all_violations,
            errors: all_errors,
        })
    }
}

/// Install the global context API (__ctx) that rules call for spatial queries.
fn install_globals(ctx: &rquickjs::Ctx<'_>, index: &SpatialIndex) -> Result<()> {
    let globals = ctx.globals();

    // Serialize the entire spatial index query results into JS-callable functions.
    // We pre-build lookup data and expose it via closures.
    //
    // Strategy: We can't pass &SpatialIndex into QuickJS closures (lifetime issues),
    // so we pre-compute all the data JS rules might need and freeze it as JSON in the
    // JS context. For large maps this is memory-heavy but avoids unsafe Rust↔JS bridges.
    //
    // Alternative: Use rquickjs class API to create a native-backed ctx object.
    // Let's do the class approach for efficiency.

    // Build a JSON representation of the context API data that JS can query.
    // The __ctx object has methods backed by pre-serialized data.

    // Pre-build adjacency data for all tiles (keyed by "x,y,z")
    let mut tile_data: std::collections::HashMap<String, serde_json::Value> = std::collections::HashMap::new();

    for z in 1..=(index.dim_z as i32) {
        for y in 1..=(index.dim_y as i32) {
            for x in 1..=(index.dim_x as i32) {
                let objects = index.at(x, y, z);
                if objects.is_empty() {
                    continue;
                }
                let key = format!("{},{},{}", x, y, z);
                let objs_json: Vec<serde_json::Value> = objects.iter().map(|p| {
                    serde_json::json!({
                        "path": p.path,
                        "vars": prefab_vars_to_json(p),
                    })
                }).collect();
                tile_data.insert(key, serde_json::Value::Array(objs_json));
            }
        }
    }

    // Pre-build area data
    let mut area_data: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for area in index.all_areas() {
        let tiles = index.area_tiles(area);
        let keys: Vec<String> = tiles.iter().map(|(x, y, z)| format!("{},{},{}", x, y, z)).collect();
        area_data.insert(area.to_string(), keys);
    }

    // Serialize tile data to JS
    let tile_json = serde_json::to_string(&tile_data)?;
    let area_json = serde_json::to_string(&area_data)?;

    // Pre-build tile_area reverse map
    let mut tile_area_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for z in 1..=(index.dim_z as i32) {
        for y in 1..=(index.dim_y as i32) {
            for x in 1..=(index.dim_x as i32) {
                if let Some(area) = index.area_of(x, y, z) {
                    tile_area_map.insert(format!("{},{},{}", x, y, z), area.to_string());
                }
            }
        }
    }
    let tile_area_json = serde_json::to_string(&tile_area_map)?;

    // Install the context object with query methods
    let setup_script = format!(
        r#"
        (function() {{
            const __tiles = {};
            const __areas = {};
            const __tileAreas = {};

            function objectsAt(x, y, z) {{
                z = z || 1;
                return __tiles[x + "," + y + "," + z] || [];
            }}

            globalThis.__ctx = {{
                // Get all objects at a tile
                at: function(x, y, z) {{
                    return objectsAt(x, y, z);
                }},

                // Get objects on adjacent tiles (4-directional)
                adjacent: function(x, y, z) {{
                    z = z || 1;
                    var results = [];
                    var dirs = [[0,1],[0,-1],[1,0],[-1,0]];
                    for (var i = 0; i < dirs.length; i++) {{
                        var nx = x + dirs[i][0];
                        var ny = y + dirs[i][1];
                        var objs = objectsAt(nx, ny, z);
                        for (var j = 0; j < objs.length; j++) {{
                            results.push(Object.assign({{x: nx, y: ny, z: z}}, objs[j]));
                        }}
                    }}
                    return results;
                }},

                // Get objects within radius
                nearby: function(x, y, z, radius) {{
                    z = z || 1;
                    radius = radius || 3;
                    var results = [];
                    for (var dx = -radius; dx <= radius; dx++) {{
                        for (var dy = -radius; dy <= radius; dy++) {{
                            if (dx === 0 && dy === 0) continue;
                            var nx = x + dx;
                            var ny = y + dy;
                            var objs = objectsAt(nx, ny, z);
                            for (var j = 0; j < objs.length; j++) {{
                                results.push(Object.assign({{x: nx, y: ny, z: z}}, objs[j]));
                            }}
                        }}
                    }}
                    return results;
                }},

                // Get the area path for a tile
                areaOf: function(x, y, z) {{
                    z = z || 1;
                    return __tileAreas[x + "," + y + "," + z] || null;
                }},

                // Get all tile keys in an area
                areaTiles: function(areaPath) {{
                    return __areas[areaPath] || [];
                }},

                // Get all objects in an area
                objectsInArea: function(areaPath) {{
                    var tiles = __areas[areaPath] || [];
                    var results = [];
                    for (var i = 0; i < tiles.length; i++) {{
                        var objs = __tiles[tiles[i]] || [];
                        for (var j = 0; j < objs.length; j++) {{
                            var parts = tiles[i].split(",");
                            results.push(Object.assign({{
                                x: parseInt(parts[0]),
                                y: parseInt(parts[1]),
                                z: parseInt(parts[2])
                            }}, objs[j]));
                        }}
                    }}
                    return results;
                }},

                // Check if a type path matches (prefix match for subtypes)
                isType: function(obj, typePath) {{
                    return obj.path && obj.path.indexOf(typePath) === 0;
                }},

                // Get variable from object (with default)
                varOf: function(obj, name, defaultVal) {{
                    if (obj.vars && obj.vars[name] !== undefined) return obj.vars[name];
                    return defaultVal !== undefined ? defaultVal : null;
                }},

                // Get dir value and convert to dx,dy
                dirToDelta: function(dir) {{
                    // BYOND dir constants
                    var NORTH = 1, SOUTH = 2, EAST = 4, WEST = 8;
                    var dx = 0, dy = 0;
                    if (dir & NORTH) dy = 1;
                    if (dir & SOUTH) dy = -1;
                    if (dir & EAST) dx = 1;
                    if (dir & WEST) dx = -1;
                    return {{dx: dx, dy: dy}};
                }},

                // Infer BYOND dir from type path (e.g. /directional/north → 1)
                inferDir: function(obj) {{
                    // Check explicit var first
                    if (obj.vars && obj.vars.dir !== undefined) return parseInt(obj.vars.dir);
                    // Infer from path suffix (common in tgstation derivatives)
                    var p = obj.path;
                    if (p.indexOf("/north") === p.length - 6 || p.indexOf("/directional/north") !== -1) return 1;
                    if (p.indexOf("/south") === p.length - 6 || p.indexOf("/directional/south") !== -1) return 2;
                    if (p.indexOf("/east") === p.length - 5 || p.indexOf("/directional/east") !== -1) return 4;
                    if (p.indexOf("/west") === p.length - 5 || p.indexOf("/directional/west") !== -1) return 8;
                    return 2; // default south
                }},

                // Get the wall behind an object (using its dir or inferred from path)
                wallBehind: function(obj) {{
                    var dir = this.inferDir(obj);
                    var delta = this.dirToDelta(dir);
                    // The wall is in the direction the object faces
                    var wx = obj.x + delta.dx;
                    var wy = obj.y + delta.dy;
                    var z = obj.z || 1;
                    var objs = objectsAt(wx, wy, z);
                    for (var i = 0; i < objs.length; i++) {{
                        if (objs[i].path.indexOf("/turf/closed/wall") === 0) {{
                            return Object.assign({{x: wx, y: wy, z: z}}, objs[i]);
                        }}
                    }}
                    return null;
                }},
            }};
        }})();
        "#,
        tile_json, area_json, tile_area_json
    );

    let mut setup_opts = EvalOptions::default();
    setup_opts.global = true;
    setup_opts.strict = false; // needs globalThis assignment
    setup_opts.backtrace_barrier = true;

    let _: rquickjs::Value = ctx.eval_with_options(
        setup_script.as_bytes(),
        setup_opts,
    ).map_err(|e| anyhow::anyhow!("Failed to install context API: {}", e))?;

    Ok(())
}

/// Convert a Prefab's variable overrides to a JSON object.
fn prefab_vars_to_json(prefab: &dmm_tools::dmm::Prefab) -> serde_json::Value {
    use dreammaker::constants::Constant;

    let mut map = serde_json::Map::new();
    for (key, val) in prefab.vars.iter() {
        let json_val = match val {
            Constant::Null(_) => serde_json::Value::Null,
            Constant::Float(f) => {
                // BYOND uses f32 for all numbers (integers are just floats with no fractional part)
                if f.fract() == 0.0 && *f >= i64::MIN as f32 && *f <= i64::MAX as f32 {
                    serde_json::Value::Number((*f as i64).into())
                } else {
                    serde_json::Number::from_f64(*f as f64)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null)
                }
            }
            Constant::String(s) => serde_json::Value::String(s.to_string()),
            Constant::Resource(s) => serde_json::Value::String(s.to_string()),
            other => serde_json::Value::String(format!("{:?}", other)),
        };
        map.insert(key.clone(), json_val);
    }
    serde_json::Value::Object(map)
}

/// Convert a serde_json::Value to a rquickjs::Value.
fn json_to_js<'js>(ctx: &rquickjs::Ctx<'js>, val: &serde_json::Value) -> Result<rquickjs::Value<'js>> {
    let json_str = serde_json::to_string(val)?;
    let script = format!("JSON.parse('{}')", json_str.replace('\\', "\\\\").replace('\'', "\\'"));
    let result: rquickjs::Value = ctx.eval(script.as_bytes())
        .map_err(|e| anyhow::anyhow!("JSON parse error: {}", e))?;
    Ok(result)
}

/// Interpret the return value from a check function.
/// Returns None if passed, Some(message) if failed.
fn interpret_check_result(result: &rquickjs::Value, default_message: &str) -> Option<String> {
    // true → pass
    if let Some(b) = result.as_bool() {
        return if b { None } else { Some(default_message.to_string()) };
    }

    // undefined/null → pass
    if result.is_undefined() || result.is_null() {
        return None;
    }

    // string → fail with that message
    if let Some(s) = result.as_string() {
        if let Ok(msg) = s.to_string() {
            if msg.is_empty() {
                return None;
            }
            return Some(msg);
        }
    }

    // anything else → pass
    None
}
