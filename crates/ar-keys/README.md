# ar-keys

## Purpose

Credential protection, scoped access tokens, and lane admission, under one master key. `hash` is the `enc:v2:` envelope, `token` is HS256 with `read:*` and `write:*` scopes, `admit` runs three lanes, and `audit` is a bounded ring that cannot hold a credential.

## Configuration

The master key comes from the credential store at `aroute serve` boot. Lane sizes and token TTLs are set at construction by `ar-server`.

## Extension points

Add a scope by extending the token scope enum and the default-deny check in `ar-mcp`. Add a lane by extending the admission enum. Both are small matches.

## Limitations

One master key protects all rows, so losing it loses every stored secret. `revoke` withdraws a token by its `jti`. The audit ring is bounded and drops old lines.

## Model experience

The `heavy` lane cannot starve `interactive`; that is the admission design.
