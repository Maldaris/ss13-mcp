//! Server state — holds the parsed environment, spatial index, rule engine, and renderer.

use std::path::{Path, PathBuf};
use anyhow::Result;
use dmm_tools::dmm::Map;
use dmm_tools::IconCache;
use dreammaker::config::MapRenderer;
use dreammaker::objtree::ObjectTree;

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

    /// The parsed map data
    pub map: Map,

    /// The icon cache for rendering (lazily loads .dmi files)
    pub icon_cache: IconCache,

    /// Map renderer configuration (extracted from .dme config)
    pub renderer_config: MapRenderer,

    /// The spatial index built from the map
    pub index: SpatialIndex,

    /// The rule engine (if rules directory exists)
    pub rule_engine: Option<RuleEngine>,
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
            map,
            icon_cache,
            renderer_config,
            index,
            rule_engine,
        })
    }
}
