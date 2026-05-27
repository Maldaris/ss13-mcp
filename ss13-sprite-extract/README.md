# ss13-sprite-extract

Extract a complete sprite corpus with semantic metadata from any SS13 codebase. Designed to produce training data for LoRA/fine-tuning pixel art generation models.

## What it does

1. **Parses** the `.dme` file using SpacemanDMM's `dreammaker` crate to build the full object tree
2. **Walks** every type under `/obj/`, `/turf/`, `/mob/` and resolves `icon`, `icon_state`, `name`, `desc` through inheritance
3. **Loads** every unique `.dmi` spritesheet and extracts individual sprites as PNGs
4. **Outputs** a structured corpus: individual PNGs + semantic metadata (JSON)

## Usage

```bash
# Build
cargo build --release

# Full extraction (all types)
target/release/sprite-extract /path/to/tgstation.dme -o output/

# Filter to specific types
target/release/sprite-extract /path/to/tgstation.dme -o output/ --filter "/obj/machinery"
target/release/sprite-extract /path/to/tgstation.dme -o output/ --filter "/obj/item/food"
```

## Output Structure

```
output/
├── manifest.json      # Summary + DMI file index (compact)
├── types.json         # All type metadata (type_path, name, desc, parent_chain, icon refs)
├── sprites.json       # All sprite entries (file path, DMI source, state, direction, frame)
└── sprites/
    ├── icons__obj__machines__power/
    │   ├── apc0_south.png
    │   ├── apc1_south.png
    │   ├── apcemag_south.png
    │   └── ...
    ├── icons__mob__human/
    │   ├── default_south.png
    │   ├── default_north.png
    │   └── ...
    └── ...
```

### manifest.json
- `total_types`, `total_dmi_files`, `total_sprites` — counts
- `dmi_files[]` — index of every DMI processed: path, dimensions, state names, output directory

### types.json
- Array of type metadata objects
- `type_path` — full DM path (e.g. `/obj/machinery/power/apc`)
- `name`, `desc` — in-game name and description
- `parent_chain` — full inheritance chain
- `icon_file` — DMI file path relative to codebase root
- `icon_state` — default icon state for this type

### sprites.json
- Array of sprite entries
- `file` — relative path to the PNG
- `dmi_source` — which DMI file this came from
- `state_name`, `direction`, `frame` — exact DMI coordinates

## Performance

On a NovaSector fork (large tgstation derivative):

| Metric | Value |
|--------|-------|
| Types extracted | 29,570 |
| Unique DMI files | 1,512 |
| Sprites produced | 62,864 |
| Parse time | ~90s |
| Extract time | ~13s |
| Output size | 285 MB |

## Dependencies

Shares the patched SpacemanDMM crates from `ss13-map-mcp`. The `dreammaker` crate provides `.dme` parsing and the object tree; `dmm-tools` provides DMI loading, sprite compositing, and PNG output.

## Training Data Notes

Each sprite PNG is a single state × direction × frame from a DMI spritesheet. Most sprites are 32×32 pixels. The semantic metadata lets you build caption datasets:

```
"A 32x32 pixel art sprite of an area power controller in SS13, showing the 
emagged/hacked state. This is /obj/machinery/power/apc, a control terminal 
for the area's electrical systems."
```

The type hierarchy (parent chain) provides taxonomic context for organizing training samples.
