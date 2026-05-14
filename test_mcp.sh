#!/bin/bash
# Quick MCP protocol test — send initialize + tool calls to the server
# Usage: ./test_mcp.sh

DME="/mnt/c/Users/robot/dev/ss13-blastwave/tgstation.dme"
DMM="/mnt/c/Users/robot/dev/ss13-blastwave/_maps/map_files/Deltastation/DeltaStation2.dmm"
BIN="./target/release/ss13-map-mcp"

echo "Testing MCP server against DeltaStation..."

# MCP initialize + list_tools + a query, piped via stdin
{
  # Initialize
  echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}'
  sleep 1
  
  # Initialized notification
  echo '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  sleep 0.5
  
  # List tools
  echo '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
  sleep 1
  
  # Query: list areas matching /area/station/engineering
  echo '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_areas","arguments":{"prefix":"/area/station/engineering"}}}'
  sleep 1
  
  # Query: search for APC objects
  echo '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search_objects","arguments":{"query":"power/apc","limit":10}}}'
  sleep 1
  
} | timeout 30 $BIN --dme "$DME" --dmm "$DMM" 2>/tmp/ss13-mcp-stderr.log

echo ""
echo "=== STDERR (server logs) ==="
cat /tmp/ss13-mcp-stderr.log
