# Contributing

Read `AGENTS.md` first — it holds the enforced Rust disciplines. Then
`docs/00-overview.md` through `docs/06-axi-mcp.md` before changing anything.

## Setup

```sh
cargo build -p ar-cli
./target/debug/ar doctor
```

## Gates (both must pass)

```sh
cargo test --release
cargo clippy --all-targets --all-features --locked -- -D warnings
```

No `unwrap` outside tests, no type-error suppression, minimal diffs.
`Cargo.lock` is committed (binary ships to users; reproducible builds).
