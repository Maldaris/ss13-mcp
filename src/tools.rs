//! MCP tool definitions for SS13 map intelligence.
//!
//! Each tool is a query against the spatial index + object tree.

use std::sync::Arc;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServerHandler, schemars, tool, tool_router, tool_handler};
use rmcp::model::ServerInfo;
use serde::Deserialize;

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

// ── Tool implementations ─────────────────────────────────────────────

/// The MCP tool handler. Holds a reference to the loaded map state and the tool router.
pub struct MapTools {
    pub state: Arc<ServerState>,
    tool_router: ToolRouter<Self>,
}

impl MapTools {
    pub fn new(state: Arc<ServerState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl MapTools {
    /// Query all objects at a specific tile coordinate.
    #[tool(description = "Get all objects at a specific (x, y, z) tile on the map")]
    fn query_tile(&self, Parameters(params): Parameters<QueryTileParams>) -> String {
        let z = params.z.unwrap_or(1);
        let objects = self.state.index.at(params.x, params.y, z);

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
    fn list_areas(&self, Parameters(params): Parameters<ListAreasParams>) -> String {
        let mut areas = self.state.index.all_areas();
        if let Some(prefix) = &params.prefix {
            areas.retain(|a| a.starts_with(prefix.as_str()));
        }
        areas.sort();

        let mut result = format!("{} areas found:\n", areas.len());
        for area in areas {
            let tile_count = self.state.index.area_tiles(area).len();
            result.push_str(&format!("  {} ({} tiles)\n", area, tile_count));
        }
        result
    }

    /// Get all objects within an area.
    #[tool(description = "Get all objects in a specific area, grouped by type")]
    fn query_area(&self, Parameters(params): Parameters<QueryAreaParams>) -> String {
        let objects = self.state.index.objects_in_area(&params.area_path);

        if objects.is_empty() {
            return format!("No objects in area '{}' (area may not exist)", params.area_path);
        }

        // Group by type path for readability
        let mut type_counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for obj in &objects {
            *type_counts.entry(&obj.prefab.path).or_default() += 1;
        }

        let tiles = self.state.index.area_tiles(&params.area_path);
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
    fn query_object_type(&self, Parameters(params): Parameters<QueryTypeParams>) -> String {
        let limit = params.limit.unwrap_or(50);
        let instances = self.state.index.instances_of(&params.type_path);

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
    fn query_adjacent(&self, Parameters(params): Parameters<QueryAdjacentParams>) -> String {
        let z = params.z.unwrap_or(1);

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
            let tile_objects = self.state.index.at(nx, ny, z);
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
    fn query_nearby(&self, Parameters(params): Parameters<QueryNearbyParams>) -> String {
        let z = params.z.unwrap_or(1);
        let radius = params.radius.unwrap_or(3);
        let nearby = self.state.index.nearby(params.x, params.y, z, radius);

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
    fn trace_network(&self, Parameters(params): Parameters<TraceNetworkParams>) -> String {
        let z = params.z.unwrap_or(1);
        let network = self.state.index.trace_network(params.x, params.y, z, &params.network_type);

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
            if let Some(area) = self.state.index.area_of(nx, ny, nz) {
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
    fn search_objects(&self, Parameters(params): Parameters<SearchObjectsParams>) -> String {
        let limit = params.limit.unwrap_or(20);
        let query_lower = params.query.to_lowercase();

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        let (dx, dy, dz) = (self.state.index.dim_x, self.state.index.dim_y, self.state.index.dim_z);
        for z in 1..=(dz as i32) {
            for y in 1..=(dy as i32) {
                for x in 1..=(dx as i32) {
                    for prefab in self.state.index.at(x, y, z) {
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
            let count = self.state.index.instances_of(path).len();
            result.push_str(&format!("  {} (×{})\n", path, count));
        }
        result
    }

    /// Run all JS validation rules against the loaded map.
    #[tool(description = "Run JavaScript validation rules against the loaded map. Returns violations grouped by severity.")]
    fn validate_rules(&self, Parameters(params): Parameters<ValidateRulesParams>) -> String {
        let engine = match &self.state.rule_engine {
            Some(e) => e,
            None => return "No rules directory configured. Use --rules <dir> to specify a rules directory.".into(),
        };

        let result = match engine.evaluate(&self.state.index) {
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
}

#[tool_handler]
impl ServerHandler for MapTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("SS13 Map Intelligence — query tiles, areas, objects, networks, and validate maps. Load a .dme + .dmm to get started.".into()),
            ..ServerInfo::default()
        }
    }
}
