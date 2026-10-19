# Skill redundancy audit — 2026-10-19

**Scope:** the managed skill library (`~/.omp/agent/managed-skills`, ~660
skills) and the user library (`~/.agents/skills`). The question asked: which
skills are redundant, how should fragmented coverage be consolidated, and
where does the arsenal have implementation gaps.

**Method:** the house procedure from `skill-inventory-before-merge` —
enumerate, read the frontmatter, then compare what each candidate *covers*
(not what it is named). A pair was a consolidation candidate only when the
description and the section list showed the same job.

## Headline

The library is large but the **aroute and harness clusters were the fragmented
ones**: 27 aroute-named skills, of which 10 covered four jobs between them.
After consolidation the cluster is **17 skills**, and the four job-shaped
hosts are:

| host | owns |
|---|---|
| `aroute-ops` | operating the proxy: topology, the hard rules, deploy, the gate, provider credentials, combo editing, diagnosis |
| `aroute-release` | shipping a release: gates, version bump, tag, CI, artifact verification, deploy, rollback |
| `aroute-provider-audit` | probing providers and surgery on combos: error taxonomy, the liveness probe, leaderboard cross-reference, config edits |
| `aroute-test-harness` | running and probing locally without touching prod: fixture-dir execution, a supervised second instance, honest verdicts, test/gate traps |

## What was consolidated (10 skills removed, no fact lost)

| removed | into | what it contributed |
|---|---|---|
| `aroute-deploy-and-probe` | `aroute-ops` + `aroute-test-harness` | ETXTBSY on `cp`; clippy/test do **not** emit a binary; supervised second instance; the `pkill` self-match trap; `EMPTY` at `max_tokens: 16` is a starved budget |
| `aroute-vps-deploy` | `aroute-ops` + `aroute-release` | the "Never" list; sha256 **string** comparison vs `sha256sum -c`; never `\|\| echo abort`; a card is not proof of dispatch |
| `aroute-deploy-verify` | `aroute-release` | the same release-asset flow, shorter |
| `aroute-provider-probe` | `aroute-provider-audit` | config-edit discipline; the byte-sync warning about the VPS's extra provider rows |
| `aroute-model-liveness-probe` | `aroute-provider-audit` | the sheep-question correctness gate; `max_tokens >= 800` for reasoners; the latency-vs-cap gate; nvext field discovery; the typed-body gotcha |
| `aroute-model-recon` | `aroute-provider-audit` | the nvext knob; deepseek's ~250s; order the stack by leaderboard **among models that answered** |
| `artificial-route-gate` | `aroute-release` | the gate's blind spots (`unwrap` outside tests; hand-typed counts; advisory races) |
| `artificial-route-gate-cycle` | `aroute-test-harness` | fixture-dir execution; undrained response bodies; frozen goldens; the OmniRoute cache-ttl grammar; the prelude rule; the fixtures-depth git trap |
| `ar-proxy-service-and-clients` | `aroute-ops` | the client contract (rewritten to today's six harnesses) |
| `artificial-route-environment-and-routing-facts` | `aroute-provider-audit` | the routers rewrite the model; the moderation classifier that answers "User Safety: safe"; breaker state is not a provider verdict |

Each host carries a `### Consolidated from …` trail, matching the pattern
`aroute-release` already used.

## Stale content the audit found and fixed

`ar-proxy-service-and-clients` listed **`ar` and `omO`** as clients and
`free-stack` as a `least-used` combo. All three were wrong by 2026-10-19: the
binary is `aroute`, the clients are omp/opencode/jcode/codex/pi/agy, and
`free-stack` was renamed `large-stack` and repointed at quality models. A
skill that names a client that no longer exists is worse than no skill, so the
table was rewritten from the live configs rather than from memory.

## Implementation gaps found and closed

1. **The tools were orphaned in the filesystem.** `harness-sync` and
   `gate-token.sh` existed only in `~/.local/bin`, so a remote host could not
   get them. They now live in the repository (`scripts/`) and the registry
   lives in `config/harness/mcp-servers.yaml`; the local registry is a symlink
   to the repo copy. The installed copy follows the repo copy.
2. **`harness-sync` broke codex.** It wrote `args = "mcp"` (a string) where
   codex wants a sequence — codex refused to start with *"invalid type:
   string, expected a sequence"*. The renderer now writes `args = ["mcp"]`.
3. **pi and agy were outside the pool.** pi reads `~/.pi/agent/mcp.json`
   (omp's shape); agy is the antigravity family and reads
   `~/.gemini/config/mcp_config.json` with a **`serverUrl`** field. Both are
   now rendered by the tool, and both were verified to load their servers.
4. **A correct URL in a disabled server is still a dead server.** omp's
   `browseros-neo` entry had the right port but the server was not in
   `enabledServers`, so every omp start printed `ECONNREFUSED`.
5. **The check reported project-scoped servers as drift.** jcode's `serena`
   lives under `projects.<path>.mcpServers`; the check now excludes them.

## What is still fragmented (not consolidated, deliberately)

- `aroute-stack-change` and `aroute-combo-trim-role-check` overlap in the
  combo-editing area, but the second is a *diagnostic* for role mis-pointing
  and the first is the *edit procedure*. Kept separate; the audit found no
  shared fact to move.
- `aroute-service-restart-diagnosis` (15 lines) is a lookup table that
  `aroute-ops`' diagnosis table already covers. Merging it would leave the
  hosts larger than the facts; it is the next candidate when either is edited.
- `aroute-side-channel-model-test` and `aroute-stream-attribution-recipe` are
  narrow and distinct (testing an upstream without touching the live build;
  attributing a stream to a serving model).
- `artificial-route-long-runs` (188 lines) is the soak harness and is a
  genuine specialist.

## The bigger gap: the library's shape

660 managed skills is a lot of surface for one operator. The redundancy that
survives this audit is mostly **cross-project** (a dozen `falsif*` and
`prove-*` skills that say the same discipline in different words). That is a
different consolidation — semantic, not structural — and it is not what was
asked here. The structural duplication in the aroute/harness area, which is
what actually bit today, is now closed.

## Verification

- `harness-sync check` exits 0: every harness matches the registry and every
  enabled server answers.
- `pi mcp list` shows 22 browser tools connected; `agy mcp list` and
  `jcode mcp list` show their five; codex's toml parses and its
  `mcp_servers` entries are sequences.
- The browseros-neo log records `omp-coding-agent` and `codex-mcp-client`
  sessions, so both harnesses really did connect.
