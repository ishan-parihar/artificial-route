# ar-obs

## Purpose

Observability: Prometheus metrics capped at `provider|family|decision`, a JSON `tracing` subscriber on a dedicated writer thread, and a redacted audit ledger in `redb`. No prompt, model name, key or session reaches a metric label.

## Configuration

`AR_OBS_DIR` selects the directory for the trace files and the ledger. The live `aroute.service` unit sets it to `~/.local/state/ar/obs`, a persistent XDG state directory, so the audit history survives a reboot. Point it at a persistent directory before relying on the ledger.

## Extension points

Add a metric by adding a counter or histogram with the bounded label set. The label set is capped at `provider|family|decision`, so a new label is a deliberate change.

## Limitations

No BigQuery and no cloud export: shipping prompts to a warehouse is a data-egress decision, not a default. Traces rotate daily and are pruned at 7 days. A raw-prompt path exists only behind an admin grant and expires after 24 h.

## Model experience

An operator reads `/metrics` in Prometheus text format. Response headers carry the decision, usage, cost and cache state, so a client sees the same numbers an operator does.
