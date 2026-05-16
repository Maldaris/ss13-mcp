//! Spatial index over a parsed DMM map.
//!
//! Builds anchor indexes (type_path → positions), area indexes (area → positions),
//! and a grid index (position → prefabs) for fast rule evaluation and MCP queries.

use std::collections::HashMap;
use dmm_tools::dmm::{Map, Prefab};

/// A located prefab — an object instance at a specific position on the map.
#[derive(Debug, Clone)]
pub struct LocatedPrefab {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub prefab: Prefab,
}

/// The spatial index over a map. Provides O(1) lookups by position, type, and area.
pub struct SpatialIndex {
    /// Grid: (x, y, z) → list of prefabs at that position
    grid: HashMap<(i32, i32, i32), Vec<Prefab>>,

    /// Anchor index: type_path prefix → list of located prefabs
    /// Matches subtypes: querying "/obj/machinery/power" matches "/obj/machinery/power/apc"
    anchors: HashMap<String, Vec<LocatedPrefab>>,

    /// Area index: area_path → list of (x, y, z) positions in that area
    areas: HashMap<String, Vec<(i32, i32, i32)>>,

    /// Reverse area: (x, y, z) → area_path
    tile_area: HashMap<(i32, i32, i32), String>,

    /// Map dimensions
    pub dim_x: usize,
    pub dim_y: usize,
    pub dim_z: usize,
}

impl SpatialIndex {
    /// Build a spatial index from a parsed map.
    ///
    /// Single pass over the map: O(tiles × objects_per_tile).
    pub fn build(map: &Map) -> Self {
        let (dim_x, dim_y, dim_z) = map.dim_xyz();
        let estimated_tiles = dim_x * dim_y * dim_z;

        let mut grid: HashMap<(i32, i32, i32), Vec<Prefab>> =
            HashMap::with_capacity(estimated_tiles);
        let mut anchors: HashMap<String, Vec<LocatedPrefab>> = HashMap::new();
        let mut areas: HashMap<String, Vec<(i32, i32, i32)>> = HashMap::new();
        let mut tile_area: HashMap<(i32, i32, i32), String> = HashMap::with_capacity(estimated_tiles);

        // Single pass: iterate every tile on every z-level
        for (z_idx, z_level) in map.iter_levels() {
            for (coord2, key) in z_level.iter_top_down() {
                let pos = (coord2.x, coord2.y, z_idx);

                if let Some(prefabs) = map.dictionary.get(&key) {
                    let mut tile_objects = Vec::with_capacity(prefabs.len());

                    for prefab in prefabs {
                        // Index by type path (every ancestor prefix)
                        let located = LocatedPrefab {
                            x: pos.0,
                            y: pos.1,
                            z: pos.2,
                            prefab: prefab.clone(),
                        };

                        // Add to anchors under the exact path
                        anchors
                            .entry(prefab.path.clone())
                            .or_default()
                            .push(located.clone());

                        // Also index under each parent path prefix for subtype matching
                        // e.g. "/obj/machinery/power/apc" also indexed under
                        // "/obj/machinery/power", "/obj/machinery", "/obj"
                        for (i, ch) in prefab.path.char_indices() {
                            if ch == '/' && i > 0 {
                                let prefix = &prefab.path[..i];
                                anchors
                                    .entry(prefix.to_string())
                                    .or_default()
                                    .push(located.clone());
                            }
                        }

                        // Track areas
                        if prefab.path.starts_with("/area/") {
                            areas.entry(prefab.path.clone()).or_default().push(pos);
                            tile_area.insert(pos, prefab.path.clone());
                        }

                        tile_objects.push(prefab.clone());
                    }

                    grid.insert(pos, tile_objects);
                }
            }
        }

        SpatialIndex {
            grid,
            anchors,
            areas,
            tile_area,
            dim_x,
            dim_y,
            dim_z,
        }
    }

    /// Get all objects at a specific position.
    pub fn at(&self, x: i32, y: i32, z: i32) -> &[Prefab] {
        self.grid.get(&(x, y, z)).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Get all instances of a type (including subtypes) on the map.
    pub fn instances_of(&self, type_path: &str) -> &[LocatedPrefab] {
        self.anchors.get(type_path).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Get all positions belonging to an area.
    pub fn area_tiles(&self, area_path: &str) -> &[(i32, i32, i32)] {
        self.areas.get(area_path).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Get the area path for a tile.
    pub fn area_of(&self, x: i32, y: i32, z: i32) -> Option<&str> {
        self.tile_area.get(&(x, y, z)).map(|s| s.as_str())
    }

    /// Get all objects on tiles adjacent to (x, y, z) — 4-directional.
    pub fn adjacent(&self, x: i32, y: i32, z: i32) -> Vec<LocatedPrefab> {
        let dirs = [(0, -1), (0, 1), (-1, 0), (1, 0)];
        let mut results = Vec::new();
        for (dx, dy) in dirs {
            let nx = x + dx;
            let ny = y + dy;
            if let Some(prefabs) = self.grid.get(&(nx, ny, z)) {
                for prefab in prefabs {
                    results.push(LocatedPrefab {
                        x: nx,
                        y: ny,
                        z,
                        prefab: prefab.clone(),
                    });
                }
            }
        }
        results
    }

    /// Get all objects within `radius` tiles of (x, y, z).
    pub fn nearby(&self, x: i32, y: i32, z: i32, radius: i32) -> Vec<LocatedPrefab> {
        let mut results = Vec::new();
        for dx in -radius..=radius {
            for dy in -radius..=radius {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let nx = x + dx;
                let ny = y + dy;
                if let Some(prefabs) = self.grid.get(&(nx, ny, z)) {
                    for prefab in prefabs {
                        results.push(LocatedPrefab {
                            x: nx,
                            y: ny,
                            z,
                            prefab: prefab.clone(),
                        });
                    }
                }
            }
        }
        results
    }

    /// Get all objects in a given area.
    pub fn objects_in_area(&self, area_path: &str) -> Vec<LocatedPrefab> {
        let mut results = Vec::new();
        if let Some(positions) = self.areas.get(area_path) {
            for &(x, y, z) in positions {
                if let Some(prefabs) = self.grid.get(&(x, y, z)) {
                    for prefab in prefabs {
                        results.push(LocatedPrefab {
                            x, y, z,
                            prefab: prefab.clone(),
                        });
                    }
                }
            }
        }
        results
    }

    /// List all area paths on the map.
    pub fn all_areas(&self) -> Vec<&str> {
        self.areas.keys().map(|s| s.as_str()).collect()
    }

    // ── Mutation support ─────────────────────────────────────────────

    /// Add an object to the spatial index at a given position.
    pub fn add_object(&mut self, x: i32, y: i32, z: i32, prefab: Prefab) {
        let pos = (x, y, z);

        // Update grid
        self.grid.entry(pos).or_default().push(prefab.clone());

        // Update anchors
        let located = LocatedPrefab { x, y, z, prefab: prefab.clone() };

        self.anchors
            .entry(prefab.path.clone())
            .or_default()
            .push(located.clone());

        for (i, ch) in prefab.path.char_indices() {
            if ch == '/' && i > 0 {
                let prefix = &prefab.path[..i];
                self.anchors
                    .entry(prefix.to_string())
                    .or_default()
                    .push(located.clone());
            }
        }

        // Update area index if it's an area
        if prefab.path.starts_with("/area/") {
            self.areas.entry(prefab.path.clone()).or_default().push(pos);
            self.tile_area.insert(pos, prefab.path.clone());
        }
    }

    /// Remove an object from the spatial index at a given position.
    /// Removes the first prefab matching the exact path and vars.
    pub fn remove_object(&mut self, x: i32, y: i32, z: i32, prefab: &Prefab) {
        let pos = (x, y, z);

        // Remove from grid
        if let Some(list) = self.grid.get_mut(&pos) {
            if let Some(idx) = list.iter().position(|p| p == prefab) {
                list.remove(idx);
            }
        }

        // Remove from anchors (exact path)
        if let Some(list) = self.anchors.get_mut(&prefab.path) {
            if let Some(idx) = list.iter().position(|lp| lp.x == x && lp.y == y && lp.z == z && lp.prefab == *prefab) {
                list.remove(idx);
            }
        }

        // Remove from prefix anchors
        for (i, ch) in prefab.path.char_indices() {
            if ch == '/' && i > 0 {
                let prefix = &prefab.path[..i];
                if let Some(list) = self.anchors.get_mut(prefix) {
                    if let Some(idx) = list.iter().position(|lp| lp.x == x && lp.y == y && lp.z == z && lp.prefab == *prefab) {
                        list.remove(idx);
                    }
                }
            }
        }

        // Remove from area index
        if prefab.path.starts_with("/area/") {
            if let Some(list) = self.areas.get_mut(&prefab.path) {
                if let Some(idx) = list.iter().position(|p| *p == pos) {
                    list.remove(idx);
                }
            }
            if self.tile_area.get(&pos).map(|a| a == &prefab.path).unwrap_or(false) {
                self.tile_area.remove(&pos);
            }
        }
    }

    /// Trace a cable/pipe network via BFS from a starting position.
    /// Returns all positions connected by objects matching `network_type`.
    pub fn trace_network(&self, start_x: i32, start_y: i32, start_z: i32, network_type: &str) -> Vec<(i32, i32, i32)> {
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();

        let start = (start_x, start_y, start_z);

        // Check if start has the network type
        if self.at(start_x, start_y, start_z).iter().any(|p| p.path.starts_with(network_type)) {
            queue.push_back(start);
            visited.insert(start);
        }

        let dirs = [(0i32, -1i32), (0, 1), (-1, 0), (1, 0)];

        while let Some((x, y, z)) = queue.pop_front() {
            for (dx, dy) in dirs {
                let next = (x + dx, y + dy, z);
                if visited.contains(&next) {
                    continue;
                }
                if self.at(next.0, next.1, next.2).iter().any(|p| p.path.starts_with(network_type)) {
                    visited.insert(next);
                    queue.push_back(next);
                }
            }
        }

        visited.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    // Tests will use a small hand-built map
}
