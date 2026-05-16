//! Server state — holds the parsed environment, spatial index, rule engine, and renderer.

use std::path::{Path, PathBuf};
use anyhow::Result;
use dmm_tools::dmm::{Map, Prefab, Key};
use dmm_tools::IconCache;
use dreammaker::ast::Ident;
use dreammaker::config::MapRenderer;
use dreammaker::constants::Constant;
use dreammaker::objtree::ObjectTree;
use tokio::sync::RwLock;

use crate::index::SpatialIndex;
use crate::rules::RuleEngine;

/// Loaded server state: the DM environment, map, spatial index, rule engine, and renderer.
pub struct ServerState {
    /// Path to the .dme file
    pub dme_path: PathBuf,

    /// Path to the loaded .dmm file
    pub dmm_path: PathBuf,

    /// The parsed object tree from the .dme environment
    pub objtree: ObjectTree,

    /// The map + spatial index (mutable for tile edits)
    pub map_data: RwLock<MapData>,

    /// The icon cache for rendering (lazily loads .dmi files)
    pub icon_cache: IconCache,

    /// Map renderer configuration (extracted from .dme config)
    pub renderer_config: MapRenderer,

    /// The rule engine (if rules directory exists)
    pub rule_engine: Option<RuleEngine>,
}

/// Mutable map data — the map grid + spatial index, protected by RwLock.
pub struct MapData {
    /// The parsed map data
    pub map: Map,

    /// The spatial index built from the map
    pub index: SpatialIndex,

    /// Whether the map has unsaved changes
    pub dirty: bool,
}

impl MapData {
    /// Place a prefab on a tile at (x, y, z).
    /// Adds the prefab to the tile's content list.
    /// Returns the new dictionary key for the tile.
    pub fn place_prefab(&mut self, x: i32, y: i32, z: i32, prefab: Prefab) -> Result<(), String> {
        let raw = self.grid_index(x, y, z)?;

        // Get current key at this position
        let current_key = self.map.grid[raw];

        // Get the current prefab list for this tile
        let current_prefabs = self.map.dictionary.get(&current_key)
            .cloned()
            .unwrap_or_default();

        // Build the new prefab list: insert the new prefab at the right layer position
        let mut new_prefabs = current_prefabs;
        let insert_pos = find_layer_position(&new_prefabs, &prefab.path);
        new_prefabs.insert(insert_pos, prefab.clone());

        // Find or create a dictionary key for this prefab list
        let new_key = self.find_or_create_key(new_prefabs);

        // Update the grid
        self.map.grid[raw] = new_key;

        // Update the spatial index
        self.index.add_object(x, y, z, prefab);

        self.dirty = true;
        Ok(())
    }

    /// Remove the first prefab matching a type path from a tile.
    /// Returns true if a prefab was removed.
    pub fn remove_prefab(&mut self, x: i32, y: i32, z: i32, type_path: &str) -> Result<bool, String> {
        let raw = self.grid_index(x, y, z)?;

        let current_key = self.map.grid[raw];
        let mut prefabs = self.map.dictionary.get(&current_key)
            .cloned()
            .unwrap_or_default();

        // Find and remove the first matching prefab
        let pos = prefabs.iter().position(|p| p.path == type_path);
        if let Some(idx) = pos {
            let removed = prefabs.remove(idx);
            let new_key = self.find_or_create_key(prefabs);
            self.map.grid[raw] = new_key;
            self.index.remove_object(x, y, z, &removed);
            self.dirty = true;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Replace the first prefab matching a type path with a new prefab on a tile.
    pub fn replace_prefab(&mut self, x: i32, y: i32, z: i32, old_path: &str, new_prefab: Prefab) -> Result<bool, String> {
        let raw = self.grid_index(x, y, z)?;

        let current_key = self.map.grid[raw];
        let mut prefabs = self.map.dictionary.get(&current_key)
            .cloned()
            .unwrap_or_default();

        let pos = prefabs.iter().position(|p| p.path == old_path);
        if let Some(idx) = pos {
            let old = std::mem::replace(&mut prefabs[idx], new_prefab.clone());
            let new_key = self.find_or_create_key(prefabs);
            self.map.grid[raw] = new_key;
            self.index.remove_object(x, y, z, &old);
            self.index.add_object(x, y, z, new_prefab);
            self.dirty = true;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Save the map to a file.
    pub fn save(&mut self, path: &Path) -> Result<(), String> {
        self.map.adjust_key_length();
        self.map.to_file(path).map_err(|e| format!("Failed to save map: {}", e))?;
        self.dirty = false;
        Ok(())
    }

    /// Convert 1-based (x, y, z) coordinates to a raw ndarray index.
    /// Grid is stored as (z, y, x) in ndarray with y-axis flipped.
    fn grid_index(&self, x: i32, y: i32, z: i32) -> Result<(usize, usize, usize), String> {
        let (dim_x, dim_y, dim_z) = self.map.dim_xyz();
        if x < 1 || x > dim_x as i32 || y < 1 || y > dim_y as i32 || z < 1 || z > dim_z as i32 {
            return Err(format!(
                "Coordinates ({},{},{}) out of bounds ({}x{}x{})",
                x, y, z, dim_x, dim_y, dim_z
            ));
        }
        Ok((z as usize - 1, dim_y - y as usize, x as usize - 1))
    }

    /// Find an existing dictionary key with the exact same prefab list,
    /// or create a new one.
    fn find_or_create_key(&mut self, prefabs: Vec<Prefab>) -> Key {
        // Search existing dictionary for a match
        for (&key, existing) in &self.map.dictionary {
            if *existing == prefabs {
                return key;
            }
        }

        // No match — create a new key
        let next_key = self.map.dictionary.keys()
            .max()
            .map(|k| k.next())
            .unwrap_or(Key::invalid()); // shouldn't happen

        self.map.dictionary.insert(next_key, prefabs);
        next_key
    }
}

/// Convert JSON var overrides into a DMM Prefab.
pub fn build_prefab(
    type_path: &str,
    vars: &std::collections::BTreeMap<String, serde_json::Value>,
    objtree: &ObjectTree,
) -> Prefab {
    let mut prefab = Prefab::from_path(type_path.to_string());

    // Only include vars that differ from defaults
    if let Some(type_ref) = crate::builder::find_type(objtree, type_path) {
        for (name, value) in vars {
            let var_val = type_ref.get_value(name);
            let default = var_val.and_then(|v| v.constant.as_ref());

            let constant = json_to_constant(value);

            // Only include if different from default
            let dominated = match (&constant, default) {
                (c, Some(d)) => format!("{}", c) == format!("{}", d),
                (Constant::Null(_), None) => true,
                _ => false,
            };

            if !dominated {
                prefab.vars.insert(
                    name.clone(),
                    constant,
                );
            }
        }
    } else {
        // Type not found in objtree — just set all vars
        for (name, value) in vars {
            prefab.vars.insert(
                name.clone(),
                json_to_constant(value),
            );
        }
    }

    prefab
}

/// Convert a serde_json::Value to a DM Constant.
pub fn json_to_constant(value: &serde_json::Value) -> Constant {
    match value {
        serde_json::Value::Null => Constant::Null(None),
        serde_json::Value::Bool(b) => Constant::Float(if *b { 1.0 } else { 0.0 }),
        serde_json::Value::Number(n) => {
            Constant::Float(n.as_f64().unwrap_or(0.0) as f32)
        }
        serde_json::Value::String(s) => {
            // Check if it looks like a type path
            if s.starts_with('/') {
                let pop = dreammaker::constants::Pop::from_path_str(s);
                Constant::Prefab(Box::new(pop))
            } else if s.starts_with('\'') && s.ends_with('\'') && s.len() > 2 {
                // Resource literal: 'icons/foo.dmi'
                Constant::Resource(Ident::from_nonstatic(&s[1..s.len()-1]))
            } else {
                Constant::String(Ident::from_nonstatic(s))
            }
        }
        serde_json::Value::Array(arr) => {
            // Convert to list(...)
            let items: Vec<(Constant, Option<Constant>)> = arr.iter()
                .map(|v| (json_to_constant(v), None))
                .collect();
            Constant::List(items.into_boxed_slice())
        }
        serde_json::Value::Object(obj) => {
            // Check for sub-datum pattern { "_type": "...", "_vars": {...} }
            if let Some(tp) = obj.get("_type").and_then(|v| v.as_str()) {
                let pop = dreammaker::constants::Pop::from_path_str(tp);
                // If there are vars, add them to the Pop
                // For now, serialize as a new() call or prefab reference
                Constant::Prefab(Box::new(pop))
            } else {
                // Treat as an assoc list: list("key" = val, ...)
                let items: Vec<(Constant, Option<Constant>)> = obj.iter()
                    .map(|(k, v)| {
                        (
                            Constant::String(Ident::from_nonstatic(k)),
                            Some(json_to_constant(v)),
                        )
                    })
                    .collect();
                Constant::List(items.into_boxed_slice())
            }
        }
    }
}

/// Determine the insertion position in a prefab list based on layer ordering.
/// BYOND layer order: area < turf < obj < mob
fn find_layer_position(prefabs: &[Prefab], path: &str) -> usize {
    let new_priority = layer_priority(path);
    for (i, p) in prefabs.iter().enumerate() {
        let existing_priority = layer_priority(&p.path);
        if new_priority < existing_priority {
            return i;
        }
    }
    prefabs.len()
}

/// Get layer priority for insertion ordering.
/// Lower number = earlier in the list (bottom of rendering stack).
fn layer_priority(path: &str) -> u8 {
    if path.starts_with("/area") { 0 }
    else if path.starts_with("/turf") { 1 }
    else if path.starts_with("/obj") { 2 }
    else if path.starts_with("/mob") { 3 }
    else { 4 }
}

impl ServerState {
    /// Load a .dme environment and .dmm map file, parse the object tree, and build the spatial index.
    pub fn load(dme_path: &Path, dmm_path: &Path, rules_dir: Option<PathBuf>) -> Result<Self> {
        // Parse the DM environment to get the object tree
        tracing::info!("Parsing environment: {}", dme_path.display());
        let mut dm_context = dreammaker::Context::default();
        dm_context.autodetect_config(dme_path);

        let pp = dreammaker::preprocessor::Preprocessor::new(&dm_context, dme_path.to_path_buf())
            .map_err(|e| anyhow::anyhow!("Failed to open environment: {}", e))?;
        let indents = dreammaker::indents::IndentProcessor::new(&dm_context, pp);
        let parser = dreammaker::parser::Parser::new(&dm_context, indents);
        let objtree = parser.parse_object_tree();

        // Extract renderer config before dropping dm_context
        let renderer_config = dm_context.config().map_renderer.clone();

        // Report any severe parse errors but don't fail — the renderer can work with partial trees
        let mut error_count = 0;
        let mut warning_count = 0;
        for error in dm_context.errors().iter() {
            if error.severity() <= dreammaker::Severity::Error {
                error_count += 1;
            } else {
                warning_count += 1;
            }
        }
        if error_count > 0 {
            tracing::warn!("Environment parsed with {} errors, {} warnings — render may be inaccurate", error_count, warning_count);
        } else {
            tracing::info!("Environment parsed successfully");
        }

        // Set up icon cache pointed at the codebase root
        let mut icon_cache = IconCache::default();
        if let Some(parent) = dme_path.parent() {
            icon_cache.set_icons_root(parent);
            tracing::info!("Icon cache root: {}", parent.display());
        }

        // Parse the map
        tracing::info!("Parsing map: {}", dmm_path.display());
        let map = Map::from_file(dmm_path)
            .map_err(|e| anyhow::anyhow!("Failed to parse map: {}", e))?;

        let (dim_x, dim_y, dim_z) = map.dim_xyz();
        tracing::info!("Map loaded: {}x{}x{}", dim_x, dim_y, dim_z);

        // Build spatial index
        tracing::info!("Building spatial index...");
        let index = SpatialIndex::build(&map);
        tracing::info!(
            "Index built: {} areas, dimensions {}x{}x{}",
            index.all_areas().len(),
            index.dim_x,
            index.dim_y,
            index.dim_z,
        );

        // Set up rule engine
        let rules_path = rules_dir.unwrap_or_else(|| {
            dme_path.parent().unwrap_or(Path::new(".")).join("_maps").join("rules")
        });

        let rule_engine = if rules_path.exists() {
            tracing::info!("Rules directory: {}", rules_path.display());
            Some(RuleEngine::new(rules_path))
        } else {
            tracing::info!("No rules directory at {} — rule validation disabled", rules_path.display());
            None
        };

        Ok(ServerState {
            dme_path: dme_path.to_path_buf(),
            dmm_path: dmm_path.to_path_buf(),
            objtree,
            map_data: RwLock::new(MapData {
                map,
                index,
                dirty: false,
            }),
            icon_cache,
            renderer_config,
            rule_engine,
        })
    }
}
