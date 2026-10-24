# 25 — The CLI as an agent interface: red-team audit (2026-10-24)

**Status:** audit complete, findings verified against the running system.
**Follow-ups:** [G1](#g1-mcp-is-compiled-out-of-the-shipped-binary) blocks the entire agent story and is the highest-value fix in this document.

## Scope

Two questions, asked against the *installed* `aroute 0.1.21` on this host:

1. what resources does the running aroute system actually cost?
2. how good is the CLI as an interface for AI agents driving it through tool calls?

Everything below was measured on the live system, not inferred from source. Where a claim
could not be tested without spending money (live completion traffic) that is stated.

## 1. What aroute is

Two systemd **user** units, plus the children they spawn:

| Unit | Binary | Ports |
|---|---|---|
| `aroute.service` | `aroute … serve` (Rust, pid 1103) | 20128 (the `/v1` proxy API) |
| `aroute-dashboard.service` | `aroute dashboard --port 20149` (a ~4 MB launcher) | 20149 (UI) |
| └─ its child | `omniroute (v16.3.8)` (Node, the compiled dist) | 20131, 20132 (internal WS) |

So the thing an operator thinks of as "the dashboard" is **two processes and four ports**,
and the Rust binary is only a thin supervisor for the Node child.

## 2. Resource reality (measured)

| Component | RSS | Peak (VmHWM) | Threads | Idle CPU |
|---|---|---|---|---|
| aroute proxy (Rust) | 44 MB | 160 MB | 26 | 0.3% |
| dashboard launcher (Rust) | 4 MB | 6 MB | 1 | 0.0% |
| dashboard child (Node) | 873 MB | 959 MB | 13 | 105% (see §3) |

On disk: dist 1.6 GB, data dir 49 MB, 194 call-log artifacts.

The Node child's footprint is dominated by V8 heap plus model-sync state; the unit's own
comment says "roughly 500 MB RSS" but the observed peak is ~960 MB, so **the unit comment
understates it by ~2x** and should be corrected.

### The operational incident found during this audit

The dashboard was **alive but useless**: `/healthz` returned HTTP 200 after **15,427 ms**
while its own journal said `event loop stalled 45.6s` and
`GET /healthz event-loop lag 91738ms (HTTP 200 is not healthy; busy != ready)`.

CPU attribution was the decisive clue: **99% kernel time** (utime 131 ticks vs stime 65,941)
with 501,889 major page faults. That is not a JS hot loop; it is the process thrashing
while the host sat at ~5.2 GB available memory (the other tenants were two 12 GB python3
jobs). `healthz` returning 200 is therefore actively misleading — a caller polling it
concludes "healthy" from a process that cannot serve a request in under a minute.

A `systemctl --user disable --now` + `enable --now` cycle restored it to **23 ms** with
artifact writing resumed, so the fix is operational (restart), not code-level. The remaining
question — why the Node child climbs to ~1 GB and 105% CPU while ModelSync runs — is
**unresolved** and is [G5](#g5-unresolved-why-the-node-child-spins).

## 3. Enable/disable the dashboard

### There is no `aroute` verb for it — and this is a real agent-facing gap

All 15 subcommands: `serve, models, providers, combo, doctor, run, configure, auth, keys,
limits, import, sync, dashboard, donate` (+ hidden `mcp`). **None of them is
`start|stop|restart|enable|disable|status`.** `aroute dashboard` *launches* a foreground
server; it cannot stop the running one, and it cannot report whether one is up.

The only paths that exist:

```sh
systemctl --user status aroute-dashboard      # observe
systemctl --user disable --now aroute-dashboard  # stop + no autostart  (reclaims ~960 MB)
systemctl --user enable  --now aroute-dashboard  # autostart + run
systemctl --user restart aroute-dashboard       # the fix for the stall above
```

Verified: `disable --now` freed the port, killed the process, and lifted host available
memory from ~5.2 GB to ~17.5 GB. `enable --now` restored it, serving in 7 s.

Full removal is `install.sh --uninstall` (binary, unit, dist; config and credentials stay).

### Auto-start registration

```
~/.config/systemd/user/aroute.service
~/.config/systemd/user/aroute-dashboard.service   # wanted by default.target
```

Note `aroute.service:20` emits `Unknown key 'StartLimitIntervalSec' in section [Service]`
on every boot — a silent unit misconfiguration that should be corrected to
`StartLimitIntervalSec=` under `[Unit]`.

## 4. Red-team: the CLI as an agent interface

Verdict: **the design is agent-aware but the shipped binary is not agent-capable.** The
source contains an MCP server and a TOON emitter; the installed binary ships neither.

### G1 — MCP is compiled out of the shipped binary (blocker)

`crates/ar-mcp/` exists and is substantial (tools, scope, audit, login, transport). The
subcommand is wired and documented:

```rust
#[cfg(feature = "mcp")]
mod mcp;                      // crates/ar-cli/src/main.rs:19
```

and `crates/ar-cli/Cargo.toml` has `default = []` with `mcp = ["dep:ar-mcp", "ar-mcp/mcp"]`.
So a default build **excludes MCP**, and the installed binary agrees (verified: it contains zero
`rmcp` symbols):

```
$ aroute mcp
error: unrecognized subcommand 'mcp'
```

Meanwhile `install.sh` already *tells users to install it* (`claude mcp add aroute …`,
`codex mcp add aroute …`). The advertised agent path does not exist in the shipped binary.

**Correction made during implementation (2026-10-24, later the same day):** my first
reading of this gap — "make MCP the default feature" — was wrong. The gate is a
*documented, deliberate invariant*, not an oversight:

> Accept: default `cargo tree` has no `rmcp`; `+mcp` idle +<25MB; agent
> `health->switch->route->cost` loop green.  — `docs/06-axi-mcp.md`

The `ar-mcp` manifest says the same: "a default `cargo tree -e no-dev` must contain no
`rmcp`", and it deliberately keeps the tool bodies, scope checks and audit trail compiling
with no SDK present so they stay testable in-process. Flipping the default would publish
the protocol SDK to every user to serve a feature most do not use, and would break that
accept gate.

The real defect is narrower and was verified: **no build path in the repo enables the
feature.** `release.yml` runs a bare `cargo build --release --locked … --bin aroute` with
no `--features`, and `install.sh` has no cargo invocation at all, so the advertised
`claude mcp add aroute` path can never work from a published artifact. The fix is to build
the MCP capability into the release artifact while keeping the *default* `cargo tree`
clean — i.e. enable the feature at the workspace/release level (or ship a second
`aroute-mcp` entry point), and keep the accept gate green.

The crate also carries a **guarded write surface** (a scope model with `denied` outcomes
recorded in an append-only `redb` audit trail, and a `--list` catalog mode). That is exactly
the right posture for an agent-facing tool, and it is being thrown away by the default build.

### G2 — no machine-readable output on any non-MCP verb

No `--json`, no `--format`. Output is CSV-ish tables:

```
count: 266 of 266 total
providers[266]{id,provider,status}:
  adapta-web,openai,ok
```

Token cost of one full call, measured with the real config and env loaded:

| Verb | Bytes | ~Tokens |
|---|---|---|
| `models` | 72,913 | **18,228** |
| `configure` | 31,660 | 7,915 |
| `doctor` | 31,446 | 7,861 |
| `providers` | 6,028 | 1,507 |
| `combo` | 638 | 159 |

`aroute models` costs 18k tokens to answer "what can I call?", and an agent must pay it in
full. There is a TOON module in the source (`crates/ar-cli/src/toon.rs`) — see G1/G4.

### G3 — no filtering, so every question costs the full table

All of these are rejected: `--name`, `--status`, `--limit`, `--only`, `--filter`.

```
$ aroute providers --name openai
error: unexpected argument '--name' found
```

Rejecting unknown flags is *correct* per AXI §6, but the consequence is that an agent that
wants one row must fetch 6 KB and `grep` it client-side. The only lever is `--fields`
(verified working: `models --fields model` = 27 tokens vs 18,228), which trims columns but
cannot subset **rows**.

### G4 — output shape is neither AXI-TOON nor JSON

`toon.rs` exists in `ar-cli`, so the intended output format is TOON (AXI §1), but the
installed binary emits the `count:` + `name[n]{cols}:` header style. That style is
TOON-*like* and readable, but without the escape rules (quoting, embedded commas/newlines)
it is ambiguous for a parser: a field containing a comma silently shifts columns. Provider
ids and model names are low-risk today, but `run` output and error strings are not.

### G5 — unresolved: why the Node child spins
See §2. ModelSync runs continuously against 88 connections; several providers return 502
and one returns `api_key_expired`, so there is a plausible retry loop, but the app log had
stopped advancing entirely, so this audit could **not** attribute the CPU to a specific
module. It needs a targeted look with instrumentation, not inference.

### Where the CLI is genuinely good (keep these)

Verified, and worth protecting:

- **Idempotent, read-only verbs** (`models`, `providers`, `combo`, `configure`, `doctor`)
  are safe for an agent to call freely.
- **Exit codes are correct**: a missing/unexpandable config exits **1**, not 0. This is the
  single most important scripting property and it holds.
- **Errors are structured and self-correcting**: `error: …` + `help: …` naming the next
  command, per AXI §6.
- **Unknown flags fail loudly** rather than being silently dropped.
- **`--fields` exists** and works, which is the AXI §2 escape hatch already present.
- **`aroute doctor` exits 1 on any failure** — a usable agent health check.

### Untested (and why)

`run`, `keys`, `limits`, `import`, `sync` were assessed from `--help` only. `run` would spend
real provider tokens; the mutating verbs would change live routing config. Their agent
ergonomics are therefore **unverified**, and the audit should not be read as covering them.

## 5. Consequence for the agentic-utility workflows

The workflows that need the CLI are: enumerate providers/models, route a request, inspect
cost, inspect credentials, audit who called what. Today an agent must:

1. shell out to `aroute` and pay 18k tokens for `models`, or
2. shell out to `systemctl` to manage the dashboard, and
3. parse CSV-ish output with regex, with no quoting guarantees.

Each is a round-trip the AXI contract exists to prevent.

## 6. Prioritised fixes

| # | Fix | Why first |
|---|---|---|
| G1 | Ship MCP: build/install with `--features mcp`; make it the default so the documented agent path exists | unblocks every other item |
| G2 | `--json` on every read verb, TOON as the human default | removes the 18k-token wall |
| G3 | `--limit` + at least one row filter per list verb | makes "one question" cheap |
| G4 | Correct TOON escaping, or emit real TOON from `toon.rs` | prevents silent mis-parsing |
| G5 | Attribute and fix the Node child's CPU/RSS climb | fixes the 15 s healthz stall |
| G6 | `aroute service {status,start,stop,restart,enable,disable}` wrapping systemd | closes the two-tool gap |
| G7 | Fix `StartLimitIntervalSec` placement; correct the "500 MB" unit comment | stops misleading operators |
