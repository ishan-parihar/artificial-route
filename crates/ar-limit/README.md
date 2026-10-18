# ar-limit

## Purpose

In-process request limiting: a per-key token bucket ported from agentgateway, keeping its spec shape (`tokens_per_fill`, `fill_interval`, `max_tokens`) and dropping its CEL key expression and `quick_cache` dependency.

## Configuration

`Spec::per_minute(rpm)` gives a burst of `rpm` refilled at `rpm` per minute. `Spec::per_minute(0)` is refused at the config edge.

## Extension points

Build a different limit by constructing a `Spec` directly. The key is a string the caller chooses, so a new keying scheme is a call-site change, not a crate change.

## Limitations

Buckets are per process. A deployment running several proxies gets one limit per instance, and a cluster-wide ceiling needs shared state, which is out of scope. The map is capped at `MAX_BUCKETS` (10 000) and evicts the oldest-refilled bucket.

## Model experience

A limited request returns 429 with `Retry-After` set to the seconds until a token lands, rounded up so a client that waits exactly that long is admitted.
