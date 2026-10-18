# Plan: aroute on racknerd behind a public endpoint

Status: deployed to the VPS and gated; the tunnel restart and harness switch are pending (2026-10-18). Owner: operator request ("deploy aroute to hermes-vps, update the instance, expose it as aroute.ishanparihar.com, point local harnesses at it").

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
| 10 | Operator | **Pending.** Needs root: `sudo systemctl restart cloudflared` | unit active |
| 11 | Local | **Pending**; applied only after step 12 passes. Point opencode, jcode and omp at `https://aroute.ishanparihar.com/v1`, token from `racknerd-gate-token` | harness smoke test |
| 12 | Both | Public `healthz`, authenticated `/v1/models`, one small non-stream chat | 200 and content |

## Rollback

- Binary: `mv ~/.local/bin/aroute.pre-0.1.16.bak ~/.local/bin/aroute` and restart.
- Gate: remove the store row or `ar.env` master, restart. The loopback listener is unchanged.
- Ingress: restore `hermes-config.yml` from its backup and restart cloudflared.
- Harnesses: restore the saved configs (backups are made before each edit).

## Harness targets (to change in step 11)

- opencode `~/.config/opencode/opencode.json`, provider `ar`: `baseURL` and `apiKey`.
- omp `~/.omp/agent/models.yml`, provider `aroute`: `baseUrl` (currently `dummy` key, which only worked with the gate open).
- jcode `~/.jcode/config.toml`, `[providers.omniroute]`: `base_url`; its env file holds the key.

## Known caveats

- `free-stack` returned 504 "produced no response within 150s" on the local swap check. The VPS has the same combo, so a public smoke test may time out for the same reason. Use a combo that answers if that happens.
- The public name returns `403 error code: 1010` to the default Python user agent; probes must set a `User-Agent`.
- The VPS doctor failures are OAuth providers with no executor in this build. They are not caused by this change.
- Standing constraint "do not touch hermes-vps" is lifted for this plan by the operator's explicit request.
