# ss13-map-mcp

MCP (Model Context Protocol) server for SS13 map intelligence. Provides spatial queries, network tracing, object search, and area analysis over BYOND `.dmm` map files.

Built on [SpacemanDMM](https://github.com/SpaceManiac/SpacemanDMM)'s `dreammaker` and `dmm-tools` crates.

## Usage

```bash
ss13-map-mcp --dme path/to/tgstation.dme --dmm path/to/DeltaStation2.dmm
```

Communicates over stdio using the MCP JSON-RPC protocol. Connect from Claude Desktop, Cursor, or any MCP client.

## Tools

| Tool | Description |
|------|-------------|
| `query_tile` | Get all objects at a specific (x, y, z) tile |
| `query_area` | Get all objects in an area, grouped by type |
| `query_object_type` | Find all instances of a type path (matches subtypes) |
| `query_adjacent` | Get objects on N/S/E/W neighboring tiles |
| `query_nearby` | Get all objects within a tile radius |
| `list_areas` | List all areas, optionally filtered by prefix |
| `trace_network` | BFS trace of cable/pipe networks from a starting tile |
| `search_objects` | Substring search across all object type paths on the map |

## Example Queries

**"What's in engineering?"**
```json
{"name": "list_areas", "arguments": {"prefix": "/area/station/engineering"}}
```

**"Find all APCs"**
```json
{"name": "search_objects", "arguments": {"query": "power/apc"}}
```

**"Trace the cable network from tile (128, 128)"**
```json
{"name": "trace_network", "arguments": {"x": 128, "y": 128, "network_type": "/obj/structure/cable"}}
```

## Building

Requires Rust stable.

```bash
cargo build --release
```

Binary: `target/release/ss13-map-mcp`

## Architecture

```
.dmm file (map data)
  → dmm-tools parser → Map (dictionary + grid)
    → SpatialIndex (anchor index, area index, grid index)
      → MCP tools (query_tile, query_area, trace_network, etc.)
        → JSON-RPC over stdio
          → LLM / MCP client
```

Single-pass index build: O(tiles × objects_per_tile). All queries are O(1) lookups or bounded BFS.

## Roadmap

- [ ] **Phase 2:** JavaScript rule DSL for map validation (embedded QuickJS)
- [ ] **Phase 3:** Integration with SpacemanDMM language server (diagnostics in VS Code)
- [ ] Full `.dme` object tree queries (type info, variable resolution, proc lookup)
- [ ] Map write-back (`place_objects` tool for AI-assisted map editing)
- [ ] Watch mode (re-index on `.dmm` file changes)

## License

GPL-3.0 (matching SpacemanDMM)
