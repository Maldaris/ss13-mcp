//! Server state — holds the parsed environment and spatial index.

use std::path::{Path, PathBuf};
use anyhow::Result;
use dmm_tools::dmm::Map;

use crate::index::SpatialIndex;

/// Loaded server state: the DM environment, map, and spatial index.
pub struct ServerState {
    /// Path to the .dme file
    pub dme_path: PathBuf,

    /// Path to the loaded .dmm file
    pub dmm_path: PathBuf,

    /// The spatial index built from the map
    pub index: SpatialIndex,

    // TODO: Add ObjectTree once we wire up type info queries
    // pub objtree: dreammaker::objtree::ObjectTree,
}

impl ServerState {
    /// Load a .dmm map file and build the spatial index.
    ///
    /// For now, we skip the full .dme parse (which is slow on large codebases)
    /// and just load the map. Type info queries will be added once we integrate
    /// the dreammaker parser.
    pub fn load(dme_path: &Path, dmm_path: &Path) -> Result<Self> {
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

        Ok(ServerState {
            dme_path: dme_path.to_path_buf(),
            dmm_path: dmm_path.to_path_buf(),
            index,
        })
    }
}
