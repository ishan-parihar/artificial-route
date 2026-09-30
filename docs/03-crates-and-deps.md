# 03 — Crates and deps

Edition 2024, `rust-version 1.90`, pinned `rust-toolchain.toml`. Profiles: `release lto=true codegen-units=1`.
Lint gate (CI only `test+clippy`, per ch.10): `cargo clippy --all-targets --all-features --locked -- -D warnings`.

## Workspace

```toml
[workspace]
members = ["crates/*"]
[workspace.dependencies]
tokio = { version = "1.47", features = ["full"] }
axum = "0.8"  hyper = "1.6"  tower = "0.5"  reqwest = { version = "0.12", features = ["rustls-tls-manual-roots","http2","stream"] }
clap = { version = "4.5", features = ["derive"] }
serde = "1.0"  serde_json = "1.0"  simd-json = "0.14"  serde_yaml = "0.9"  toml = "0.8"
tiktoken-rs = "0.7"  futures = "0.3"  async-stream = "0.3"  tokio-stream = "0.1"  tokio-util = "0.7"
bytes = "1.10"  arc-swap = "1.7"  crossbeam-channel = "0.5"  governor = "0.10"
quick_cache = "0.6"  redb = "2.6"  rusqlite = { version = "0.32", features = ["bundled"] }
aes-gcm = "0.10"  argon2 = "0.5"  zeroize = "1.8"  keyring = "3.6"  jsonwebtoken = "9.3"  uuid = { version = "1.18", features = ["v4"] }
regex-automata = "0.4"  aho-corasick = "1.1"  notify-debouncer-full = "0.3"  shellexpand = "3.1"
tracing = "0.1"  tracing-subscriber = { version = "0.3", features = ["env-filter","json"] }  prometheus = "0.13"
jemallocator = "0.5"  usearch = "2.20"
```

Why each: `tokio/axum/hyper/tower` listener+routing (agentgateway spine). `reqwest rustls` upstream, H2 pooled via `ar-pool`. `clap derive` CLI. `serde*` config+wire; `simd-json` hot chat path only. `tiktoken-rs` exact counting. `governor` RPM leases. `quick_cache+redb` exact cache mem+disk (drop `sqlx`). `rusqlite bundled` ledger only. `aes-gcm+argon2+zeroize+keyring+jsonwebtoken` keys (see 04). `regex-automata+aho` guardrails linear-time. `notify-debouncer-full+shellexpand` hot reload + `$VAR` inject. `usearch` P1 vector only.

Dropped: `onnxruntime`, `tokenizers ML`, `sqlx`, `rmcp`, `tonic/prost`, `cel-fork` (unless CEL rules needed — prefer plain config).

## Module -> crate

`ar-registry` (JSON include), `ar-translate` (thiserror enums, static dispatch generics per ch.6),
`ar-exec` (reqwest + `BufList`), `ar-route` (strategy fns, no dyn unless heterogeneous list),
`ar-tokens` (BPE + estimator), `ar-compress` (pure `&str->String`, `Cow` per ch.1),
`ar-config` (serde + `hotReload`), `ar-cli` (anyhow only here, thiserror in libs per ch.4),
`ar-cache|ar-guard|ar-obs` (see 04).

## Disciplines (rust-best-practices)

* `&str/&[T]` params, `Cow` ambiguous, `Copy` only <=24B. No `.clone()` in loops; `.iter()` + iterators, no intermediate `collect` (ch.1,3).
* `Result<T,E>` + `?`, never `unwrap/expect` outside tests. Libs `thiserror`, binary `anyhow` (ch.4).
* Static dispatch generics on hot path; `dyn Trait` only for heterogeneous policy lists; box at API boundary (ch.6).
* `TypeState` for `Connection<Disconnected|Connected>` — only connected sends (ch.7).
* `//` why, `///` what/how, `#![deny(missing_docs)]` on libs, `TODO(#n)` links (ch.8).
* `Send/Sync` audit on shared `ArcSwap/Strng` (ch.9). `clippy::perf,redundant_clone,large_enum_variant,needless_collect` deny. `#[expect]` with reason, never bare `allow` (ch.2).
* Tests: `verb_should_outcome_when_condition`, one assert, doc-tests for public API, `insta` snapshots for translator goldens (ch.5).
* Benchmarks (ch.10): dev/test/synthetic split, `provisional:` on fitted constants, 3x repeats `criterion+black_box`, record load/CPU/profile/SHA, generator refuses overwrite, never hand-edit reports.
