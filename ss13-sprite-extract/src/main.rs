use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use tracing::{info, debug};

use dreammaker::constants::Constant;
use dreammaker::dmi::{Dir, Dirs, StateIndex};
use dreammaker::objtree::TypeRef;
use dmm_tools::dmi::Image;
use dmm_tools::IconCache;

mod overlay_analyzer;
mod walker;

/// Extract sprite corpus with semantic metadata from SS13 codebases
#[derive(Parser, Debug)]
#[command(name = "sprite-extract", version, about)]
struct Args {
    /// Path to the .dme file
    #[arg()]
    dme_path: PathBuf,

    /// Output directory for extracted sprites
    #[arg(short, long, default_value = "output")]
    output: PathBuf,

    /// Only extract types matching this prefix (e.g. "/obj/machinery")
    #[arg(short, long)]
    filter: Option<String>,

    /// Skip types with no icon defined
    #[arg(long, default_value_t = true)]
    skip_no_icon: bool,

    /// Extract only first frame of animations
    #[arg(long, default_value_t = true)]
    first_frame_only: bool,
}

// ── Output structures ──────────────────────────────────────────────

/// Metadata for a single DM type
#[derive(Debug, Serialize)]
struct TypeMeta {
    type_path: String,
    name: Option<String>,
    desc: Option<String>,
    parent_path: String,
    parent_chain: Vec<String>,
    icon_file: Option<String>,
    icon_state: Option<String>,
    /// Overlay analysis results (if update_overlays was analyzable)
    #[serde(skip_serializing_if = "Option::is_none")]
    overlay_info: Option<overlay_analyzer::OverlayInfo>,
}

/// Metadata for a unique DMI file
#[derive(Debug, Serialize)]
struct DmiMeta {
    path: String,
    width: u32,
    height: u32,
    state_count: usize,
    states: Vec<String>,
    sprite_dir: String,
}

/// A single extracted sprite file
#[derive(Debug, Serialize)]
struct SpriteEntry {
    /// Relative path to the PNG within the output dir
    file: String,
    /// DMI source file
    dmi_source: String,
    /// State name within the DMI
    state_name: String,
    /// Direction
    direction: String,
    /// Frame index (0-based)
    frame: usize,
    /// Sprite width
    width: u32,
    /// Sprite height
    height: u32,
}

/// Top-level corpus manifest
#[derive(Debug, Serialize)]
struct Manifest {
    source_dme: String,
    total_types: usize,
    total_dmi_files: usize,
    total_sprites: usize,
    dmi_files: Vec<DmiMeta>,
    types: Vec<TypeMeta>,
    sprites: Vec<SpriteEntry>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sprite_extract=info".into()),
        )
        .init();

    let args = Args::parse();

    // Parse the codebase
    info!("Parsing {}...", args.dme_path.display());
    let ctx = dreammaker::Context::default();
    let dme_path = args.dme_path.canonicalize()
        .context("Failed to canonicalize .dme path")?;

    let pp = dreammaker::preprocessor::Preprocessor::new(&ctx, dme_path.clone())
        .context("Failed to create preprocessor")?;
    let indents = dreammaker::indents::IndentProcessor::new(&ctx, pp);
    let mut parser = dreammaker::parser::Parser::new(&ctx, indents);
    parser.enable_procs();  // Parse proc bodies so we can analyze update_overlays()
    let objtree = parser.parse_object_tree();

    let error_count = ctx.errors().iter().filter(|e| e.severity() == dreammaker::Severity::Error).count();
    let warning_count = ctx.errors().iter().filter(|e| e.severity() == dreammaker::Severity::Warning).count();
    info!("Parsed object tree: {} errors, {} warnings", error_count, warning_count);

    // Set up icon cache
    let codebase_root = dme_path.parent().unwrap_or(Path::new("."));
    let mut icon_cache = IconCache::default();
    icon_cache.set_icons_root(codebase_root);

    // Walk the object tree
    let types = walker::collect_extractable_types(&objtree, args.filter.as_deref());
    info!("Found {} types to extract", types.len());

    // Create output directory
    std::fs::create_dir_all(&args.output)
        .context("Failed to create output directory")?;
    std::fs::create_dir_all(args.output.join("sprites"))
        .context("Failed to create sprites directory")?;

    // ── Phase 1: Collect all unique DMI files referenced by types ──
    let pb = ProgressBar::new(types.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:60.cyan/blue} {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("█▓░"),
    );

    let mut type_metas: Vec<TypeMeta> = Vec::new();
    let mut dmi_files_needed: HashSet<String> = HashSet::new();

    for type_ref in &types {
        let path = type_ref.get().path.clone();
        pb.set_message(path.clone());

        let icon_resource = resolve_var_string(type_ref, "icon");
        let icon_state = resolve_var_string(type_ref, "icon_state");
        let name = resolve_var_string(type_ref, "name");
        let desc = resolve_var_string(type_ref, "desc");

        let parent_chain = build_parent_chain(type_ref);
        let parent_path = type_ref
            .parent_type()
            .map(|p| p.get().path.clone())
            .unwrap_or_default();

        if let Some(ref icon) = icon_resource {
            dmi_files_needed.insert(icon.clone());
        }

        // Analyze overlays from update_overlays() proc AST
        let overlay_info = overlay_analyzer::analyze_overlays(type_ref);

        type_metas.push(TypeMeta {
            type_path: path,
            name,
            desc,
            parent_path,
            parent_chain,
            icon_file: icon_resource,
            icon_state,
            overlay_info,
        });

        pb.inc(1);
    }

    pb.finish_with_message("Types collected");

    // ── Phase 1.5: Inherit overlay info for types without own update_overlays ──
    // Build a map of type_path → overlay_info for types that have their own
    let overlay_map: std::collections::HashMap<String, overlay_analyzer::OverlayInfo> = type_metas.iter()
        .filter_map(|t| t.overlay_info.as_ref().map(|oi| (t.type_path.clone(), oi.clone())))
        .collect();

    let mut inherited_count = 0usize;
    for (idx, type_ref) in types.iter().enumerate() {
        if type_metas[idx].overlay_info.is_some() {
            continue; // Already has own overlay analysis
        }

        // Walk parent chain to find an ancestor with overlay info
        for parent_path in &type_metas[idx].parent_chain {
            if let Some(parent_info) = overlay_map.get(parent_path) {
                if let Some(inherited) = overlay_analyzer::inherit_overlays(type_ref, parent_info) {
                    // Also collect any icon overrides from overlays
                    if let Some(ref icon) = type_metas[idx].icon_file {
                        dmi_files_needed.insert(icon.clone());
                    }
                    type_metas[idx].overlay_info = Some(inherited);
                    inherited_count += 1;
                    break;
                }
            }
        }
    }

    if inherited_count > 0 {
        info!("Inherited overlay info for {} child types", inherited_count);
    }

    // ── Phase 2: Extract all sprites from unique DMI files ──
    info!("Extracting sprites from {} unique DMI files...", dmi_files_needed.len());
    let pb2 = ProgressBar::new(dmi_files_needed.len() as u64);
    pb2.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] {bar:60.green/blue} {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("█▓░"),
    );

    let mut all_sprites: Vec<SpriteEntry> = Vec::new();
    let mut dmi_metas: Vec<DmiMeta> = Vec::new();
    let mut extracted_count = 0usize;

    for dmi_path_str in &dmi_files_needed {
        pb2.set_message(dmi_path_str.clone());

        let icon_path = Path::new(dmi_path_str.as_str());
        let icon_file = match icon_cache.retrieve_uniq(icon_path) {
            Some(f) => f,
            None => {
                debug!("Could not load DMI: {}", dmi_path_str);
                pb2.inc(1);
                continue;
            }
        };

        // Create output subdir for this DMI file
        let dmi_dir_name = sanitize_dmi_path(dmi_path_str);
        let dmi_out_dir = args.output.join("sprites").join(&dmi_dir_name);
        std::fs::create_dir_all(&dmi_out_dir)?;

        // Extract every state
        for state in &icon_file.metadata.states {
            let dirs = match state.dirs {
                Dirs::One => vec![Dir::South],
                Dirs::Four => Dir::CARDINALS.to_vec(),
                Dirs::Eight => Dir::ALL.to_vec(),
            };

            let frame_count = match &state.frames {
                dreammaker::dmi::Frames::One => 1,
                dreammaker::dmi::Frames::Count(n) => {
                    if args.first_frame_only { 1 } else { *n }
                }
                dreammaker::dmi::Frames::Delays(d) => {
                    if args.first_frame_only { 1 } else { d.len() }
                }
            };

            for &dir in &dirs {
                let dir_str = dir_to_string(dir);

                for frame in 0..frame_count {
                    let state_idx = StateIndex::from(state.name.as_str());
                    let rect = match icon_file.metadata.rect_of(
                        icon_file.image.width,
                        &state_idx,
                        dir,
                        frame as u32,
                    ) {
                        Some(r) => r,
                        None => continue,
                    };

                    // Build filename
                    let state_safe = sanitize_filename(&state.name);
                    let filename = if frame_count > 1 {
                        format!("{}_{}_f{}.png", state_safe, dir_str, frame)
                    } else {
                        format!("{}_{}.png", state_safe, dir_str)
                    };

                    let sprite_path = dmi_out_dir.join(&filename);

                    // Extract and write the sprite
                    let mut sprite = Image::new_rgba(
                        icon_file.metadata.width,
                        icon_file.metadata.height,
                    );
                    sprite.composite(
                        &icon_file.image,
                        (0, 0),
                        rect,
                        [255, 255, 255, 255],
                    );
                    sprite.to_file(&sprite_path)
                        .with_context(|| format!("Failed to write {}", sprite_path.display()))?;

                    let rel_path = format!("sprites/{}/{}", dmi_dir_name, filename);

                    all_sprites.push(SpriteEntry {
                        file: rel_path,
                        dmi_source: dmi_path_str.clone(),
                        state_name: state.name.clone(),
                        direction: dir_str.to_string(),
                        frame,
                        width: icon_file.metadata.width,
                        height: icon_file.metadata.height,
                    });

                    extracted_count += 1;
                }
            }
        }

        // Record DMI metadata
        dmi_metas.push(DmiMeta {
            path: dmi_path_str.clone(),
            width: icon_file.metadata.width,
            height: icon_file.metadata.height,
            state_count: icon_file.metadata.states.len(),
            states: icon_file.metadata.states.iter().map(|s| s.name.clone()).collect(),
            sprite_dir: format!("sprites/{}", dmi_dir_name),
        });

        pb2.inc(1);
    }

    pb2.finish_with_message("Sprites extracted");

    // ── Phase 2.5: Composite overlay combinations ──
    let types_with_overlays: Vec<&TypeMeta> = type_metas.iter()
        .filter(|t| t.overlay_info.is_some() && t.icon_file.is_some())
        .collect();

    if !types_with_overlays.is_empty() {
        info!("Compositing overlays for {} types...", types_with_overlays.len());

        let composites_dir = args.output.join("composites");
        std::fs::create_dir_all(&composites_dir)?;

        let pb3 = ProgressBar::new(types_with_overlays.len() as u64);
        pb3.set_style(
            ProgressStyle::default_bar()
                .template("[{elapsed_precise}] {bar:60.magenta/blue} {pos}/{len} {msg}")
                .unwrap()
                .progress_chars("█▓░"),
        );

        let mut composite_count = 0usize;

        for type_meta in &types_with_overlays {
            pb3.set_message(type_meta.type_path.clone());

            let icon_path_str = type_meta.icon_file.as_ref().unwrap();
            let icon_path = Path::new(icon_path_str.as_str());
            let overlay_info = type_meta.overlay_info.as_ref().unwrap();

            let icon_file = match icon_cache.retrieve_uniq(icon_path) {
                Some(f) => f,
                None => { pb3.inc(1); continue; }
            };

            // Create output dir for this type's composites
            let type_dir = composites_dir.join(type_meta.type_path.trim_start_matches('/'));
            std::fs::create_dir_all(&type_dir)?;

            // ── Visual States path (state-machine aware) ──
            // When visual_states are available, use them instead of combos.
            // Each visual state specifies a base icon_state override + resolved overlays.
            if !overlay_info.visual_states.is_empty() {
                for vs in &overlay_info.visual_states {
                    // Determine the base icon_state for this visual state
                    let vs_base_state = vs.base_icon_state.as_deref()
                        .unwrap_or(type_meta.icon_state.as_deref().unwrap_or(""));
                    let vs_base_idx = StateIndex::from(vs_base_state);

                    let base_state_meta = icon_file.metadata.state_names.get(&vs_base_idx)
                        .and_then(|pos| icon_file.metadata.states.get(*pos));

                    let dirs = match base_state_meta.map(|s| s.dirs) {
                        Some(Dirs::One) | None => vec![Dir::South],
                        Some(Dirs::Four) => Dir::CARDINALS.to_vec(),
                        Some(Dirs::Eight) => Dir::ALL.to_vec(),
                    };

                    // Get frame count for the base state
                    let base_frame_count = match base_state_meta.map(|s| &s.frames) {
                        Some(dreammaker::dmi::Frames::One) | None => 1,
                        Some(dreammaker::dmi::Frames::Count(n)) => *n,
                        Some(dreammaker::dmi::Frames::Delays(d)) => d.len(),
                    };

                    for &dir in &dirs {
                        let dir_str = dir_to_string(dir);

                        // Iterate over animation frames
                        for frame in 0..base_frame_count {
                            let mut composite = Image::new_rgba(
                                icon_file.metadata.width,
                                icon_file.metadata.height,
                            );

                            // Composite the base state at this frame
                            if let Some(rect) = icon_file.metadata.rect_of(
                                icon_file.image.width,
                                &vs_base_idx,
                                dir,
                                frame as u32,
                            ) {
                                composite.composite(&icon_file.image, (0, 0), rect, [255, 255, 255, 255]);
                            }

                            // Composite each overlay, matching animation frame when available
                            let mut valid_overlays = Vec::new();
                            for overlay_state_name in &vs.overlays {
                                let resolved_names = resolve_overlay_pattern(overlay_state_name, icon_file);
                                for resolved in &resolved_names {
                                    let overlay_idx = StateIndex::from(resolved.as_str());

                                    // Try to match the same frame index for animated overlays
                                    let overlay_frame = {
                                        let overlay_meta = icon_file.metadata.state_names.get(&overlay_idx)
                                            .and_then(|pos| icon_file.metadata.states.get(*pos));
                                        let overlay_frame_count = match overlay_meta.map(|s| &s.frames) {
                                            Some(dreammaker::dmi::Frames::One) | None => 1,
                                            Some(dreammaker::dmi::Frames::Count(n)) => *n,
                                            Some(dreammaker::dmi::Frames::Delays(d)) => d.len(),
                                        };
                                        // Use same frame if overlay has matching frame count, else frame 0
                                        if overlay_frame_count == base_frame_count {
                                            frame as u32
                                        } else {
                                            0
                                        }
                                    };

                                    if let Some(rect) = icon_file.metadata.rect_of(
                                        icon_file.image.width,
                                        &overlay_idx,
                                        dir,
                                        overlay_frame,
                                    ) {
                                        composite.composite(&icon_file.image, (0, 0), rect, [255, 255, 255, 255]);
                                        valid_overlays.push(resolved.clone());
                                    }
                                }
                            }

                            if valid_overlays.is_empty() && base_state_meta.is_none() {
                                continue; // No base and no overlays — skip
                            }

                            // Build filename: state_name + frame (if animated) + direction
                            let filename = if base_frame_count > 1 {
                                format!("{}_{}_f{}.png",
                                    sanitize_filename(&vs.name),
                                    dir_str,
                                    frame,
                                )
                            } else {
                                format!("{}_{}.png",
                                    sanitize_filename(&vs.name),
                                    dir_str,
                                )
                            };

                            let sprite_path = type_dir.join(&filename);
                            composite.to_file(&sprite_path)
                                .with_context(|| format!("Failed to write composite {}", sprite_path.display()))?;

                            let rel_path = format!("composites/{}/{}",
                                type_meta.type_path.trim_start_matches('/'),
                                filename,
                            );

                            let overlay_desc = if valid_overlays.is_empty() {
                                vs.name.clone()
                            } else {
                                format!("{}+{}", vs.name, valid_overlays.join("+"))
                            };

                            all_sprites.push(SpriteEntry {
                                file: rel_path,
                                dmi_source: icon_path_str.clone(),
                                state_name: overlay_desc,
                                direction: dir_str.to_string(),
                                frame,
                                width: icon_file.metadata.width,
                                height: icon_file.metadata.height,
                            });

                            extracted_count += 1;
                            composite_count += 1;
                        }
                    }
                }

                pb3.inc(1);
                continue; // Skip combo-based compositing for this type
            }

            // ── Combo path (original behavior for types without visual states) ──
            let base_state_name = type_meta.icon_state.as_deref().unwrap_or("");
            let base_state_idx = StateIndex::from(base_state_name);

            for (combo_idx, combo) in overlay_info.combos.iter().enumerate() {
                let base_state = icon_file.metadata.state_names.get(&base_state_idx)
                    .and_then(|pos| icon_file.metadata.states.get(*pos));

                let dirs = match base_state.map(|s| s.dirs) {
                    Some(Dirs::One) | None => vec![Dir::South],
                    Some(Dirs::Four) => Dir::CARDINALS.to_vec(),
                    Some(Dirs::Eight) => Dir::ALL.to_vec(),
                };

                for &dir in &dirs {
                    let dir_str = dir_to_string(dir);

                    let mut composite = Image::new_rgba(
                        icon_file.metadata.width,
                        icon_file.metadata.height,
                    );

                    if let Some(rect) = icon_file.metadata.rect_of(
                        icon_file.image.width,
                        &base_state_idx,
                        dir,
                        0,
                    ) {
                        composite.composite(&icon_file.image, (0, 0), rect, [255, 255, 255, 255]);
                    }

                    let mut valid_overlays = Vec::new();
                    for overlay_state_name in combo {
                        let resolved_names = resolve_overlay_pattern(overlay_state_name, icon_file);
                        for resolved in &resolved_names {
                            let overlay_idx = StateIndex::from(resolved.as_str());
                            if let Some(rect) = icon_file.metadata.rect_of(
                                icon_file.image.width,
                                &overlay_idx,
                                dir,
                                0,
                            ) {
                                composite.composite(&icon_file.image, (0, 0), rect, [255, 255, 255, 255]);
                                valid_overlays.push(resolved.clone());
                            }
                        }
                    }

                    if valid_overlays.is_empty() {
                        continue;
                    }

                    let combo_desc = if valid_overlays.len() <= 3 {
                        valid_overlays.iter()
                            .map(|s| sanitize_filename(s))
                            .collect::<Vec<_>>()
                            .join("+")
                    } else {
                        format!("combo{}", combo_idx)
                    };

                    let filename = format!("{}_{}_{}.png",
                        sanitize_filename(base_state_name),
                        combo_desc,
                        dir_str,
                    );

                    let sprite_path = type_dir.join(&filename);
                    composite.to_file(&sprite_path)
                        .with_context(|| format!("Failed to write composite {}", sprite_path.display()))?;

                    let rel_path = format!("composites/{}/{}",
                        type_meta.type_path.trim_start_matches('/'),
                        filename,
                    );

                    all_sprites.push(SpriteEntry {
                        file: rel_path,
                        dmi_source: icon_path_str.clone(),
                        state_name: format!("{}+{}", base_state_name, combo_desc),
                        direction: dir_str.to_string(),
                        frame: 0,
                        width: icon_file.metadata.width,
                        height: icon_file.metadata.height,
                    });

                    extracted_count += 1;
                    composite_count += 1;
                }
            }

            pb3.inc(1);
        }

        pb3.finish_with_message("Composites done");
        info!("Generated {} composite sprites", composite_count);
    }

    // ── Phase 3: Write output files ──

    // Write types index (separate file — can be large)
    let types_path = args.output.join("types.json");
    std::fs::write(&types_path, serde_json::to_string(&type_metas)?)?;

    // Write sprites index (separate file)
    let sprites_path = args.output.join("sprites.json");
    std::fs::write(&sprites_path, serde_json::to_string(&all_sprites)?)?;

    // Write compact manifest (summary + DMI index only)
    let manifest = Manifest {
        source_dme: args.dme_path.display().to_string(),
        total_types: type_metas.len(),
        total_dmi_files: dmi_files_needed.len(),
        total_sprites: extracted_count,
        dmi_files: dmi_metas,
        types: Vec::new(),    // types are in types.json
        sprites: Vec::new(),  // sprites are in sprites.json
    };

    let manifest_path = args.output.join("manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)?;

    info!(
        "Done! {} types, {} DMIs, {} sprites → {}",
        manifest.total_types,
        manifest.total_dmi_files,
        manifest.total_sprites,
        manifest_path.display()
    );

    Ok(())
}

/// Resolve a string var through the type inheritance chain
fn resolve_var_string(type_ref: &TypeRef, var_name: &str) -> Option<String> {
    let mut current = Some(*type_ref);
    while let Some(ty) = current {
        if let Some(var_val) = ty.get().vars.get(var_name) {
            if let Some(ref constant) = var_val.value.constant {
                match constant {
                    Constant::String(s) => return Some(s.to_string()),
                    Constant::Resource(r) => return Some(r.to_string()),
                    Constant::Null(_) => return None,
                    _ => {}
                }
            }
        }
        current = ty.parent_type();
    }
    None
}

/// Build the parent type chain as a list of path strings
fn build_parent_chain(type_ref: &TypeRef) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = type_ref.parent_type();
    while let Some(ty) = current {
        let path = ty.get().path.clone();
        if path.is_empty() {
            break;
        }
        chain.push(path);
        current = ty.parent_type();
    }
    chain
}

fn dir_to_string(dir: Dir) -> &'static str {
    match dir {
        Dir::North => "north",
        Dir::South => "south",
        Dir::East => "east",
        Dir::West => "west",
        Dir::Northeast => "northeast",
        Dir::Northwest => "northwest",
        Dir::Southeast => "southeast",
        Dir::Southwest => "southwest",
    }
}

/// Resolve overlay patterns like "apcox-[locked]" to actual DMI state names.
/// Patterns with `[var]` placeholders get matched against all states in the DMI.
fn resolve_overlay_pattern(pattern: &str, icon_file: &dmm_tools::dmi::IconFile) -> Vec<String> {
    // If no interpolation brackets, treat as literal
    if !pattern.contains('[') {
        // Check if this exact state exists
        let idx = StateIndex::from(pattern);
        if icon_file.metadata.state_names.contains_key(&idx) {
            return vec![pattern.to_string()];
        }
        return Vec::new();
    }

    // Build a regex-like pattern: replace [anything] with a wildcard match
    // Then find all DMI states that match
    let mut regex_parts = Vec::new();
    let mut remaining = pattern;

    while let Some(bracket_start) = remaining.find('[') {
        // Add literal prefix
        regex_parts.push(regex_escape(&remaining[..bracket_start]));

        if let Some(bracket_end) = remaining[bracket_start..].find(']') {
            // Replace [var] with a wildcard that matches common DM values
            regex_parts.push(".+".to_string());
            remaining = &remaining[bracket_start + bracket_end + 1..];
        } else {
            // Malformed — no closing bracket
            return Vec::new();
        }
    }
    // Add trailing literal
    regex_parts.push(regex_escape(remaining));

    let pattern_str = format!("^{}$", regex_parts.join(""));

    // Simple regex match against all state names
    let mut matches = Vec::new();
    for state in &icon_file.metadata.states {
        if simple_regex_match(&pattern_str, &state.name) {
            matches.push(state.name.clone());
        }
    }

    matches
}

/// Escape special regex chars in a literal string
fn regex_escape(s: &str) -> String {
    let special = ".*+?()[]{}^$|\\";
    let mut result = String::new();
    for c in s.chars() {
        if special.contains(c) {
            result.push('\\');
        }
        result.push(c);
    }
    result
}

fn sanitize_filename(s: &str) -> String {
    if s.is_empty() {
        return "default".to_string();
    }
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | ' ' => '_',
            c => c,
        })
        .collect()
}

/// Convert a DMI path like "icons/obj/machines/power.dmi" into a safe directory name
fn sanitize_dmi_path(s: &str) -> String {
    s.trim_end_matches(".dmi")
        .replace('/', "__")
        .replace('\\', "__")
}

/// Very simple regex matching for overlay patterns.
/// Supports literal chars and `.+` wildcard (1+ chars).
fn simple_regex_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.trim_start_matches('^');
    let dollar = String::from_utf8(vec![0x24]).unwrap();
    let pattern = pattern.trim_end_matches(dollar.as_str());
    match_recursive(pattern, text)
}

fn match_recursive(pattern: &str, text: &str) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }

    // Check for .+ wildcard
    if let Some(rest) = pattern.strip_prefix(".+") {
        for i in 1..=text.len() {
            if match_recursive(rest, &text[i..]) {
                return true;
            }
        }
        return false;
    }

    // Check for escaped char
    if pattern.len() >= 2 && pattern.as_bytes()[0] == b'\\' {
        let expected = pattern.as_bytes()[1];
        if !text.is_empty() && text.as_bytes()[0] == expected {
            return match_recursive(&pattern[2..], &text[1..]);
        }
        return false;
    }

    // Literal char match
    if let (Some(pc), Some(tc)) = (pattern.as_bytes().first(), text.as_bytes().first()) {
        if pc == tc {
            return match_recursive(&pattern[1..], &text[1..]);
        }
    }

    false
}
