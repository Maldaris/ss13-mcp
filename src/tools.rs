//! MCP tool definitions for SS13 map intelligence.
//!
//! Uses a dynamic tool registry to support the builder pattern:
//! - Base tools (query, render, validate) are always available when no builder is active
//! - Builder tools replace the entire tool surface when a builder scope is active
//! - `notifications/tools/list_changed` fires on every scope transition

use std::sync::Arc;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, schemars, tool, tool_router};
use rmcp::model::{ServerInfo, CallToolResult, CallToolRequestParams, ListToolsResult, PaginatedRequestParams};
use rmcp::service::RequestContext;
use rmcp::RoleServer;
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::builder::BuilderScopeStack;
use crate::state::ServerState;

// ── Tool parameter types ─────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryTileParams {
    /// X coordinate
    pub x: i32,
    /// Y coordinate
    pub y: i32,
    /// Z level (default 1)
    pub z: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryAreaParams {
    /// Area type path (e.g. "/area/station/engineering/main")
    pub area_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryTypeParams {
    /// Object type path — matches subtypes (e.g. "/obj/machinery/power" matches "/obj/machinery/power/apc")
    pub type_path: String,
    /// Maximum results to return (default 50)
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryAdjacentParams {
    /// X coordinate
    pub x: i32,
    /// Y coordinate
    pub y: i32,
    /// Z level (default 1)
    pub z: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryNearbyParams {
    /// X coordinate
    pub x: i32,
    /// Y coordinate
    pub y: i32,
    /// Z level (default 1)
    pub z: Option<i32>,
    /// Search radius in tiles (default 3)
    pub radius: Option<i32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TraceNetworkParams {
    /// X coordinate of starting tile
    pub x: i32,
    /// Y coordinate of starting tile
    pub y: i32,
    /// Z level (default 1)
    pub z: Option<i32>,
    /// Network type path prefix (e.g. "/obj/structure/cable", "/obj/machinery/atmospherics/pipe")
    pub network_type: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchObjectsParams {
    /// Search query — matched against object type paths
    pub query: String,
    /// Maximum results to return (default 20)
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListAreasParams {
    /// Optional filter — only return areas matching this prefix
    pub prefix: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ValidateRulesParams {
    /// Optional: only run rules matching this ID prefix
    pub filter: Option<String>,
    /// Maximum violations to return (default 100)
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListRulesParams {
    // No params needed, but struct required for the tool macro
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RenderRegionParams {
    /// Left X coordinate (1-based, inclusive)
    pub x1: usize,
    /// Bottom Y coordinate (1-based, inclusive)
    pub y1: usize,
    /// Right X coordinate (1-based, inclusive)
    pub x2: usize,
    /// Top Y coordinate (1-based, inclusive)
    pub y2: usize,
    /// Z level (default 1)
    pub z: Option<usize>,
    /// Render pass filter: "pipes", "cables", "pipes-and-cables", or comma-separated pass names. Default shows everything.
    pub filter: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RenderAreaParams {
    /// Area type path (e.g. "/area/station/engineering/main")
    pub area_path: String,
    /// Render pass filter: "pipes", "cables", "pipes-and-cables", or comma-separated pass names. Default shows everything.
    pub filter: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SaveMapParams {
    /// Output file path. If omitted, overwrites the loaded map file.
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlacePrefabParams {
    /// X coordinate (1-based)
    pub x: i32,
    /// Y coordinate (1-based)
    pub y: i32,
    /// Z level (default 1)
    pub z: Option<i32>,
    /// Type path of the prefab to place (e.g. "/obj/structure/table")
    pub type_path: String,
    /// Optional variable overrides as JSON object (e.g. {"name": "My Table", "anchored": 1})
    pub vars: Option<serde_json::Map<String, serde_json::Value>>,
    /// If set, removes the first object matching this type path before placing
    pub replace: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RemoveObjectParams {
    /// X coordinate (1-based)
    pub x: i32,
    /// Y coordinate (1-based)
    pub y: i32,
    /// Z level (default 1)
    pub z: Option<i32>,
    /// Type path of the object to remove (first exact match)
    pub type_path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BatchPlacementParam {
    /// X coordinate (1-based)
    pub x: i32,
    /// Y coordinate (1-based)
    pub y: i32,
    /// Z level (default 1)
    pub z: Option<i32>,
    /// Type path of the prefab to place
    pub type_path: String,
    /// Optional variable overrides as JSON object
    pub vars: Option<serde_json::Map<String, serde_json::Value>>,
    /// If set, removes the first object matching this type path before placing
    pub replace: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlaceBatchParams {
    /// Array of placements to make in a single operation
    pub placements: Vec<BatchPlacementParam>,
}

// ── Tool implementations (static base tools) ─────────────────────────

/// Internal struct that holds just the static tool router.
/// The macro generates the ToolRouter from the method implementations.
pub(crate) struct StaticTools {
    pub state: Arc<ServerState>,
    tool_router: ToolRouter<Self>,
}

impl StaticTools {
    pub fn new(state: Arc<ServerState>) -> Self {
        Self {
            state: state.clone(),
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl StaticTools {
    /// Query all objects at a specific tile coordinate.
    #[tool(description = "Get all objects at a specific (x, y, z) tile on the map")]
    async fn query_tile(&self, Parameters(params): Parameters<QueryTileParams>) -> String {
        let z = params.z.unwrap_or(1);
        let map_data = self.state.map_data.read().await;
        let objects = map_data.index.at(params.x, params.y, z);

        if objects.is_empty() {
            return format!("No objects at ({}, {}, {})", params.x, params.y, z);
        }

        let mut result = format!("Objects at ({}, {}, {}):\n", params.x, params.y, z);
        for obj in objects {
            result.push_str(&format!("  {}\n", obj));
        }
        result
    }

    /// List all areas on the map, optionally filtered by prefix.
    #[tool(description = "List all area paths on the map, optionally filtered by a path prefix")]
    async fn list_areas(&self, Parameters(params): Parameters<ListAreasParams>) -> String {
        let map_data = self.state.map_data.read().await;
        let mut areas = map_data.index.all_areas();
        if let Some(prefix) = &params.prefix {
            areas.retain(|a| a.starts_with(prefix.as_str()));
        }
        areas.sort();

        let mut result = format!("{} areas found:\n", areas.len());
        for area in areas {
            let tile_count = map_data.index.area_tiles(area).len();
            result.push_str(&format!("  {} ({} tiles)\n", area, tile_count));
        }
        result
    }

    /// Get all objects within an area.
    #[tool(description = "Get all objects in a specific area, grouped by type")]
    async fn query_area(&self, Parameters(params): Parameters<QueryAreaParams>) -> String {
        let map_data = self.state.map_data.read().await;
        let objects = map_data.index.objects_in_area(&params.area_path);

        if objects.is_empty() {
            return format!("No objects in area '{}' (area may not exist)", params.area_path);
        }

        // Group by type path for readability
        let mut type_counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for obj in &objects {
            *type_counts.entry(&obj.prefab.path).or_default() += 1;
        }

        let tiles = map_data.index.area_tiles(&params.area_path);
        let mut result = format!(
            "Area '{}': {} tiles, {} objects\n\nObject types:\n",
            params.area_path,
            tiles.len(),
            objects.len()
        );
        for (type_path, count) in &type_counts {
            result.push_str(&format!("  {} ×{}\n", type_path, count));
        }
        result
    }

    /// Find all instances of a type (including subtypes) on the map.
    #[tool(description = "Find all instances of a type path on the map (matches subtypes). Returns positions and variable overrides.")]
    async fn query_object_type(&self, Parameters(params): Parameters<QueryTypeParams>) -> String {
        let limit = params.limit.unwrap_or(50);
        let map_data = self.state.map_data.read().await;
        let instances = map_data.index.instances_of(&params.type_path);

        if instances.is_empty() {
            return format!("No instances of '{}' on the map", params.type_path);
        }

        let total = instances.len();
        let mut result = format!("{} instances of '{}' found", total, params.type_path);
        if total > limit {
            result.push_str(&format!(" (showing first {}):\n", limit));
        } else {
            result.push_str(":\n");
        }

        for inst in instances.iter().take(limit) {
            result.push_str(&format!("  ({},{},{}) {}\n", inst.x, inst.y, inst.z, inst.prefab));
        }
        result
    }

    /// Get objects on tiles adjacent to a position (4-directional).
    #[tool(description = "Get all objects on tiles adjacent (N/S/E/W) to a position")]
    async fn query_adjacent(&self, Parameters(params): Parameters<QueryAdjacentParams>) -> String {
        let z = params.z.unwrap_or(1);
        let map_data = self.state.map_data.read().await;

        let dirs: [(i32, i32, &str); 4] = [
            (0, 1, "North"),
            (0, -1, "South"),
            (1, 0, "East"),
            (-1, 0, "West"),
        ];

        let mut result = format!("Adjacent to ({}, {}, {}):\n", params.x, params.y, z);
        for (dx, dy, dir_name) in &dirs {
            let nx = params.x + dx;
            let ny = params.y + dy;
            let tile_objects = map_data.index.at(nx, ny, z);
            if !tile_objects.is_empty() {
                result.push_str(&format!("  {} ({},{}):\n", dir_name, nx, ny));
                for obj in tile_objects {
                    result.push_str(&format!("    {}\n", obj));
                }
            }
        }
        result
    }

    /// Get all objects within a radius of a position.
    #[tool(description = "Get all objects within a tile radius of a position")]
    async fn query_nearby(&self, Parameters(params): Parameters<QueryNearbyParams>) -> String {
        let z = params.z.unwrap_or(1);
        let radius = params.radius.unwrap_or(3);
        let map_data = self.state.map_data.read().await;
        let nearby = map_data.index.nearby(params.x, params.y, z, radius);

        let mut result = format!(
            "{} objects within {} tiles of ({}, {}, {}):\n",
            nearby.len(), radius, params.x, params.y, z
        );

        // Group by position
        let mut by_pos: std::collections::BTreeMap<(i32, i32), Vec<&str>> = std::collections::BTreeMap::new();
        for obj in &nearby {
            by_pos.entry((obj.x, obj.y)).or_default().push(&obj.prefab.path);
        }
        for ((x, y), paths) in &by_pos {
            result.push_str(&format!("  ({},{}): {}\n", x, y, paths.join(", ")));
        }
        result
    }

    /// Trace a cable or pipe network from a starting position using BFS.
    #[tool(description = "Trace a connected network (cables, pipes, etc.) from a starting tile via BFS. Returns all connected positions.")]
    async fn trace_network(&self, Parameters(params): Parameters<TraceNetworkParams>) -> String {
        let z = params.z.unwrap_or(1);
        let map_data = self.state.map_data.read().await;
        let network = map_data.index.trace_network(params.x, params.y, z, &params.network_type);

        if network.is_empty() {
            return format!(
                "No '{}' network found at ({}, {}, {})",
                params.network_type, params.x, params.y, z
            );
        }

        let mut result = format!(
            "Network '{}' from ({},{},{}): {} connected tiles\n",
            params.network_type, params.x, params.y, z, network.len()
        );

        // Show which areas the network spans
        let mut area_counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for &(nx, ny, nz) in &network {
            if let Some(area) = map_data.index.area_of(nx, ny, nz) {
                *area_counts.entry(area).or_default() += 1;
            }
        }
        result.push_str("Areas spanned:\n");
        for (area, count) in &area_counts {
            result.push_str(&format!("  {} ({} tiles)\n", area, count));
        }
        result
    }

    /// Search object type paths on the map by substring.
    #[tool(description = "Search for object types on the map by substring match against type paths")]
    async fn search_objects(&self, Parameters(params): Parameters<SearchObjectsParams>) -> String {
        let limit = params.limit.unwrap_or(20);
        let query_lower = params.query.to_lowercase();
        let map_data = self.state.map_data.read().await;

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        let (dx, dy, dz) = (map_data.index.dim_x, map_data.index.dim_y, map_data.index.dim_z);
        for z in 1..=(dz as i32) {
            for y in 1..=(dy as i32) {
                for x in 1..=(dx as i32) {
                    for prefab in map_data.index.at(x, y, z) {
                        if prefab.path.to_lowercase().contains(&query_lower) {
                            seen.insert(prefab.path.clone());
                        }
                    }
                }
            }
        }

        let mut result = format!("Types matching '{}':\n", params.query);
        let mut sorted: Vec<_> = seen.into_iter().collect();
        sorted.sort();
        for (i, path) in sorted.iter().enumerate() {
            if i >= limit {
                result.push_str(&format!("  ... and {} more\n", sorted.len() - limit));
                break;
            }
            let count = map_data.index.instances_of(path).len();
            result.push_str(&format!("  {} (×{})\n", path, count));
        }
        result
    }

    /// Run all JS validation rules against the loaded map.
    #[tool(description = "Run JavaScript validation rules against the loaded map. Returns violations grouped by severity.")]
    async fn validate_rules(&self, Parameters(params): Parameters<ValidateRulesParams>) -> String {
        let engine = match &self.state.rule_engine {
            Some(e) => e,
            None => return "No rules directory configured. Use --rules <dir> to specify a rules directory.".into(),
        };

        let map_data = self.state.map_data.read().await;
        let result = match engine.evaluate(&map_data.index) {
            Ok(r) => r,
            Err(e) => return format!("Rule evaluation failed: {}", e),
        };

        let limit = params.limit.unwrap_or(100);

        let mut violations = result.violations;

        // Apply filter if specified
        if let Some(filter) = &params.filter {
            violations.retain(|v| v.rule_id.starts_with(filter.as_str()));
        }

        let total_violations = violations.len();

        let mut output = format!(
            "Validation complete: {} rules evaluated, {} anchors checked, {} violations\n",
            result.rules_evaluated, result.anchors_checked, total_violations
        );

        if !result.errors.is_empty() {
            output.push_str(&format!("\n{} errors during evaluation:\n", result.errors.len()));
            for err in &result.errors {
                output.push_str(&format!("  ⚠ {}\n", err));
            }
        }

        if violations.is_empty() {
            output.push_str("\n✅ No violations found.\n");
            return output;
        }

        // Group by severity
        let errors: Vec<_> = violations.iter().filter(|v| v.severity == "error").collect();
        let warnings: Vec<_> = violations.iter().filter(|v| v.severity == "warning").collect();
        let infos: Vec<_> = violations.iter().filter(|v| v.severity == "info").collect();

        if !errors.is_empty() {
            output.push_str(&format!("\n❌ {} errors:\n", errors.len()));
            for (i, v) in errors.iter().enumerate() {
                if i >= limit { 
                    output.push_str(&format!("  ... and {} more\n", errors.len() - limit));
                    break; 
                }
                output.push_str(&format!("  ({},{},{}) [{}] {}: {}\n",
                    v.x, v.y, v.z, v.rule_id, v.anchor_path, v.message));
            }
        }

        if !warnings.is_empty() {
            output.push_str(&format!("\n⚠️ {} warnings:\n", warnings.len()));
            for (i, v) in warnings.iter().enumerate() {
                if i >= limit {
                    output.push_str(&format!("  ... and {} more\n", warnings.len() - limit));
                    break;
                }
                output.push_str(&format!("  ({},{},{}) [{}] {}: {}\n",
                    v.x, v.y, v.z, v.rule_id, v.anchor_path, v.message));
            }
        }

        if !infos.is_empty() {
            output.push_str(&format!("\nℹ️ {} info:\n", infos.len()));
            for (i, v) in infos.iter().enumerate() {
                if i >= limit {
                    output.push_str(&format!("  ... and {} more\n", infos.len() - limit));
                    break;
                }
                output.push_str(&format!("  ({},{},{}) [{}] {}: {}\n",
                    v.x, v.y, v.z, v.rule_id, v.anchor_path, v.message));
            }
        }

        output
    }

    /// List available JS rule files and their rule definitions.
    #[tool(description = "List available JavaScript rule files in the rules directory")]
    fn list_rules(&self, Parameters(_params): Parameters<ListRulesParams>) -> String {
        let engine = match &self.state.rule_engine {
            Some(e) => e,
            None => return "No rules directory configured. Use --rules <dir> to specify a rules directory.".into(),
        };

        let files = match engine.discover_rule_files() {
            Ok(f) => f,
            Err(e) => return format!("Failed to scan rules directory: {}", e),
        };

        if files.is_empty() {
            return "No .js rule files found in rules directory.".into();
        }

        let mut output = format!("{} rule files found:\n", files.len());
        for file in &files {
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
            output.push_str(&format!("  {} ({} bytes)\n", name, size));
        }
        output
    }

    /// Render a rectangular region of the map as a PNG image.
    #[tool(description = "Render a rectangular region of the map to a PNG image. Returns base64-encoded PNG. Coordinates are 1-based. Use filter for layer-specific views: 'pipes', 'cables', 'pipes-and-cables'.")]
    fn render_region(&self, Parameters(params): Parameters<RenderRegionParams>) -> String {
        let z = params.z.unwrap_or(1);
        let filter = params.filter.as_deref();

        // Sanity check region size — cap at 64x64 tiles (2048x2048 px) to prevent OOM
        let width = params.x2.saturating_sub(params.x1) + 1;
        let height = params.y2.saturating_sub(params.y1) + 1;
        if width > 64 || height > 64 {
            return format!(
                "Region too large: {}x{} tiles (max 64x64). Use smaller bounds or render_area for auto-bounded areas.",
                width, height
            );
        }

        match crate::render::render_region(
            &self.state,
            params.x1,
            params.y1,
            params.x2,
            params.y2,
            z,
            filter,
        ) {
            Ok(result) => {
                let mut output = format!(
                    "Rendered region ({},{}) to ({},{}) z={}: {}x{} px ({}x{} tiles)\n",
                    params.x1, params.y1, params.x2, params.y2, z,
                    result.width_px, result.height_px,
                    result.width_tiles, result.height_tiles,
                );
                if !result.icon_errors.is_empty() {
                    output.push_str(&format!(
                        "\n⚠ {} icon errors (missing sprites):\n",
                        result.icon_errors.len()
                    ));
                    for (i, err) in result.icon_errors.iter().enumerate() {
                        if i >= 10 {
                            output.push_str(&format!("  ... and {} more\n", result.icon_errors.len() - 10));
                            break;
                        }
                        output.push_str(&format!("  {}\n", err));
                    }
                }
                output.push_str("\n[IMAGE:base64]\n");
                output.push_str(&result.base64_png);
                output
            }
            Err(e) => format!("Render failed: {}", e),
        }
    }

    /// Render an area of the map as a PNG image, auto-bounded to the area's extent.
    #[tool(description = "Render all tiles of a specific area as a PNG image. Automatically finds the area's bounding box and adds 1-tile padding. Use filter for layer-specific views.")]
    fn render_area(&self, Parameters(params): Parameters<RenderAreaParams>) -> String {
        let filter = params.filter.as_deref();

        match crate::render::render_area(&self.state, &params.area_path, filter) {
            Ok(Some(result)) => {
                let mut output = format!(
                    "Rendered area '{}': {}x{} px ({}x{} tiles)\n",
                    params.area_path,
                    result.width_px, result.height_px,
                    result.width_tiles, result.height_tiles,
                );
                if !result.icon_errors.is_empty() {
                    output.push_str(&format!(
                        "\n⚠ {} icon errors (missing sprites):\n",
                        result.icon_errors.len()
                    ));
                    for (i, err) in result.icon_errors.iter().enumerate() {
                        if i >= 10 {
                            output.push_str(&format!("  ... and {} more\n", result.icon_errors.len() - 10));
                            break;
                        }
                        output.push_str(&format!("  {}\n", err));
                    }
                }
                output.push_str("\n[IMAGE:base64]\n");
                output.push_str(&result.base64_png);
                output
            }
            Ok(None) => format!("Area '{}' not found on the map", params.area_path),
            Err(e) => format!("Render failed: {}", e),
        }
    }

    /// Save the map to a file (overwrite or new path).
    #[tool(description = "Save the current map state to a .dmm file. Without a path, overwrites the loaded file. Returns the number of dictionary entries.")]
    async fn save_map(&self, Parameters(params): Parameters<SaveMapParams>) -> String {
        let save_path = params.path
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.state.dmm_path.clone());

        let mut map_data = self.state.map_data.write().await;
        
        if !map_data.dirty {
            return format!("Map has no unsaved changes. Save path would be: {}", save_path.display());
        }

        match map_data.save(&save_path) {
            Ok(()) => {
                let dict_size = map_data.map.dictionary.len();
                format!(
                    "✅ Map saved to: {}\n  {} dictionary entries\n  Map marked clean.",
                    save_path.display(), dict_size
                )
            }
            Err(e) => format!("❌ Save failed: {}", e),
        }
    }

    /// Place a prefab directly on a map tile.
    #[tool(description = "Place an object on a map tile. Provide a type path and optional var overrides. Use 'replace' to swap an existing object. Vars support: strings, numbers, paths (/obj/...), lists ([1,2,3]), null.")]
    async fn place_prefab(&self, Parameters(params): Parameters<PlacePrefabParams>) -> String {
        let z = params.z.unwrap_or(1);

        // Build the prefab
        let mut vars = std::collections::BTreeMap::new();
        if let Some(var_map) = params.vars {
            for (k, v) in var_map {
                vars.insert(k, v);
            }
        }
        let prefab = crate::state::build_prefab(&params.type_path, &vars, &self.state.objtree);
        let prefab_str = format!("{}", prefab);

        let mut map_data = self.state.map_data.write().await;

        // Validate bounds
        let (dim_x, dim_y, dim_z) = (map_data.index.dim_x, map_data.index.dim_y, map_data.index.dim_z);
        if params.x < 1 || params.x > dim_x as i32 || params.y < 1 || params.y > dim_y as i32 || z < 1 || z > dim_z as i32 {
            return format!("❌ Coordinates ({},{},{}) out of bounds ({}x{}x{})",
                params.x, params.y, z, dim_x, dim_y, dim_z);
        }

        // Remove replacement if specified
        let mut replaced = false;
        if let Some(rp) = &params.replace {
            match map_data.remove_prefab(params.x, params.y, z, rp) {
                Ok(true) => { replaced = true; },
                Ok(false) => {
                    return format!("⚠ Replace target '{}' not found at ({},{},{})", rp, params.x, params.y, z);
                }
                Err(e) => return format!("❌ Remove failed: {}", e),
            }
        }

        match map_data.place_prefab(params.x, params.y, z, prefab) {
            Ok(()) => {
                let mut output = format!("✅ Placed at ({},{},{}): {}", params.x, params.y, z, prefab_str);
                if replaced {
                    output.push_str(&format!("\n  Replaced: {}", params.replace.unwrap()));
                }
                output
            }
            Err(e) => format!("❌ Placement failed: {}", e),
        }
    }

    /// Remove an object from a map tile.
    #[tool(description = "Remove the first object matching a type path from a tile.")]
    async fn remove_object(&self, Parameters(params): Parameters<RemoveObjectParams>) -> String {
        let z = params.z.unwrap_or(1);
        let mut map_data = self.state.map_data.write().await;

        match map_data.remove_prefab(params.x, params.y, z, &params.type_path) {
            Ok(true) => format!("✅ Removed '{}' from ({},{},{})", params.type_path, params.x, params.y, z),
            Ok(false) => format!("⚠ '{}' not found at ({},{},{})", params.type_path, params.x, params.y, z),
            Err(e) => format!("❌ Remove failed: {}", e),
        }
    }

    /// Place multiple objects in a single batch operation.
    #[tool(description = "Place multiple objects on map tiles in one call. Each placement has x, y, z, type_path, optional vars, and optional replace. Much more efficient than individual place_prefab calls. Returns a summary of results.")]
    async fn place_batch(&self, Parameters(params): Parameters<PlaceBatchParams>) -> String {
        let placements: Vec<crate::state::BatchPlacement> = params.placements.into_iter().map(|p| {
            let vars = p.vars.map(|vm| {
                let mut map = std::collections::BTreeMap::new();
                for (k, v) in vm { map.insert(k, v); }
                map
            });
            crate::state::BatchPlacement {
                x: p.x,
                y: p.y,
                z: p.z.unwrap_or(1),
                type_path: p.type_path,
                vars,
                replace: p.replace,
            }
        }).collect();

        let count = placements.len();
        let mut map_data = self.state.map_data.write().await;
        let results = map_data.place_batch(placements, &self.state.objtree);

        let ok_count = results.iter().filter(|r| r.ok).count();
        let fail_count = count - ok_count;

        let mut output = format!("Batch: {}/{} placed successfully", ok_count, count);
        if fail_count > 0 {
            output.push_str(&format!("\n❌ {} failures:", fail_count));
            for r in &results {
                if !r.ok {
                    output.push_str(&format!("\n  ({},{},{}) {} — {}",
                        r.x, r.y, r.z, r.type_path, r.error.as_deref().unwrap_or("unknown")));
                }
            }
        }
        output
    }
}

// ── Dynamic MCP Server Handler ───────────────────────────────────────

/// The main MCP server handler with dynamic tool routing.
/// 
/// Tool dispatch follows scope-stack semantics:
/// - When no builder is active: serves static base tools (query, render, validate)
/// - When a builder scope is active: serves ONLY that scope's tools
/// - Scope transitions fire `notifications/tools/list_changed`
pub struct MapTools {
    /// The loaded map state (objtree, spatial index, renderer, etc.)
    pub state: Arc<ServerState>,
    /// Static base tools (generated by #[tool_router] macro)
    static_tools: StaticTools,
    /// Builder scope stack — when non-empty, replaces the entire tool surface
    pub(crate) scope_stack: RwLock<BuilderScopeStack>,
}

impl MapTools {
    pub fn new(state: Arc<ServerState>) -> Self {
        let static_tools = StaticTools::new(state.clone());
        Self {
            state,
            static_tools,
            scope_stack: RwLock::new(BuilderScopeStack::new()),
        }
    }
}

/// Manual ServerHandler implementation for dynamic tool dispatch.
///
/// Instead of using #[tool_handler] which generates a static list_tools/call_tool,
/// we implement these methods ourselves to support scope-based tool switching.
impl ServerHandler for MapTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "SS13 Map Intelligence — query tiles, areas, objects, networks, validate maps, \
                 render regions, and assemble + place prefabs with the builder pattern.\n\n\
                 ## Builder Pattern\n\
                 Use `builder_init(type_path)` to start assembling a prefab. This REPLACES the \
                 tool surface with builder-specific tools:\n\
                 - `list_vars` — enumerate available properties with types/defaults\n\
                 - `var_info(name)` — detailed info on a specific var\n\
                 - `set_var(name, value)` — set with validation. Supports strings, numbers, \
                   paths (/obj/...), lists ([1,2,3]), assoc maps ({\"key\": val}), resources ('icon.dmi')\n\
                 - `edit(var_name)` — push a sub-scope for a datum property (replaces tools again)\n\
                 - `validate()` — check readiness\n\
                 - `commit()` — pop scope. Root scope can place on tile with x,y,z args.\n\
                 - `discard()` — pop scope without saving\n\n\
                 ## Map Mutation\n\
                 When committing a root scope, provide x,y,z to place the prefab on a tile.\n\
                 Use `save_map()` to write changes to disk. Changes are tracked (dirty flag).\n\n\
                 The tool list changes on every scope transition. Only one scope is visible at a time."
                .into()
            ),
            ..ServerInfo::default()
        }
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, rmcp::ErrorData>> + Send + '_ {
        async move {
            let stack = self.scope_stack.read().await;
            let tools = if let Some(scope) = stack.current() {
                let is_root = stack.is_root();
                scope.tools(is_root)
            } else {
                // Static tools + builder_init entry point
                let mut tools = self.static_tools.tool_router.list_all();
                tools.push(Self::builder_init_tool());
                tools
            };
            Ok(ListToolsResult {
                tools,
                meta: None,
                next_cursor: None,
            })
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResult, rmcp::ErrorData>> + Send + '_ {
        async move {
            let tool_name = request.name.clone();
            
            // Check if we're in a builder scope
            {
                let stack = self.scope_stack.read().await;
                if stack.current().is_some() {
                    // In builder scope — dispatch to builder
                    drop(stack); // Release read lock before taking write lock
                    return self.dispatch_builder_tool(&tool_name, request, context).await;
                }
            }

            // Not in builder scope — check if this is a builder_init call
            if tool_name.as_ref() == "builder_init" {
                return self.dispatch_builder_tool(&tool_name, request, context).await;
            }

            // Dispatch to static tools
            let tcc = rmcp::handler::server::tool::ToolCallContext::new(
                &self.static_tools,
                request,
                context,
            );
            self.static_tools.tool_router.call(tcc).await
        }
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        // This is synchronous so we can't await the RwLock.
        // For the get_tool case, try static first (it's used for capability checks).
        self.static_tools.tool_router.get(name).cloned()
    }
}

impl MapTools {
    /// Create the Tool definition for builder_init (shown in the base tool list).
    fn builder_init_tool() -> rmcp::model::Tool {
        rmcp::model::Tool::new(
            "builder_init",
            "Start assembling a new prefab. REPLACES the entire tool surface with builder tools. Use list_vars, set_var, validate, commit to build incrementally.",
            Arc::new({
                let mut map = serde_json::Map::new();
                map.insert("type".to_string(), serde_json::json!("object"));
                map.insert("properties".to_string(), serde_json::json!({
                    "type_path": {
                        "type": "string",
                        "description": "DM type path to build (e.g. \"/obj/machinery/door/airlock/engineering\")"
                    }
                }));
                map.insert("required".to_string(), serde_json::json!(["type_path"]));
                map
            }),
        )
    }

    /// Dispatch a tool call to the builder system.
    /// Handles scope transitions, placement mutations, and fires notifications.
    async fn dispatch_builder_tool(
        &self,
        tool_name: &str,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let peer = context.peer.clone();
        
        let result = {
            let mut stack = self.scope_stack.write().await;
            crate::builder::handle_builder_call(&self.state, &mut stack, tool_name, args)
        };

        match result {
            Ok(response) => {
                // Check for placement request (root commit with coordinates)
                let final_text = if response.text.starts_with("PLACEMENT_PENDING:") {
                    self.handle_placement(&response.text).await
                } else {
                    response.text
                };

                // If the tool call changed the scope, notify the client
                if response.scope_changed {
                    if let Err(e) = peer.notify_tool_list_changed().await {
                        tracing::warn!("Failed to notify tool list changed: {}", e);
                    }
                }
                Ok(CallToolResult::success(vec![
                    rmcp::model::Content::text(final_text),
                ]))
            }
            Err(msg) => {
                Ok(CallToolResult::error(vec![
                    rmcp::model::Content::text(msg),
                ]))
            }
        }
    }

    /// Handle a placement request from root commit.
    /// Parses the PLACEMENT_PENDING header and mutates the map.
    async fn handle_placement(&self, text: &str) -> String {
        // Parse "PLACEMENT_PENDING:x:y:z:replace_path:prefab_str\n..."
        let first_line = text.lines().next().unwrap_or("");
        let parts: Vec<&str> = first_line.splitn(6, ':').collect();
        
        if parts.len() < 5 {
            return format!("Internal error: malformed placement header\n{}", text);
        }

        let x: i32 = parts[1].parse().unwrap_or(0);
        let y: i32 = parts[2].parse().unwrap_or(0);
        let z: i32 = parts[3].parse().unwrap_or(1);
        let replace_path = if parts[4].is_empty() { None } else { Some(parts[4]) };

        // Reconstruct the prefab from the builder's last scope
        // We need to rebuild it since we can't pass complex objects through text
        // The args were already consumed, so we parse the prefab line
        let prefab_line = text.lines()
            .find(|l| l.starts_with("Prefab: "))
            .map(|l| &l[8..])
            .unwrap_or("");

        // Build the prefab from the text representation
        // For proper placement, rebuild from the type path and vars
        let prefab = parse_prefab_display(prefab_line);

        let mut map_data = self.state.map_data.write().await;
        
        // Validate coordinates are in bounds
        let (dim_x, dim_y, dim_z) = (map_data.index.dim_x, map_data.index.dim_y, map_data.index.dim_z);
        if x < 1 || x > dim_x as i32 || y < 1 || y > dim_y as i32 || z < 1 || z > dim_z as i32 {
            return format!(
                "Coordinates ({},{},{}) out of bounds. Map dimensions: {}x{}x{}\n{}",
                x, y, z, dim_x, dim_y, dim_z,
                &text[text.find('\n').unwrap_or(0)..],
            );
        }

        // Remove replacement if specified
        if let Some(rp) = replace_path {
            match map_data.remove_prefab(x, y, z, rp) {
                Ok(true) => {},
                Ok(false) => {
                    tracing::warn!("Replace target '{}' not found at ({},{},{})", rp, x, y, z);
                }
                Err(e) => {
                    return format!("Failed to remove '{}': {}", rp, e);
                }
            }
        }

        // Place the new prefab
        match map_data.place_prefab(x, y, z, prefab) {
            Ok(()) => {
                format!(
                    "✅ Prefab placed at ({}, {}, {})\n  {}\n{}\n\
                     Map has unsaved changes. Use `save_map()` to write to disk.\n\
                     Tool surface restored to base map tools.",
                    x, y, z, prefab_line,
                    replace_path.map(|rp| format!("  Replaced: {}\n", rp)).unwrap_or_default(),
                )
            }
            Err(e) => format!("❌ Placement failed: {}\n  {}", e, prefab_line),
        }
    }
}

/// Parse a Prefab from its Display format: "/path/to/type{var = val; var2 = val2}"
/// This is a best-effort parser for round-tripping through the text protocol.
fn parse_prefab_display(s: &str) -> dmm_tools::dmm::Prefab {
    let s = s.trim();
    
    // Split into path and vars
    if let Some(brace_start) = s.find('{') {
        let path = s[..brace_start].trim().to_string();
        let vars_str = &s[brace_start+1..s.len()-1]; // strip { and }
        
        let mut prefab = dmm_tools::dmm::Prefab::from_path(path);
        
        // Parse semicolon-separated var = value pairs
        for pair in vars_str.split(';') {
            let pair = pair.trim();
            if pair.is_empty() { continue; }
            
            if let Some(eq_pos) = pair.find('=') {
                let name = pair[..eq_pos].trim();
                let value_str = pair[eq_pos+1..].trim();
                
                // Parse the value back into a Constant
                let constant = parse_dm_value(value_str);
                prefab.vars.insert(name.to_string(), constant);
            }
        }
        
        prefab
    } else {
        dmm_tools::dmm::Prefab::from_path(s.to_string())
    }
}

/// Parse a DM value string back into a Constant.
/// Handles: numbers, strings ("..."), null, paths (/foo/bar), resources ('foo.dmi'), lists.
fn parse_dm_value(s: &str) -> dreammaker::constants::Constant {
    use dreammaker::ast::Ident;
    use dreammaker::constants::Constant;
    
    let s = s.trim();
    
    if s == "null" {
        Constant::Null(None)
    } else if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        // String literal — unescape basic escapes
        let inner = &s[1..s.len()-1];
        let unescaped = inner.replace("\\\"", "\"").replace("\\n", "\n").replace("\\\\", "\\");
        Constant::String(Ident::from_nonstatic(&unescaped))
    } else if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
        // Resource literal
        Constant::Resource(Ident::from_nonstatic(&s[1..s.len()-1]))
    } else if s.starts_with('/') {
        // Type path / prefab
        Constant::Prefab(Box::new(dreammaker::constants::Pop::from_path_str(s)))
    } else if s.starts_with("list(") && s.ends_with(')') {
        // List literal — simplified parser
        let inner = &s[5..s.len()-1];
        let items: Vec<(Constant, Option<Constant>)> = if inner.is_empty() {
            Vec::new()
        } else {
            // Simple split by comma (doesn't handle nested lists)
            inner.split(',')
                .map(|item| {
                    let item = item.trim();
                    if let Some(eq) = item.find('=') {
                        let key = parse_dm_value(item[..eq].trim());
                        let val = parse_dm_value(item[eq+1..].trim());
                        (key, Some(val))
                    } else {
                        (parse_dm_value(item), None)
                    }
                })
                .collect()
        };
        Constant::List(items.into_boxed_slice())
    } else if let Ok(f) = s.parse::<f32>() {
        Constant::Float(f)
    } else {
        // Unknown — treat as string
        Constant::String(Ident::from_nonstatic(s))
    }
}
