# ar-core

## Purpose

Shared vocabulary: the interned name type `Strng` (`Arc<str>`) and the helpers around it (`as_str`, `intern`). Every crate that needs a provider, model or key name depends on it.

## Configuration

None. The crate has no runtime settings.

## Extension points

Use `Strng` for names that are cloned or compared often. New shared types belong in the crate that owns them, not here. Cross-crate seams live where they are implemented, for example `ArExec` in `ar-route`.

## Limitations

`Strng` is 16 bytes, not 8, because `Arc<str>` is a fat pointer. Swapping in `arcstr` is a measured decision, not a default.

## Model experience

Not user-visible. Names interned here appear in the JSON and headers that the crates above it produce.
