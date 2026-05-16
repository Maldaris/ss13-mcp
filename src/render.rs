//! Map rendering — generates PNG images of map regions using SpacemanDMM's renderer.
//!
//! Wraps the dmm-tools minimap generator with region selection, render pass
//! configuration, and PNG/base64 output suitable for MCP tool responses.

use std::sync::RwLock;
use anyhow::Result;
use base64::Engine;
use dmm_tools::minimap;
use dmm_tools::render_passes;
use foldhash::HashSet;

use crate::state::ServerState;

/// Render a rectangular region of the map to a PNG image.
///
/// Coordinates are 1-based (matching BYOND's coordinate system).
/// Returns the PNG as base64-encoded bytes.
pub fn render_region(
    state: &ServerState,
    x1: usize,
    y1: usize,
    x2: usize,
    y2: usize,
    z: usize,
    render_pass_filter: Option<&str>,
) -> Result<RenderResult> {
    let (dim_x, dim_y, dim_z) = state.map.dim_xyz();

    // Validate bounds
    if z < 1 || z > dim_z {
        anyhow::bail!("Z level {} out of range (1-{})", z, dim_z);
    }
    let x1 = x1.max(1).min(dim_x);
    let y1 = y1.max(1).min(dim_y);
    let x2 = x2.max(x1).min(dim_x);
    let y2 = y2.max(y1).min(dim_y);

    // Configure render passes
    let renderer_config = &state.renderer_config;
    let render_passes = match render_pass_filter {
        Some("pipes") => render_passes::configure(renderer_config, "only-pipenet", "all"),
        Some("cables") | Some("wires") => render_passes::configure(renderer_config, "only-powernet", "all"),
        Some("pipes-and-cables") | Some("wires-and-pipes") => {
            render_passes::configure(renderer_config, "only-wires-and-pipes", "all")
        }
        Some(custom) => render_passes::configure(renderer_config, custom, ""),
        None => render_passes::configure(renderer_config, "", ""),
    };

    let errors: RwLock<HashSet<String>> = Default::default();
    let bump = bumpalo::Bump::new();

    // z_level uses 0-based index internally
    let z_level = state.map.z_level(z - 1);

    let ctx = minimap::Context {
        objtree: &state.objtree,
        map: &state.map,
        level: z_level,
        // min/max are 0-based for the renderer
        min: (x1 - 1, y1 - 1),
        max: (x2 - 1, y2 - 1),
        render_passes: &render_passes,
        errors: &errors,
        bump: &bump,
    };

    let image = minimap::generate(ctx, &state.icon_cache)
        .map_err(|_| anyhow::anyhow!("Render failed"))?;

    let png_bytes = image.to_bytes()
        .map_err(|e| anyhow::anyhow!("PNG encoding failed: {}", e))?;

    let base64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);

    // Collect any icon errors that occurred
    let icon_errors: Vec<String> = errors.read().unwrap().iter().cloned().collect();

    Ok(RenderResult {
        base64_png: base64,
        width_px: image.width,
        height_px: image.height,
        width_tiles: (x2 - x1 + 1) as u32,
        height_tiles: (y2 - y1 + 1) as u32,
        icon_errors,
    })
}

/// Render all tiles belonging to a specific area.
///
/// Finds the bounding box of the area and renders that region.
/// Returns None if the area doesn't exist.
pub fn render_area(
    state: &ServerState,
    area_path: &str,
    render_pass_filter: Option<&str>,
) -> Result<Option<RenderResult>> {
    let tiles = state.index.area_tiles(area_path);
    if tiles.is_empty() {
        return Ok(None);
    }

    // Find bounding box
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    let mut z = tiles[0].2;

    for &(tx, ty, tz) in tiles {
        min_x = min_x.min(tx);
        min_y = min_y.min(ty);
        max_x = max_x.max(tx);
        max_y = max_y.max(ty);
        z = tz; // Areas are typically single-z
    }

    // Add 1-tile padding for context
    let pad = 1;
    min_x = (min_x - pad).max(1);
    min_y = (min_y - pad).max(1);
    max_x = (max_x + pad).min(state.index.dim_x as i32);
    max_y = (max_y + pad).min(state.index.dim_y as i32);

    let result = render_region(
        state,
        min_x as usize,
        min_y as usize,
        max_x as usize,
        max_y as usize,
        z as usize,
        render_pass_filter,
    )?;

    Ok(Some(result))
}

/// Result of a render operation.
pub struct RenderResult {
    /// The rendered image as base64-encoded PNG
    pub base64_png: String,
    /// Image width in pixels
    pub width_px: u32,
    /// Image height in pixels
    pub height_px: u32,
    /// Region width in tiles
    pub width_tiles: u32,
    /// Region height in tiles
    pub height_tiles: u32,
    /// Any icon loading errors encountered during rendering
    pub icon_errors: Vec<String>,
}
