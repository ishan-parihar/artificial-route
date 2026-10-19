---
ID: "RG-001"
Title: "the /ui panel is published without a credential of its own"
Status: "Active"
Date: "2026-10-19"
Description: "The /ui router carries no credential check; its only guard is loopback_only, a DNS-rebinding defence. When the tunnel published aroute.ishanparihar.com, that made every /ui write endpoint reachable from the internet with no token: POST /ui/api/combos rewrote the production config, and with the gate row readable POST /ui/api/keys/mint would issue a token the operator's provider keys honour. The same middleware also refuses exactly the requests a browser makes from a public URL (it forwards Origin while rewriting Host), so the panel was half dead and half open at the same time. The regression is the class: a trust boundary inherited from a bind address does not survive a reverse proxy."
Severity: "High"
---

# RG-001: the /ui panel is published without a credential of its own

Immutable once numbered: a correction is a new record that supersedes this one.

## Symptom

On 2026-10-19, from outside the VPS:

- `POST https://aroute.ishanparihar.com/ui/api/combos` with the config's own combo list
  returned `{"ok":true,"pending_restart":true}` and **rewrote
  `~/.config/ar/config.yaml`** on the production host. 39 lines changed (the YAML renderer
  emits block-style lists where the hand-written file used flow-style). No credential was
  presented. The request carried no `Origin` header, which is why it was admitted.
- `POST /ui/api/provider-remove` and `POST /ui/api/limits` both reached their handlers and
  returned their own validation errors (409, 400) — the handlers ran.
- `POST /ui/api/keys/mint` returned `409 "the gate is not armed"`. That refusal came from
  an **unreadable store row** (see `docs/27` §3), not from any access control. With the row
  readable, the endpoint would have issued a token that `/v1` accepts — the operator's
  provider keys, spendable by the caller.

At the same time, the same page in a browser was half dead: both font faces 403
(`font-fallback` FAIL, 6 FAIL across 3 widths in `probe.mjs`) and every write button
403 `non_loopback_request`, because a browser attaches `Origin` to a cross-origin `fetch()`
and to a CSS-driven font request, and the tunnel forwards `Origin` while rewriting `Host`.

**The panel was read-only in public and fully open to a script.**

## Root cause

`crates/ar-cli/src/webui.rs:14-17` states the design: "whoever can reach the socket can
already call the proxy. No second auth layer, for the same reason P0 has none." The `/ui`
router's only layer is `ar_server::app::loopback_only`
(`crates/ar-cli/src/webui.rs:145-146`), and that middleware refuses a request whose `Host`
**or** `Origin` names a non-loopback authority
(`crates/ar-server/src/app.rs:680-701`).

The reasoning was sound for a loopback bind and stopped being sound when the proxy was
published through a cloudflared tunnel (`~/.cloudflared/hermes-config.yml`, ingress
`aroute.ishanparihar.com -> http://127.0.0.1:20128` with `httpHostHeader: 127.0.0.1:20128`).
The tunnel rewrites `Host` to loopback — so the rebinding check's `Host` half passes — and
forwards `Origin` untouched, so its `Origin` half fires for exactly the requests a browser
makes and for none of the requests a script makes.

Two failures, one cause: **a trust boundary inherited from a bind address does not survive
a reverse proxy.** The bind is no longer the boundary; the socket is reachable from the
internet, and the boundary has to be a credential.

The HTTP gate that does exist is applied by `routes::authorize`
(`crates/ar-server/src/routes.rs:2193`) from the chat and media routes. It is not applied
to `/ui`, because `/ui` is merged into the router *after* `app()` has run
(`crates/ar-cli/src/webui.rs:145-146`) and never inherits the gate layer.

## Law

**A route that changes state must carry its own credential check. A transport-level
restriction is not a credential.**

Specifically:

1. **A published surface needs a credential, not a bind address.** If a route group can be
   reached from off-host, it authenticates. The bind address is a deployment detail; the
   ingress rule is a deployment detail; neither is an authorisation decision.
2. **A rebinding check is not an access control.** `loopback_only` answers "is this request
   for the loopback service?" — a question about DNS rebinding. It must not be the only
   thing between the internet and a write. A check that refuses a browser's `Origin` while
   admitting a script's silence is a check with no security value.
3. **The credential a write path uses must be the one the rest of the service verifies.**
   `/v1` and `/ui` must agree on the master (§3 of `docs/27` is the same defect one layer
   down: three names, one master, currently desynced).
4. **A missing or unreadable credential fails closed on a write.** Read paths may degrade
   to anonymous; a write path may not. Today an unreadable store row is a warning and the
   server continues (`crates/ar-cli/src/serve.rs:127-135`) — the wrong direction for a
   panel that can mint tokens and rewrite the routing.

## Detection

- **Any state-changing route reachable without a credential** is a finding. Probe it from
  off-host with `curl` (no `Origin`) and with a real browser (with `Origin`) — the two
  answers differ, and the *silent* one is the attacker.
- `loopback_only` is a **loopback check, not an auth check**. Any new route group that
  merges into the router outside `app()` must be reviewed for its own credential.
- `aroute doctor` reporting `gate,ok,armed` is **not** evidence the gate is armed: the
  check reads the row's *name*, not whether it decrypts
  (`crates/ar-cli/src/commands.rs:1086-1088` says so in its own comment). Verify with a
  **token that the live server accepts** (`/v1/models` → 200), not with `doctor`.
- The boot log line `ar: gate row present but unreadable — serving ungated` is the
  signature of this class: a credential store that is present, named, and not usable.
- `pending_restart` on the panel means the **file** changed, not that the running config
  did. A write that reports success has not proven it: re-read and compare
  (`docs/27` §2.2 records a "no-op" save that still rewrote the file).

## Orthogonality

the owners found are the ASKS register, the RUNBOOK deploy section, the STATE snapshot, the TODO register and the WORKLOG - none of them owns a regression record; the RUNBOOK section describes the deploy, and the finding is a defect class (a control panel whose trust boundary is the socket, inherited by a router that is later published through a tunnel) rather than a deployment step or a task row (recon R1019200910dce: unauthenticated write path public dashboard loopback_only gate)

## Revisions

- 2026-10-19: created
