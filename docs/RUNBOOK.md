# Runbook — exact commands, environment traps

| task | command | traps |
|---|---|---|

## Deploying to the racknerd VPS (`racknerd`, Debian 12, glibc 2.36)

- **Build the static musl binary, never the host build.** The local release build links
  glibc 2.39 and fails on the VPS with `GLIBC_2.39 not found`. Use the release recipe:
  `cargo build --release --locked --target x86_64-unknown-linux-musl --bin aroute`, then
  `strip`. Stage it as `~/.local/bin/aroute.new`, run `./aroute.new --version` before the `mv`.
- **Install from the GitHub release on the VPS** (v0.1.17 flow): download the
  `aroute-x86_64-unknown-linux-musl` asset, verify `sha256sum` against the published
  `SHA256SUMS` **by comparing the two hash strings directly** — `sha256sum -c` fails on
  filename-path mismatches, and a `|| echo abort` guard swallows the failure and lets the
  swap proceed anyway (both bit this deploy; the post-swap hash comparison proved the
  binary authentic). Keep the previous binary as `aroute.prev-<oldver>` for rollback.
- **Never copy `ar.env` between machines.** It carries the credential-store master (`AR_MASTER_KEY`) and the gate master (`AR_HTTP_MASTER_KEY`); a file copied from a box without them (or with different ones) makes the store undecryptable and every minted token invalid — the symptom is a 401 `credential_rejected` on every request. The VPS's `ar.env` is the only copy of its masters. To add a *credential* (a provider key), append that one line to the VPS's file over ssh; never overwrite the file.
- **The gate has one master, three names.** `aroute serve` verifies with `AR_HTTP_MASTER_KEY`; `aroute keys mint` signs with the `http-gate` row in the credential store; the store row is encrypted under `AR_MASTER_KEY`. All three must be the same 32 bytes. `aroute keys arm-gate` **without** `--key` generates a *fresh random* master, which silently desynchronises the server from the store — every token then fails to verify. The correct sequence after any master change:
  ```sh
  NEW=$(python3 -c "import secrets;print(secrets.token_hex(32))")
  sed -i "s|^AR_HTTP_MASTER_KEY=.*|AR_HTTP_MASTER_KEY=$NEW|" ~/.config/ar/ar.env
  sed -i "s|^AR_MASTER_KEY=.*|AR_MASTER_KEY=$NEW|" ~/.config/ar/ar.env
  set -a; . ~/.config/ar/ar.env; set +a
  aroute keys arm-gate --key-id racknerd-harness --key "$NEW"
  systemctl --user restart aroute
  aroute keys mint --key-id racknerd-harness   # the new token
  ```
  Then copy the token into the three harness configs (opencode `provider.ar.options.apiKey`, omp `models.yml` `providers.aroute.apiKey`, `~/.jcode/provider-omniroute.env`) and `~/.config/ar/racknerd-gate-token.jwt`.
- **Arm the gate before any public hostname.** An unarmed listener is open to the internet
  once a tunnel reaches it. `aroute keys arm-gate` needs `$AR_MASTER_KEY` in `ar.env`
  (it encrypts the credential store). Run aroute commands with
  `systemd-run --user --pipe --wait -p EnvironmentFile=$HOME/.config/ar/ar.env`, not by
  sourcing the file. Write the minted token to a mode-600 file.
- **Tunnel ingress rewrites Host.** aroute refuses a non-loopback `Host` (`loopback_only`).
  Set `originRequest.httpHostHeader: 127.0.0.1:<port>` on the ingress rule; the gate is the
  auth. Keep the bind on `127.0.0.1`; `public` is not needed.
- **Cloudflare 1010.** The zone blocks the default `Python-urllib` user agent. Probes need an
  explicit `User-Agent`; harness clients send their own and are not blocked.
- **cloudflared reloads only on restart.** `sudo systemctl restart cloudflared` after any
  ingress change; `cloudflared tunnel --config <file> ingress validate` first.

