# ar-cli

## Purpose

The `aroute` (also `ar`) binary. Subcommands: `serve`, `run`, `doctor`, `import`, `auth`, `sync`, `dashboard`, `mcp`, and the `/ui` control page. `main.rs` builds the command graph; `serve.rs` owns the listener and shutdown.

## Configuration

`--config` or `AR_CONFIG` selects the config file. Provider keys are `$VAR` references resolved from the environment, typically `ar.env` loaded by the service unit. `ServeArgs` takes `--port`; the listener refuses a routable bind.

## Extension points

Add a subcommand as a variant of the clap enum in `cli.rs` and a handler in `commands.rs` or its own module. Keep the AXI contract: exit codes are `0` success, `1` error and `2` usage, and nothing prompts interactively.

## Limitations

`serve` refuses to start on a malformed compiled-in table (`check_embedded`), an empty API key, or a routable bind. Shutdown drains in-flight responses for at most `DRAIN_WINDOW` (10 s), then drops the rest. The window is shorter than the unit's `TimeoutStopSec`.

## Model experience

Scripts and agents drive the CLI without a TTY: stdout carries data and structured errors, stderr carries diagnostics, and a bare `--version` exits before the command graph loads.
