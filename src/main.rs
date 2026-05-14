//! SS13 Map MCP Server
//!
//! An MCP (Model Context Protocol) server that provides spatial intelligence
//! over SS13 map files. Uses SpacemanDMM's dreammaker and dmm-tools crates
//! to parse the DM environment and map data, builds a spatial index, and
//! exposes query tools over MCP for AI-assisted map making and validation.
//!
//! Usage:
//!   ss13-map-mcp --dme path/to/tgstation.dme --dmm path/to/_maps/map_files/Station.dmm

mod index;
mod state;
mod tools;

use std::path::PathBuf;
use std::sync::Arc;
use anyhow::Result;
use rmcp::ServiceExt;

/// CLI arguments
struct Args {
    dme_path: PathBuf,
    dmm_path: PathBuf,
}

fn parse_args() -> Result<Args> {
    let args: Vec<String> = std::env::args().collect();

    let mut dme_path = None;
    let mut dmm_path = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--dme" => {
                i += 1;
                dme_path = Some(PathBuf::from(&args[i]));
            }
            "--dmm" => {
                i += 1;
                dmm_path = Some(PathBuf::from(&args[i]));
            }
            "--help" | "-h" => {
                eprintln!("Usage: ss13-map-mcp --dme <path/to/tgstation.dme> --dmm <path/to/station.dmm>");
                eprintln!();
                eprintln!("MCP server for SS13 map intelligence. Communicates over stdio.");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --dme <path>  Path to the .dme environment file");
                eprintln!("  --dmm <path>  Path to the .dmm map file to load");
                std::process::exit(0);
            }
            other => {
                anyhow::bail!("Unknown argument: {}. Use --help for usage.", other);
            }
        }
        i += 1;
    }

    Ok(Args {
        dme_path: dme_path.ok_or_else(|| anyhow::anyhow!("Missing --dme argument"))?,
        dmm_path: dmm_path.ok_or_else(|| anyhow::anyhow!("Missing --dmm argument"))?,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    // Log to stderr so MCP stdio communication isn't disrupted
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let args = parse_args()?;

    // Load environment and map
    let server_state = state::ServerState::load(&args.dme_path, &args.dmm_path)?;
    let state = Arc::new(server_state);

    tracing::info!("Starting MCP server over stdio...");

    // Create tool router and start MCP server
    let map_tools = tools::MapTools::new(state);
    let transport = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    let server = map_tools.serve(transport).await?;

    tracing::info!("MCP server running. Waiting for requests...");
    server.waiting().await?;

    Ok(())
}
