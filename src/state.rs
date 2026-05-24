//! Server state — holds the parsed environment, spatial index, rule engine, and renderer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use anyhow::Result;
use dmm_tools::dmm::{Map, Prefab, Key};
use dmm_tools::IconCache;
use dreammaker::ast::Ident;
use dreammaker::config::MapRenderer;
use dreammaker::constants::Constant;
use dreammaker::objtree::ObjectTree;
use ndarray::Array3;
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

/// A single placement in a batch operation.
pub struct BatchPlacement {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub type_path: String,
    pub vars: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    pub replace: Option<String>,
}

/// Result of a single placement in a batch.
pub struct BatchResult {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub type_path: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// Mutable map data — the map grid + spatial index, protected by RwLock.
///
/// EDITING MODEL: We never mutate `map.dictionary` in place — that's read-only
/// after initial load. All edits go into `tile_overrides`, which maps raw grid
/// indices to fully-resolved prefab lists. On `save()`, we walk the entire grid,
/// resolve each tile (override or original), and build a fresh Map with a freshly-
/// deduped dictionary. This avoids any risk of corrupting unrelated grid cells
/// via shared dictionary keys.
pub struct MapData {
    /// The parsed map data (dictionary + grid as loaded — treated as immutable)
    pub map: Map,

    /// Per-tile overrides — fully resolved prefab list for tiles we've edited.
    /// Key is raw grid index (z, y, x). On read, override > dictionary lookup.
    pub tile_overrides: HashMap<(usize, usize, usize), Vec<Prefab>>,

    /// The spatial index built from the map
    pub index: SpatialIndex,

    /// Whether the map has unsaved changes
    pub dirty: bool,
}

impl MapData {
    /// Get the current prefab list at a raw grid index, considering overrides.
    fn prefabs_at_raw(&self, raw: (usize, usize, usize)) -> Vec<Prefab> {
        if let Some(over) = self.tile_overrides.get(&raw) {
            return over.clone();
        }
        let key = self.map.grid[raw];
        self.map.dictionary.get(&key).cloned().unwrap_or_default()
    }

    /// Place a prefab on a tile at (x, y, z).
    /// For turfs: replaces the existing turf (a tile has exactly one turf).
    /// For areas: replaces the existing area (a tile has exactly one area).
    /// For objs/mobs: adds to the tile's content list.
    pub fn place_prefab(&mut self, x: i32, y: i32, z: i32, prefab: Prefab) -> Result<(), String> {
        let raw = self.grid_index(x, y, z)?;

        // Resolve current prefab list (override > dictionary)
        let mut new_prefabs = self.prefabs_at_raw(raw);

        // If placing a turf or area, remove any existing one of the same layer.
        // A tile has exactly one turf and one area; objs/mobs may stack freely.
        let new_layer = layer_priority(&prefab.path);
        let path_kind = prefab.path.as_str();
        if path_kind.starts_with("/turf") || path_kind.starts_with("/area") {
            let removed: Vec<Prefab> = new_prefabs.iter()
                .filter(|p| layer_priority(&p.path) == new_layer)
                .cloned()
                .collect();
            new_prefabs.retain(|p| layer_priority(&p.path) != new_layer);
            for r in &removed {
                self.index.remove_object(x, y, z, r);
            }
        }

        // Insert at the right layer position
        let insert_pos = find_layer_position(&new_prefabs, &prefab.path);
        new_prefabs.insert(insert_pos, prefab.clone());

        // Store as override — the authoritative copy of this tile's contents.
        // We also update map.dictionary + map.grid for the renderer's benefit,
        // but only by APPENDING fresh keys — never modifying existing entries.
        self.tile_overrides.insert(raw, new_prefabs.clone());
        self.sync_tile_to_map(raw, new_prefabs);

        // Update the spatial index
        self.index.add_object(x, y, z, prefab);

        self.dirty = true;
        Ok(())
    }

    /// Mirror an override into `self.map` so the renderer sees current state.
    /// SAFETY: This only APPENDS new dictionary entries at max+1. It never
    /// modifies existing entries. The grid cell is updated to the new key.
    /// Orphaned dictionary entries are tolerated — they'll be cleaned up at save.
    fn sync_tile_to_map(&mut self, raw: (usize, usize, usize), prefabs: Vec<Prefab>) {
        let next_key = self.map.dictionary.keys()
            .max()
            .map(|k| k.next())
            .unwrap_or_else(Key::default);
        self.map.dictionary.insert(next_key, prefabs);
        self.map.grid[raw] = next_key;
        // adjust_key_length will be called on save; renderer doesn't need it
    }

    /// Place multiple prefabs in a single operation.
    /// Returns a summary of successes and failures.
    pub fn place_batch(&mut self, placements: Vec<BatchPlacement>, objtree: &ObjectTree) -> Vec<BatchResult> {
        let mut results = Vec::with_capacity(placements.len());
        for p in placements {
            // Handle replace first
            if let Some(ref replace_path) = p.replace {
                let _ = self.remove_prefab(p.x, p.y, p.z, replace_path);
            }

            // Build prefab with optional vars
            let prefab = if let Some(ref vars) = p.vars {
                build_prefab(&p.type_path, vars, objtree)
            } else {
                Prefab::from_path(p.type_path.clone())
            };

            match self.place_prefab(p.x, p.y, p.z, prefab) {
                Ok(()) => results.push(BatchResult {
                    x: p.x, y: p.y, z: p.z,
                    type_path: p.type_path,
                    ok: true,
                    error: None,
                }),
                Err(e) => results.push(BatchResult {
                    x: p.x, y: p.y, z: p.z,
                    type_path: p.type_path,
                    ok: false,
                    error: Some(e),
                }),
            }
        }
        results
    }

    /// Remove the first prefab matching a type path from a tile.
    /// Returns true if a prefab was removed.
    pub fn remove_prefab(&mut self, x: i32, y: i32, z: i32, type_path: &str) -> Result<bool, String> {
        let raw = self.grid_index(x, y, z)?;
        let mut prefabs = self.prefabs_at_raw(raw);

        // Find and remove the first matching prefab
        let pos = prefabs.iter().position(|p| p.path == type_path);
        if let Some(idx) = pos {
            let removed = prefabs.remove(idx);
            self.tile_overrides.insert(raw, prefabs.clone());
            self.sync_tile_to_map(raw, prefabs);
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
        let mut prefabs = self.prefabs_at_raw(raw);

        let pos = prefabs.iter().position(|p| p.path == old_path);
        if let Some(idx) = pos {
            let old = std::mem::replace(&mut prefabs[idx], new_prefab.clone());
            self.tile_overrides.insert(raw, prefabs.clone());
            self.sync_tile_to_map(raw, prefabs);
            self.index.remove_object(x, y, z, &old);
            self.index.add_object(x, y, z, new_prefab);
            self.dirty = true;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Save the map to a file.
    ///
    /// Strategy: rebuild the dictionary + grid from scratch by walking every
    /// tile in the original grid, resolving overrides, and deduping into a
    /// fresh dictionary keyed 0..N. This is the only safe way to write — any
    /// attempt to mutate `map.dictionary` in place risks corrupting unrelated
    /// tiles that share keys.
    pub fn save(&mut self, path: &Path) -> Result<(), String> {
        use std::collections::BTreeMap;

        let (dim_x, dim_y, dim_z) = self.map.dim_xyz();

        // Build a fresh dictionary: prefab list → new Key
        // Use a fingerprint (formatted string) for deduplication to avoid any
        // reliance on `PartialEq for Prefab` (which we suspect of misbehaving).
        let mut dedup: HashMap<String, Key> = HashMap::new();
        let mut new_dict: BTreeMap<Key, Vec<Prefab>> = BTreeMap::new();
        let mut keygen = KeyGen::new();

        let mut new_grid: Array3<Key> = Array3::default((dim_z, dim_y, dim_x));

        for z in 0..dim_z {
            for y in 0..dim_y {
                for x in 0..dim_x {
                    let raw = (z, y, x);
                    let prefabs: Vec<Prefab> = if let Some(over) = self.tile_overrides.get(&raw) {
                        over.clone()
                    } else {
                        let orig_key = self.map.grid[raw];
                        self.map.dictionary.get(&orig_key).cloned().unwrap_or_default()
                    };

                    let fingerprint = fingerprint_prefabs(&prefabs);
                    let key = if let Some(&k) = dedup.get(&fingerprint) {
                        k
                    } else {
                        let k = keygen.take();
                        dedup.insert(fingerprint, k);
                        new_dict.insert(k, prefabs);
                        k
                    };
                    new_grid[raw] = key;
                }
            }
        }

        // Swap the freshly-built dictionary + grid into self.map
        self.map.dictionary = new_dict;
        self.map.grid = new_grid;
        self.map.adjust_key_length();

        // Write to disk
        self.map.to_file(path).map_err(|e| format!("Failed to save map: {}", e))?;

        // After a successful save, the on-disk file is the source of truth.
        // The overrides have been folded into the dictionary, so clear them.
        self.tile_overrides.clear();
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

}

/// Build a stable, deterministic fingerprint for a prefab list. Used as a
/// dedup key during save — two tiles with the same fingerprint share a Key.
///
/// The fingerprint sorts var entries so that the same prefab written in
/// different var orders hashes identically.
fn fingerprint_prefabs(prefabs: &[Prefab]) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    for p in prefabs {
        s.push_str(&p.path);
        if !p.vars.is_empty() {
            let mut vars: Vec<_> = p.vars.iter().collect();
            vars.sort_by(|a, b| a.0.cmp(b.0));
            s.push('{');
            for (k, v) in vars {
                let _ = write!(s, "{}={};", k, v);
            }
            s.push('}');
        }
        s.push('\u{1F}');  // unit separator
    }
    s
}

/// Sequential key generator — yields Key(0), Key(1), Key(2), ...
struct KeyGen {
    next: Key,
}

impl KeyGen {
    fn new() -> Self {
        KeyGen { next: Key::default() }
    }
    fn take(&mut self) -> Key {
        let k = self.next;
        self.next = self.next.next();
        k
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
/// Find the insertion index for a new prefab so the resulting list stays in
/// BYOND DMM serialization order: objs/mobs first, then the turf, then the area
/// last. The parser walks `members` from the end (`members[len-1]` is the area,
/// `members[len-2]` is the turf), so getting this order wrong causes the area's
/// vars (e.g. `turfs_by_zlevel`) to be applied to whatever is at the tail of
/// the list, runtiming and leaving the trailing object partially-initialized.
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

/// Layer priority for `.dmm` serialization order. Lower number = earlier in
/// the tile's prefab list (closer to the front of the file's tuple). The
/// resulting order is `(/obj..., /mob..., /turf, /area)`, which is what
/// `code/modules/mapping/reader.dm` expects when it parses tiles back-to-front.
fn layer_priority(path: &str) -> u8 {
    if path.starts_with("/obj") { 0 }
    else if path.starts_with("/mob") { 1 }
    else if path.starts_with("/turf") { 2 }
    else if path.starts_with("/area") { 3 }
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
                tile_overrides: HashMap::new(),
                index,
                dirty: false,
            }),
            icon_cache,
            renderer_config,
            rule_engine,
        })
    }
}
