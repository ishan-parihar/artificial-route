# ar-config

## Purpose

File-mode configuration. One YAML file is the whole source of truth. `ConfigHandle` holds an `ArcSwap` snapshot, reloads it, and watches the file for changes.

## Configuration

`Config::parse` expands `$VAR` references on the raw text before YAML parsing, so a secret lives in the environment and never in the file. `empty_api_keys` lists any key whose variable is set but empty.

## Extension points

Add a config field to the serde structs in `lib.rs` and give it a default. The reload path is `ConfigHandle::reload`.

## Limitations

No overlay or control plane: the file is the only source (xDS and SPIFFE are out of scope per `docs/00`). A reload refuses a config with an empty API key and keeps the previous one serving. `load` stays permissive so `doctor` can still read a broken file.

## Model experience

A reload that fails leaves the running proxy on its previous config.
