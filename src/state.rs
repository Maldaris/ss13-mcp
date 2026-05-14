//! Server state — holds the parsed environment, spatial index, and rule engine.

use std::path::{Path, PathBuf};
use anyhow::Result;
use dmm_tools::dmm::Map;

use crate::index::SpatialIndex;
use crate::rules::RuleEngine;

/// Loaded server state: the DM environment, map, spatial index, and rule engine.
pub struct ServerState {
    /// Path to the .dme file
    pub dme_path: PathBuf,

    /// Path to the loaded .dmm file
    pub dmm_path: PathBuf,

    /// The spatial index built from the map
    pub index: SpatialIndex,

    /// The rule engine (if rules directory exists)
    pub rule_engine: Option<RuleEngine>,
}

impl ServerState {
    /// Load a .dmm map file and build the spatial index.
    pub fn load(dme_path: &Path, dmm_path: &Path, rules_dir: Option<PathBuf>) -> Result<Self> {
        tracing::info!("Parsing map: {}", dmm_path.display());
        let map = Map::from_file(dmm_path)
            .map_err(|e| anyhow::anyhow!("Failed to parse map: {}", e))?;

        let (dim_x, dim_y, dim_z) = map.dim_xyz();
        tracing::info!("Map loaded: {}x{}x{}", dim_x, dim_y, dim_z);

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
            // Default: _maps/rules/ relative to the .dme file
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
            index,
            rule_engine,
        })
    }
}
