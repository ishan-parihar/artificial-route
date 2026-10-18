# ar-mcp

## Purpose

The optional MCP control plane: the essential-12 tool catalog from `docs/06`, as thin wrappers over the same routing, pricing, admission and OAuth functions the data plane calls. Also an append-only `redb` audit row per guarded call.

## Configuration

Enabled with `--features mcp`, which is off by default so the default build links none of it. `aroute mcp` starts the stdio transport.

## Extension points

Add a tool by adding a `Tool` entry to the catalog and a wrapper that calls an existing function. Route every body through `guard`, which is the only entry point the scope check protects.

## Limitations

The tool bodies are `pub(crate)` and reachable only through `guard`. Default-deny scope means an unscoped call fails.

## Model experience

An agent discovers tools with `tool_search`, which queries the catalog.
