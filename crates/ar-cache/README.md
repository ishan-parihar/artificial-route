# ar-cache

## Purpose

Exact-match response cache. Keys are `blake3` digests of a canonical request, held in a 32 MB `quick_cache` memory tier with an optional bounded `redb` file, plus a 24 h `Idempotency-Key` store and the prefix-pin affinity hook that `ar-route` calls.

## Configuration

Sized by the memory-tier bound. `CacheConfig::memory_only()` and `with_mem_bytes(..)` set it; the default is `DEFAULT_MEM_BYTES` (32 MB). `ar-server` passes the bound through `Components::cache_bytes`.

## Extension points

Key composition lives in `key::request_key_with`, which takes the tenant, the served target, the body, and the caller key. `CacheState::Bypass` is decided by the caller, not inferred here. `Cache::get`, `store` and `store_with_ttl` are the read and write surfaces.

## Limitations

Exact match only: there is no semantic or embedding cache. The memory tier is per process. A key covers only the fields in the canonical request, so a field the key omits cannot distinguish two requests. Oversized values are refused rather than stored truncated.

## Model experience

A repeated non-streaming request returns `x-ar-cache: hit` with the stored body. A miss returns `miss`. Streamed requests report `bypass`, because a stream is never looked up. A reply served by a fallback is not stored under the winner's key.
