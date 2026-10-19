# TODO — the register

One row per work item; rows update in place, verdicts append — never delete.
`genome status <id> <state> --evidence <path>` is the supervised transition.

| id | status | item / verdict | notes |
|---|---|---|---|
| UI-00 | open | **U0 — close the unauthenticated `/ui` write path** (AR-2 ingress 404 + AR-3 write handlers refuse on an unreadable store + the credential-store fix) | blocking; the public write endpoints are reachable with no credential today. `docs/27` §2, `docs/plans/2026-10-19-ui-dashboard-upgrade.md` U0. AR-2 needs a root `systemctl restart cloudflared` (operator's step) |
| UI-01 | open | **U1 — gate `/ui` on the HTTP master** so the panel can be published deliberately (AR-1: bearer gate + paste-a-token field + `?token=` first paint) | needs a release; depends on UI-00 landing first |
| UI-02 | open | **Fix the accent contrast failure** — `--amber #e0900f` as text on `--panel #ffffff` is 2.6:1; `.star.on` and `.minted summary` are the two users | add an `--amber-ink` for the light band (`#8a5a00` = 5.9:1 on white). `docs/27` §4.4, audit F6. Cheap; should not wait for the craft pass |
| UI-03 | open | **U2 — honest states**: four data states per view, errors that persist until dismissed, a write that re-reads and compares | `docs/27` §6 |
| UI-04 | open | **U3 — the operator's missing views**: resilience (breaker/cooldown), provider health/expiry, per-combo metrics, budgets + quota windows | `docs/27` §6 |
| UI-05 | open | **U4 — a settings pane over the yaml** (server, limits.default, timeouts, discovery, auth mode) | byte-exact splices; a wrong value refused naming the field |
| UI-06 | open | **U5 — craft**: h1 to 28–32px, table buttons to 44px, footer paths get a title + copy button, a reviewed light band | CRAFT is 12/20 today; the bar is 16/20 with no zero. `docs/27` §4.3 |
| UI-07 | open | **Fix the doctor gate check** — it reports `gate,ok,armed` from the row's *name* while the row is undecryptable; a store that is present but unreadable must make the check FAIL | `crates/ar-cli/src/commands.rs:1086-1088` says so in its own comment. Audit F3 |
| UI-08 | open | **Rule the canonical credential store on the VPS** and delete the other | `~/.config/ar/credentials.db` (what `serve` opens) vs `~/credentials.db` (what a shell in `$HOME` opens) — desynced today. `docs/27` §3, §7.2 |
