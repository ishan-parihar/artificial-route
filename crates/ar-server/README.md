# ar-server

## Purpose

The axum 0.8 spine. Four inbound dialects (`/v1/chat/completions`, `/v1/messages`, `/v1/responses`, `/api/chat`) share one routing pipeline, with `/v1/models`, `/healthz` and `/metrics` alongside. Three tower layers wrap it.

## Configuration

`ServerConfig` is built from `ar-config`. `Components` takes the executor and optional overrides such as `cache_bytes` and `extra_models`. Tests use `Components::with_exec` to inject a fake upstream.

## Extension points

Add a dialect by adding an inbound translator in `ar-translate` and a route in `routes.rs`. Add a provider path with `ArExec`, which `HttpExec` implements in production.

## Limitations

The request timeout bounds the request, not the response body, so a long generation can outlive it. A stop drains for `DRAIN_WINDOW` and then drops the rest, which an in-band `stream_error` frame reports to the client. Caching applies to non-streaming requests only.

## Model experience

A client sees the OpenAI, Anthropic, Responses or Ollama shape it asked for. Errors are JSON bodies with the upstream reason, and an unknown path returns a JSON 404 that names the path.
