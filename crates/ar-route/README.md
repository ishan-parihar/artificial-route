# ar-route

## Purpose

The router. Twenty strategies over a candidate list, the `auto/*` virtual factory, the attempt loop with failover, a per-key resilience layer (circuit breaker), last-known-good provider memory, and the read-only `explain` and `simulate` introspection.

## Configuration

The strategy, combo and target settings come from the `combos` section of `config.yaml`, loaded by `ar-config`. Breaker thresholds are set at construction.

## Extension points

Add a strategy by implementing its sorter in `strategy.rs` and listing it in `Strategy::all`. The attempt loop calls the executor through the `Executor` trait, so a test double needs no network.

## Limitations

An unknown or deferred strategy returns `RouteError::DeferredStrategy`, never a panic. Failover happens before the first byte of a stream; once a stream has started it cannot fail over. Breaker state is per process.

## Model experience

A response carries `x-ar-decision` (strategy, outcome, provider, model, attempts), so a client can see which target served it and whether it failed over. A streamed failure arrives as an in-band `stream_error` frame.
