# Plan: aroute on racknerd behind a public endpoint

Status: **complete** (2026-10-19). Local-only proxy removed; all harnesses use `https://aroute.ishanparihar.com/v1` with the gate JWT. Owner: operator request ("deploy aroute to hermes-vps, update the instance, expose it as aroute.ishanparihar.com, point local harnesses at it"; extended 2026-10-19: "remove the local machine aroute, and only use the remote endpoint for all intents and purposes").

## Facts gathered (read-only)

- Host: `racknerd` (`~/.ssh/config`), x86_64, 2 cores, 2.4 GB RAM, about 840 MB available at 2026-10-18 19:50.
- aroute 0.1.16 already runs there as the user unit `aroute.service` on `127.0.0.1:20128`, from `~/.local/bin/aroute`. Its config is `~/.config/ar/config.yaml`, its env file `~/.config/ar/ar.env` (mode 600). That is the instance to update.
- The gate is **not armed** on the VPS (`doctor`: "no store so no gate — loopback only"). Doctor reports 27 failing checks, mostly OAuth providers with no executor in this build.
- The system cloudflared unit runs `tunnel --config /home/nerd/.cloudflared/hermes-config.yml run` (tunnel `8938b025-…`). Its ingress covers hermes, browser and paperclip. Editing it and restarting the unit needs root; the user has no passwordless sudo.
- `~/.cloudflared/cert.pem` is present, so DNS routing can run as the user.
- Local harnesses point at local `127.0.0.1:20128`: opencode (`~/.config/opencode/opencode.json`), jcode (`~/.jcode/config.toml`, `[providers.omniroute]`, key in `provider-omniroute.env`), omp (`aroute/…` model names).

## Constraints

1. **Gate before ingress.** A public hostname must not reach the proxy until `aroute keys arm-gate` has stored the master. An open listener would expose the provider keys to anyone.
2. **`$AR_MASTER_KEY` first.** The credential store is encrypted with it (`docs/04`, `README` §credentials). It is generated on the VPS and written into `ar.env` without printing it.
3. **No secret output.** The minted client token is written to a mode-600 file, copied to the local machine, and never printed. `ar.env` is never sourced or printed; commands run with `systemd-run -p EnvironmentFile=`.
4. **Pushed commit only.** The VPS binary is the release build of the pushed commit `5677625`.
5. **Root steps are the operator's.** `sudo systemctl restart cloudflared` is run by the user.

## Steps

| # | Where | Action | Check |
|---|---|---|---|
| 1 | VPS | **Done** (static musl build; the host glibc build failed with `GLIBC_2.39 not found`). Back up `aroute` to `aroute.pre-0.1.16.bak`; stage the new binary as `aroute.new`; check sha256; `mv` it into place | `aroute --version` shows the new build |
| 2 | VPS | Generate `AR_MASTER_KEY` (32 bytes hex) into `ar.env` if absent | `grep -c ^AR_MASTER_KEY=` is 1; value never printed |
| 3 | VPS | Restart `aroute.service`; `healthz` on 127.0.0.1:20128 | `ok` |
| 4 | VPS | `aroute keys arm-gate --key-id racknerd-harness` run with the service environment, output to `~/.config/ar/gate-token.txt` (mode 600) | file exists, mode 600 |
| 5 | VPS | Restart the service so it reads the master from the store; `doctor` reports the gate armed | `gate` check `ok` |
| 6 | VPS | **Done**: `/healthz` 200; `/v1/models`, `/v1/models/{id}`, `/metrics` 401 without or with a bad token, 200 with the token. Earlier the catalog and metrics were open; fixed in `663e78d`. Unauthenticated `/v1/models` on localhost → 401; with the token → 200 | status codes only |
| 7 | Local | **Done**: `~/.config/ar/racknerd-gate-token`, mode 600. `scp` the token file to `~/.config/ar/racknerd-gate-token` (mode 600) | file exists |
| 8 | VPS | **Done** (validated; `Matched rule #3`). Backup `hermes-config.yml.bak-aroute-20261018`. Add `aroute.ishanparihar.com` → `http://127.0.0.1:20128` before the catch-all in `hermes-config.yml` (backup first); `cloudflared tunnel ingress validate` | validate passes |
| 9 | VPS | **Done** (CNAME added). `cloudflared tunnel --origincert ~/.cloudflared/cert.pem route dns 8938b025-… aroute.ishanparihar.com` | DNS record created |
| 10 | Operator | **Done** (evidenced 2026-10-19: public hostname routes to the proxy; cloudflared active). Needed root: `sudo systemctl restart cloudflared` | unit active |
| 11 | Local | **Done** (2026-10-19): opencode `ar` provider → remote URL + JWT (lane `free-stack`→`large-stack`, dead `nvidia/z-ai/glm-5.3` passthrough lane dropped); omp `aroute` → remote URL + JWT (`free-stack`→`large-stack`, stale `paper-stack` lane dropped; `config.yml` roles `advisor`/`task` → `aroute/large-stack`); jcode `omniroute` → remote URL + `provider-omniroute.env` holds the JWT (lane renamed). Backups: `*.bak-pre-remote-20261019` beside each config | all four harness configs parse; no live config references :20128 |
| 12 | Both | **Done** (2026-10-18/19): Public `healthz` 200; `/v1/models` 401 anonymous / 200 with JWT; `large-stack` non-stream chat returns correct content (2026-10-19 after rename) | status codes + content |
| 13 | Local | **Done** (2026-10-19): local `aroute.service`, `aroute.update.timer`, `aroute-dashboard.service`, `omniroute`, `omniroute-disk-guard.timer` stopped + disabled. No local proxy port listens. Binary `~/.local/bin/aroute` kept as CLI/MCP + rollback path; local config + `usage.sqlite` kept as archive | `systemctl --user is-active` all inactive; `ss` shows no :20128/:20129/:20149 |
| 14 | Both | **Done** (2026-10-19): post-decommission remote smoke — healthz 200, `/v1/models` 200 with JWT, `large-stack` non-stream serves `stepfun-ai/step-5-preview` (correct answer), `large-stack`+`small-stack` SSE streams 32/6 frames | status codes + frames |

## Rollback

- Binary: `mv ~/.local/bin/aroute.pre-0.1.16.bak ~/.local/bin/aroute` and restart.
- Gate: remove the store row or `ar.env` master, restart. The loopback listener is unchanged.
- Ingress: restore `hermes-config.yml` from its backup and restart cloudflared.
- Harnesses: restore the saved configs (backups are made before each edit).

## Harness targets (to change in step 11)

- opencode `~/.config/opencode/opencode.json`, provider `ar`: `baseURL` → `https://aroute.ishanparihar.com/v1`, `apiKey` → gate JWT. Rename lane `free-stack` → `large-stack`; drop `nvidia/z-ai/glm-5.3` passthrough lane.
- omp `~/.omp/agent/models.yml`, provider `aroute`: `baseUrl` → remote (currently `dummy` key, which only worked with the gate open). Rename lane `free-stack` → `large-stack`; drop stale `paper-stack` lane (undefined on both sides).
- jcode `~/.jcode/config.toml`, `[providers.omniroute]`: `base_url` → remote; its env file holds the key. Rename lanes.

## Combo rename + repair (2026-10-19, remote-only migration)

- `free-stack` → `large-stack` on VPS config first, then local config + all harness lanes. Clean cutover: no alias, old id 404s.
- `large-stack` targets (v3, 2026-10-19, operator cut ultra-550b): priority chain `nvidia/stepfun-ai/step-5-preview` (Artificial Analysis Intelligence Index 43.7 — top open-weight reachable on any held key; correct, 1–9s, needs max_tokens ≥ ~500 or content is null) → `kilocode/reflection/beam-free` (AA index 30.3, verified, ~1s) → `kilocode/nvidia/nemotron-3.5-super-vl:free` (AA index 26.5, already proven via small-stack). Nothing else stays in the chain by operator ruling — no substitutes below this bar.
- Why the frontier is out of reach for now (2026-10-19 cross-ref of the AA leaderboard vs every held key): kilocode/cline frontier ids (claude-opus-5, gpt-5.6-sol, gemini-4-argon, mimo-v2.6-pro) are credits-gated ($0 balance); the 15 VPS-extra providers (ghe-copilot, github, gitlab-duo, antigravity, kiro, devin-cli/desktop, trae, xai-oauth, zed-hosted, kimi-coding, clinepass, agy, openference, codebuddy-cn) carry `dummy` keys — their OAuth tokens reject live (xai invalid, kimi expired, clinepass unauthorized); GLM-5.3-Max (44.8) and Kimi-K3 (43.6) outrank step-5 but are dead on every reachable lane (429 on all nvidia NIM keys, no `:free` variant, 410 EOL on nvidia). Upgrade path: fund kilocode credits, or re-auth one OAuth provider on the VPS.
- Removed: `nvidia/z-ai/glm-5.3` (429 dead on all keys), `nvidia/z-ai/glm-5.3-flash` (TTFT 99–124s, crosses 110s stream-start cap).
- `browser-stack-2` still targets dead `nvidia/z-ai/glm-5.3` — left as-is (out of scope; harness lanes don't use it as default).

## Known caveats

- `free-stack` returned 504 "produced no response within 150s" on the local swap check. The VPS has the same combo, so a public smoke test may time out for the same reason. Use a combo that answers if that happens. **Resolved 2026-10-19**: `free-stack` was renamed `large-stack` and repointed at live targets (see above); the broken nvidia GLM targets are gone.
- **Token rotation**: the gate JWT expires 2026-11-17 (`exp` in the JWT). Rotate with `aroute keys mint --key-id racknerd-harness` on the VPS (service env via `systemd-run -p EnvironmentFile=$HOME/.config/ar/ar.env`), then copy the new token into the three harness configs (`opencode.json` `provider.ar.options.apiKey`, `models.yml` `aroute.apiKey`, `~/.jcode/provider-omniroute.env`) and `~/.config/ar/racknerd-gate-token.jwt`.
- **`scripts/sync-harness-lanes.py` removed** (2026-10-19): it pointed harness lanes at the local :20128 gateway, which no longer exists; running it would have undone the migration.
- The public name returns `403 error code: 1010` to the default Python user agent; probes must set a `User-Agent`.
- The VPS doctor failures are OAuth providers with no executor in this build. They are not caused by this change.
- Standing constraint "do not touch hermes-vps" is lifted for this plan by the operator's explicit request.
