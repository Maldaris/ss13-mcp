//! Server state — holds the parsed environment, spatial index, rule engine, and renderer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use anyhow::Result;
use dmm_tools::dmm::{allocate_unused_key, save_preserve_format, Map, MapFormatSnapshot, Prefab, Key};
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

    /// File this map was read from, and the default target for `save()`.
    /// Lives here rather than on `ServerState` because the active map can be
    /// swapped at runtime, and the path has to travel with it.
    pub path: PathBuf,

    /// Per-tile overrides — fully resolved prefab list for tiles we've edited.
    /// Key is raw grid index (z, y, x). On read, override > dictionary lookup.
    pub tile_overrides: HashMap<(usize, usize, usize), Vec<Prefab>>,

    /// The spatial index built from the map
    pub index: SpatialIndex,

    /// Whether the map has unsaved changes
    pub dirty: bool,

    /// On-disk layout captured at load time for format-preserving saves.
    /// Absent for maps created from scratch via `create_map`.
    pub format_snapshot: Option<MapFormatSnapshot>,
}

impl MapData {
    /// Make a different map the one every tool operates on.
    ///
    /// Staged edits belong to the map they were written against, so they are
    /// dropped rather than carried over; callers are expected to have saved or
    /// consciously abandoned them first.
    pub fn activate(&mut self, map: Map, path: PathBuf) {
        self.index = SpatialIndex::build(&map);
        self.map = map;
        self.tile_overrides.clear();
        self.path = path.clone();
        self.format_snapshot = MapFormatSnapshot::from_file(&path, &self.map).ok();
        self.dirty = false;
    }

    /// Activate a freshly created map that has no on-disk snapshot yet.
    pub fn activate_new(&mut self, map: Map, path: PathBuf) {
        self.index = SpatialIndex::build(&map);
        self.map = map;
        self.tile_overrides.clear();
        self.path = path;
        self.format_snapshot = None;
        self.dirty = false;
    }

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

    /// Replace the turf on a tile at (x, y, z).
    pub fn set_turf(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        type_path: &str,
        vars: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
        objtree: &ObjectTree,
    ) -> Result<(), String> {
        if !type_path.starts_with("/turf/") {
            return Err(format!("'{}' is not a turf path (must start with /turf/)", type_path));
        }
        let prefab = if let Some(vars) = vars {
            build_prefab(type_path, vars, objtree)
        } else {
            Prefab::from_path(type_path.to_string())
        };
        self.place_prefab(x, y, z, prefab)
    }

    /// Mirror an override into `self.map` so the renderer sees current state.
    /// Reuses an existing dictionary key when content matches; otherwise appends
    /// a fresh unused key without touching unrelated entries.
    fn sync_tile_to_map(&mut self, raw: (usize, usize, usize), prefabs: Vec<Prefab>) {
        let fingerprint = fingerprint_prefabs(&prefabs);
        for (&key, existing) in &self.map.dictionary {
            if fingerprint_prefabs(existing) == fingerprint {
                self.map.grid[raw] = key;
                return;
            }
        }

        let next_key = allocate_unused_key(&self.map, &Default::default());
        self.map.dictionary.insert(next_key, prefabs);
        self.map.grid[raw] = next_key;
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
    /// When a format snapshot is available, unchanged content is written back
    /// verbatim. Pass `compact: true` to rebuild and dedupe the dictionary.
    pub fn save(&mut self, path: &Path, compact: bool) -> Result<(), String> {
        if compact {
            return self.save_compact(path);
        }

        if let Some(snapshot) = &self.format_snapshot {
            if !self.dirty {
                std::fs::write(path, &snapshot.original_bytes)
                    .map_err(|e| format!("Failed to save map: {}", e))?;
                return Ok(());
            }

            save_preserve_format(&self.map, snapshot, path, false)
                .map_err(|e| format!("Failed to save map: {}", e))?;
        } else {
            self.map
                .to_file(path)
                .map_err(|e| format!("Failed to save map: {}", e))?;
        }

        self.drop_unreferenced_entries();
        self.tile_overrides.clear();
        self.format_snapshot = MapFormatSnapshot::from_file(path, &self.map).ok();
        self.dirty = false;
        Ok(())
    }

    /// Forget dictionary entries no tile names any more.
    ///
    /// Edits leave these behind constantly — every intermediate state of a
    /// multi-step change mints one — and the file we just wrote does not contain
    /// them. Keeping them in memory would diverge from disk and, worse, hold
    /// their keys hostage: the allocator treats a key with an entry as taken, so
    /// a long session on a two-character-key map would march toward running out.
    fn drop_unreferenced_entries(&mut self) {
        let referenced: std::collections::BTreeSet<Key> = self.map.grid.iter().copied().collect();
        self.map.dictionary.retain(|key, _| referenced.contains(key));
    }

    /// Rebuild the dictionary from scratch and write in canonical TGM form.
    fn save_compact(&mut self, path: &Path) -> Result<(), String> {
        use std::collections::BTreeMap;

        let (dim_x, dim_y, dim_z) = self.map.dim_xyz();

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

        self.map.dictionary = new_dict;
        self.map.grid = new_grid;
        self.map.adjust_key_length();

        self.map
            .to_file(path)
            .map_err(|e| format!("Failed to save map: {}", e))?;

        self.tile_overrides.clear();
        self.format_snapshot = MapFormatSnapshot::from_file(path, &self.map).ok();
        self.dirty = false;
        Ok(())
    }

    /// Convert 1-based BYOND (x, y, z) coordinates to a raw ndarray index.
    ///
    /// The grid is stored (z, y, x) with the y axis flipped: ndarray y=0 is the
    /// first key line of each column block, which is the *highest* BYOND y. This
    /// has to stay in step with `Coord2::to_raw`, which the read path uses via
    /// `SpatialIndex`, or edits land on the mirrored row.
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

        let format_snapshot = MapFormatSnapshot::from_file(dmm_path, &map).ok();

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
            objtree,
            map_data: RwLock::new(MapData {
                map,
                path: dmm_path.to_path_buf(),
                tile_overrides: HashMap::new(),
                index,
                dirty: false,
                format_snapshot,
            }),
            icon_cache,
            renderer_config,
            rule_engine,
        })
    }
}

#[cfg(test)]
mod preserve_tests {
    use super::*;

    /// A hand-written 2x2 TGM map. Small enough to reason about by eye, and
    /// self-contained so the test runs everywhere rather than skipping.
    ///
    /// Column blocks list keys from the highest BYOND y downwards, so the table
    /// on the first line of the `(2,1,1)` block sits at BYOND (2,2,1).
    const TINY_TGM: &str = concat!(
        "//MAP CONVERTED BY dmm2tgm.py THIS HEADER COMMENT PREVENTS RECONVERSION, DO NOT REMOVE\n",
        "\"aa\" = (\n",
        "/turf/open/floor/plating,\n",
        "/area/space)\n",
        "\"ab\" = (\n",
        "/obj/structure/table,\n",
        "/turf/open/floor/plating,\n",
        "/area/space)\n",
        "\n",
        "(1,1,1) = {\"\n",
        "aa\n",
        "aa\n",
        "\"}\n",
        "(2,1,1) = {\"\n",
        "ab\n",
        "aa\n",
        "\"}\n",
    );

    fn tiny_map(dir: &Path) -> MapData {
        let path = dir.join("tiny.dmm");
        std::fs::write(&path, TINY_TGM).expect("write fixture");
        let map = Map::from_file(&path).expect("parse fixture");
        MapData {
            index: SpatialIndex::build(&map),
            map,
            path,
            tile_overrides: HashMap::new(),
            dirty: false,
            format_snapshot: None,
        }
    }

    fn load(path: &Path) -> MapData {
        let map = Map::from_file(path).expect("parse map");
        let format_snapshot = MapFormatSnapshot::from_file(path, &map).ok();
        MapData {
            index: SpatialIndex::build(&map),
            map,
            path: path.to_path_buf(),
            tile_overrides: HashMap::new(),
            dirty: false,
            format_snapshot,
        }
    }

    fn paths_at(data: &MapData, x: i32, y: i32, z: i32) -> Vec<String> {
        let (_, dim_y, _) = data.map.dim_xyz();
        let raw = (z as usize - 1, dim_y - y as usize, x as usize - 1);
        data.map
            .dictionary
            .get(&data.map.grid[raw])
            .map(|prefabs| prefabs.iter().map(|p| p.path.clone()).collect())
            .unwrap_or_default()
    }

    /// An edit saved in preserve-layout mode has to actually take effect.
    ///
    /// The failure this guards against is silent and total: the new dictionary
    /// entry gets appended, so the file grows and looks edited, but the grid
    /// still names the old key. Nothing reports an error, the map reloads
    /// cleanly, and the tile is simply unchanged with an orphan entry alongside.
    #[test]
    fn preserving_save_repoints_the_grid() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut data = tiny_map(temp.path());
        data.format_snapshot = MapFormatSnapshot::from_file(&data.path.clone(), &data.map).ok();

        data.place_prefab(2, 2, 1, Prefab::from_path("/obj/structure/chair"))
            .expect("place");

        let out = temp.path().join("saved.dmm");
        data.save(&out, false).expect("save");

        let reloaded = load(&out);
        let paths = paths_at(&reloaded, 2, 2, 1);
        assert!(
            paths.iter().any(|p| p == "/obj/structure/chair"),
            "tile (2,2,1) came back as {paths:?} — the grid still points at the pre-edit key, \
             so the appended dictionary entry is an orphan"
        );
    }

    /// Swapping one object for another on a tile that is the sole user of its key.
    ///
    /// This is the shape every `replace` takes, and it used to lose the edit
    /// outright. The removal step repoints the tile at a fresh key, which leaves
    /// the original key referenced by nothing; the placement step then asks for
    /// an unused key and is handed that same original back, because the
    /// allocator only looked at the grid. Inserting under it overwrote a live
    /// dictionary entry, and since the tile's key had come full circle, the
    /// preserving save saw an unchanged grid and wrote none of it.
    #[test]
    fn replacing_an_object_survives_a_preserving_save() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut data = tiny_map(temp.path());
        data.format_snapshot = MapFormatSnapshot::from_file(&data.path.clone(), &data.map).ok();

        // (2,2,1) holds the only table on the map, so its key "ab" belongs to it alone.
        assert!(data.remove_prefab(2, 2, 1, "/obj/structure/table").expect("remove"));
        data.place_prefab(2, 2, 1, Prefab::from_path("/obj/structure/chair"))
            .expect("place");

        let out = temp.path().join("saved.dmm");
        data.save(&out, false).expect("save");

        let reloaded = load(&out);
        let paths = paths_at(&reloaded, 2, 2, 1);
        assert_eq!(
            paths,
            vec![
                "/obj/structure/chair".to_string(),
                "/turf/open/floor/plating".to_string(),
                "/area/space".to_string(),
            ],
            "tile (2,2,1) came back as {paths:?}"
        );
    }

    /// A key freed up by an edit must not be handed back out while its entry lives.
    #[test]
    fn allocating_a_key_skips_orphaned_dictionary_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut data = tiny_map(temp.path());

        let table_key = data.map.grid[(0, 0, 1)];
        assert!(data.remove_prefab(2, 2, 1, "/obj/structure/table").expect("remove"));
        assert!(
            !data.map.grid.iter().any(|&k| k == table_key),
            "the fixture only works if the removal leaves the table's key unreferenced"
        );

        let before = data.map.dictionary.get(&table_key).cloned();
        data.place_prefab(2, 2, 1, Prefab::from_path("/obj/structure/chair"))
            .expect("place");

        assert_eq!(
            data.map.dictionary.get(&table_key),
            before.as_ref(),
            "the placement reused the orphaned key and overwrote its entry"
        );
    }

    /// A saved file must name every entry it carries, and carry every entry it names.
    ///
    /// Two ways to break that, and a replace hits both at once. It mints a key
    /// for the state between the removal and the placement, which is dead by the
    /// time the save runs and must never be written. And it moves the tile off
    /// the key it arrived on, stranding an entry that was already in the file,
    /// which has to be taken back out or the swap reads as an unexplained
    /// addition sitting next to an unexplained leftover.
    #[test]
    fn preserving_save_writes_no_orphan_entries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut data = tiny_map(temp.path());
        data.format_snapshot = MapFormatSnapshot::from_file(&data.path.clone(), &data.map).ok();

        assert!(data.remove_prefab(2, 2, 1, "/obj/structure/table").expect("remove"));
        data.place_prefab(2, 2, 1, Prefab::from_path("/obj/structure/chair"))
            .expect("place");

        let out = temp.path().join("saved.dmm");
        data.save(&out, false).expect("save");

        let reloaded = Map::from_file(&out).expect("reparse saved");
        let referenced: std::collections::BTreeSet<_> = reloaded.grid.iter().copied().collect();
        let orphans: Vec<String> = reloaded
            .dictionary
            .keys()
            .filter(|key| !referenced.contains(key))
            .map(|key| format!("{}", reloaded.format_key(*key)))
            .collect();
        assert!(
            orphans.is_empty(),
            "saved file carries dictionary entries no tile references: {orphans:?}"
        );

        assert!(
            reloaded.dictionary.len() < 3,
            "the table's entry should be gone, leaving only the plating tile and the chair tile"
        );
    }

    /// Pruning is scoped to the edit, not a general tidy-up of the file.
    ///
    /// A map that arrives with dead keys keeps them. They are not this save's
    /// doing, and rewriting parts of the file nobody asked about is the one
    /// thing preserving mode exists to avoid.
    #[test]
    fn preserving_save_keeps_dead_keys_it_did_not_create() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("dead.dmm");
        // "ac" is in the dictionary but absent from every column block.
        std::fs::write(
            &path,
            concat!(
                "//MAP CONVERTED BY dmm2tgm.py THIS HEADER COMMENT PREVENTS RECONVERSION, DO NOT REMOVE\n",
                "\"aa\" = (\n/turf/open/floor/plating,\n/area/space)\n",
                "\"ab\" = (\n/obj/structure/table,\n/turf/open/floor/plating,\n/area/space)\n",
                "\"ac\" = (\n/obj/structure/rack,\n/turf/open/floor/plating,\n/area/space)\n",
                "\n(1,1,1) = {\"\naa\naa\n\"}\n(2,1,1) = {\"\nab\naa\n\"}\n",
            ),
        )
        .expect("write fixture");

        let mut data = load(&path);
        data.place_prefab(1, 1, 1, Prefab::from_path("/obj/structure/chair"))
            .expect("place");

        let out = temp.path().join("saved.dmm");
        data.save(&out, false).expect("save");

        let saved = std::fs::read_to_string(&out).expect("read saved");
        assert!(
            saved.contains("\"ac\" = ("),
            "a dead key the edit did not strand was removed anyway"
        );
    }

    /// Every tile the edit did not touch must come back untouched.
    #[test]
    fn preserving_save_leaves_other_tiles_alone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut data = tiny_map(temp.path());
        data.format_snapshot = MapFormatSnapshot::from_file(&data.path.clone(), &data.map).ok();

        data.place_prefab(2, 2, 1, Prefab::from_path("/obj/structure/chair"))
            .expect("place");

        let out = temp.path().join("saved.dmm");
        data.save(&out, false).expect("save");

        let reloaded = load(&out);
        for (x, y) in [(1, 1), (1, 2), (2, 1)] {
            let paths = paths_at(&reloaded, x, y, 1);
            assert_eq!(
                paths,
                vec!["/turf/open/floor/plating".to_string(), "/area/space".to_string()],
                "untouched tile ({x},{y},1) came back as {paths:?}"
            );
        }
    }
}

#[cfg(test)]
mod coordinate_tests {
    use super::*;

    fn fixture() -> PathBuf {
        PathBuf::from(
            r"C:\Users\robot\dev\Blastwave-content\_maps\BlastwaveRuins\interdiction_array.dmm",
        )
    }

    fn load(path: &Path) -> MapData {
        let map = Map::from_file(path).expect("open fixture");
        MapData {
            index: SpatialIndex::build(&map),
            map,
            path: path.to_path_buf(),
            tile_overrides: HashMap::new(),
            dirty: false,
            format_snapshot: None,
        }
    }

    /// Writing a tile and reading it back must agree on where the tile is.
    ///
    /// `place_prefab` goes through `grid_index`, while every query goes through
    /// `SpatialIndex`/`Coord2::from_raw`. If those two disagree about the Y axis,
    /// edits silently land on the mirrored row. Deliberately tested well away from
    /// the middle row, where a flip is only off by one and easy to miss.
    #[test]
    fn write_and_read_agree_on_y() {
        let src = fixture();
        if !src.is_file() {
            eprintln!("Skipping: fixture not found at {}", src.display());
            return;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        let copy = temp.path().join("map.dmm");
        std::fs::copy(&src, &copy).expect("copy fixture");

        let mut data = load(&copy);
        let dim_y = data.map.dim_xyz().1 as i32;

        // Asymmetric on purpose: y=6 mirrors to y=55 on this 60-tall map.
        let (x, y, z) = (5, 6, 1);
        assert_ne!(y, dim_y - y + 1, "test coordinate must not sit on the mirror line");

        let marker = Prefab::from_path("/obj/effect/landmark/start");
        data.place_prefab(x, y, z, marker.clone()).expect("place");

        let out = temp.path().join("saved.dmm");
        data.save(&out, false).expect("save");

        let reloaded = load(&out);
        let found = reloaded.index.instances_of("/obj/effect/landmark/start");
        assert_eq!(found.len(), 1, "expected exactly one marker after save/reload");
        assert_eq!(
            (found[0].x, found[0].y, found[0].z),
            (x, y, z),
            "marker placed at ({x},{y},{z}) came back at ({},{},{}) - write and read \
             disagree about the Y axis",
            found[0].x, found[0].y, found[0].z
        );
    }
}
