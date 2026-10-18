# ar-tokens

## Purpose

Token counting, usage normalization, pricing and the usage ledger. `count_text` estimates tokens, `NormalizedUsage` reads a provider's `usage` object into `{prompt, completion, total}`, and `Ledger` prices and records it append-only.

## Configuration

Prices come from the compiled-in pricing table (`pricing.rs`, `meta.rs`). The usage ledger is the one in `ar-obs`.

## Extension points

Add a pricing row to the table. Add a usage shape by adding a case to `usage.rs`. Both are data plus one match.

## Limitations

`count_text` is an estimate, not the provider's tokenizer, so its count can differ from the provider's. Prices are only as current as the table.

## Model experience

Response headers report tokens, cost and savings, computed from the same priced row the ledger stores.
