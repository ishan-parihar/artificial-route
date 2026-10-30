# Bug Report & Execution Plan: Upstream Rejections, 502 Bad Gateway Cloudflare HTML Interception, and Context Ceilings

**Status: Investigation, Empirical Red-Teaming, and Validation Complete; Active Plan Ready for Execution.**
**Author: Antigravity (Pair Programming Agent)**
**Date: 2026-10-30**
**Tracking IDs: Register `ROUTE-10`, Ledger `D18`**

---

## 1. Executive Summary & Incident Overview

### The Incident
Clients calling `https://aroute.ishanparihar.com/v1/chat/completions` (notably `jcode` and `opencode`) using `minimax-m3.1-flash` and other single-target combos encountered sudden, fatal request crashes formatted as:

```text
OpenAI-compatible chat request failed endpoint: https://aroute.ishanparihar.com/v1/chat/completions model: minimax-m3.1-flash auth: AROUTE_API_KEY status: 502 Bad Gateway response: <!DOCTYPE html> <!--[if lt IE 7]> <html class="no-js ie6 ol"
```

Because the response was HTML rather than OpenAI-compatible JSON (`{"error": {...}}`), client parsers threw unhandled exceptions and sessions stalled.

### Summary of Findings
Empirical investigation and live red-teaming across the local host, Racknerd VPS, Cloudflare tunnel, and upstream providers (`integrate.api.nvidia.com`, `api.kilo.ai`) revealed **six interrelated failure layers**:

1. **400 -> 502 Status Mutation in `ar-server`:** When all combo targets reject a request with HTTP 400, `crates/ar-server/src/routes.rs:3381` explicitly rewrites `status = StatusCode::BAD_REQUEST` to `StatusCode::BAD_GATEWAY` (502).
2. **Cloudflare Tunnel Edge Interception:** Cloudflare edge fronting `aroute.ishanparihar.com` intercepts HTTP 502 from origin, discards the origin's JSON error payload, and substitutes Cloudflare's default HTML error page. (HTTP 400, by contrast, passes straight through with its JSON payload intact).
3. **Hardware / KV-Cache Limit on NVIDIA NIM (`262,144` Tokens):** Although MiniMax-M3.1-Flash model architecture supports 1M tokens, NVIDIA's hosted NIM trial API at `integrate.api.nvidia.com` deploys containers with a strict 256K (`262,144`) total token ceiling (`prompt_tokens + max_tokens`). Requests exceeding 262,144 tokens are rejected with HTTP 400:
   `{"message":"Invalid request","type":"Bad Request","code":400,"details":{"type":"context_length","limit":262144,"actual":...}}`.
4. **False Context Advertising in `config.yaml`:** `config.yaml` advertised `context_length: 1000000`. Clients (`jcode`, `opencode`) used this value to size their context buffers and compaction thresholds, allowing histories to grow to ~300k–400k tokens before compacting. These oversized prompts inevitably tripped NVIDIA's 262,144 token limit.
5. **Classifier Blind Spot in `ar-route`:** In `crates/ar-route/src/attempt.rs`, `ADVANCE_400_ROWS` searches for `"context length"` (space), failing to match NVIDIA's `"context_length"` (underscore). Furthermore, `json_error_has_code` only inspects root and `error` objects for `type`/`code`, ignoring `details.type`. NVIDIA's rejections were tagged `"unclassified_400"`.
6. **Single-Target Combos & Retired Models:**
   - `minimax-m3.1-flash`, `step-5-preview`, and `beam-free` have only 1 target configured; any upstream failure immediately exhausts the chain.
   - NVIDIA `z-ai/glm-5.3` reached End-of-Life on 2026-10-28 (HTTP 410 Gone: `"The model 'z-ai/glm-5.3' has reached its end of life on 2026-10-28T08:00:00Z and is no longer available."`).
   - NVIDIA `stepfun-ai/step-5-preview` and `z-ai/glm-5.3-flash` hang indefinitely (>110s), triggering client and gateway timeouts.

---

## 2. Empirical Red-Team Findings & Validation Evidence

### 2.1 Direct Reproduction via Public Cloudflare Endpoint
Sending a prompt with `max_tokens: 270000` to `https://aroute.ishanparihar.com/v1/chat/completions`:

```bash
# Request to public endpoint
curl -s -X POST https://aroute.ishanparihar.com/v1/chat/completions \
  -H "Authorization: Bearer $AROUTE_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"minimax-m3.1-flash","messages":[{"role":"user","content":"hi"}],"max_tokens":270000}'
```
**Observed Response:**
- **Status:** `502 Bad Gateway`
- **Content-Type:** `text/html; charset=UTF-8`
- **Body:**
  ```html
  <!DOCTYPE html>
  <!--[if lt IE 7]> <html class="no-js ie6 oldie" lang="en-US"> <![endif]-->
  <!--[if IE 7]>    <html class="no-js ie7 oldie" lang="en-US"> <![endif]-->
  <!--[if IE 8]>    <html class="no-js ie8 oldie" lang="en-US"> <![endif]-->
  <!--[if gt IE 8]><!--> <html class="no-js" lang="en-US"> <!--<![endif]-->
  <head>
  <title>aroute.ishanparihar.com | 502: Bad gateway</title>
  ...
  ```
This reproduces the exact user error verbatim.

### 2.2 Loopback Verification on VPS (Origin Truth)
Running the exact same request directly against the origin server at `http://127.0.0.1:20128/v1/chat/completions` on the Racknerd VPS:

```bash
# Loopback request directly to aroute
curl -i -X POST http://127.0.0.1:20128/v1/chat/completions \
  -H "Authorization: Bearer $AROUTE_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"minimax-m3.1-flash","messages":[{"role":"user","content":"hi"}],"max_tokens":270000}'
```
**Observed Response:**
- **Status:** `HTTP/1.1 502 Bad Gateway`
- **Content-Type:** `application/json`
- **Headers:**
  - `x-ar-decision: strategy=priority;outcome=failover;provider=nvidia;model=-;attempts=1`
  - `x-ar-attempt-diagnostic: {"attempt":1,"attempted_model":"minimaxai/minimax-m3.1-flash","classifier_rule":"unclassified_400","provider":"nvidia","status":400}`
- **Body:**
  ```json
  {"error":{"type":"server_error","code":"upstream_request_rejected","message":"all providers rejected the request; last was nvidia"}}
  ```
**Proof:**
1. The origin process (`aroute`) receives HTTP 400 from NVIDIA (`attempt_status: 400`).
2. `aroute` tags it `classifier_rule: "unclassified_400"`.
3. `aroute` deliberately converts the HTTP 400 to HTTP 502 Bad Gateway.
4. Cloudflare edge intercepts the 502, strips `aroute`'s JSON error, and generates the HTML page.

### 2.3 Cloudflare Passthrough Behavior on HTTP 400
To prove that Cloudflare does *not* intercept HTTP 400:
Sending an invalid JSON request to `https://aroute.ishanparihar.com/v1/chat/completions`:
- **Status:** `HTTP/2 400 Bad Request`
- **Content-Type:** `application/json`
- **Body:**
  ```json
  {"error":{"type":"invalid_request_error","code":"invalid_request","reason":"unparsable_body","message":"/v1/chat/completions expects a JSON object with `model`: expected value at line 1 column 1"}}
  ```
**Proof:** When `aroute` returns HTTP 400, Cloudflare forwards the JSON body intact to the client.

### 2.4 Empirical Boundary Probes against NVIDIA NIM API
Direct probes were executed against `https://integrate.api.nvidia.com/v1/chat/completions` using the live production key `AR_KEY_NVIDIA` across various context sizes:

| Prompt / Tokens | Max Tokens | Total (`actual`) | Status | Upstream Response |
|---|---|---|---|---|
| 5 tokens | 5 | 10 | **200 OK** | Successful completion |
| 10,179 tokens | 5 | 10,184 | **200 OK** | Successful completion |
| 100,179 tokens | 5 | 100,184 | **200 OK** | Successful completion |
| 179 tokens | 260,000 | 260,179 | **200 OK** | Successful completion |
| 179 tokens | 262,000 | 262,179 | **400 Bad Request** | `{"message":"Invalid request","type":"Bad Request","code":400,"details":{"type":"context_length","limit":262144,"actual":262179}}` |
| 179 tokens | 262,144 | 262,323 | **400 Bad Request** | `{"message":"Invalid request","type":"Bad Request","code":400,"details":{"type":"context_length","limit":262144,"actual":262323}}` |
| 270,179 tokens | 5 | 270,184 | **400 Bad Request** | `{"message":"Invalid request","type":"Bad Request","code":400,"details":{"type":"context_length","limit":262144,"actual":270184}}` |

**Conclusive Evidence on Context Limit:**
$262,144 = 256 \times 1024 = 2^{18}$ tokens.
NVIDIA NIM calculates `actual = prompt_tokens + max_tokens`.
If `actual <= 262,144`: NVIDIA accepts the request.
If `actual > 262,144`: NVIDIA strictly rejects the request with HTTP 400, naming `details.type: "context_length"` and `limit: 262144`.

### 2.5 Health Audit of Related Models
Probes against other models configured in `config.yaml`:
1. `nvidia/z-ai/glm-5.3`:
   - Response: `HTTP 410 Gone`
   - Body: `{"type":"about:blank","title":"Gone","status":410,"detail":"The model 'z-ai/glm-5.3' has reached its end of life on 2026-10-28T08:00:00Z and is no longer available."}`
2. `nvidia/stepfun-ai/step-5-preview`:
   - Hangs indefinitely (>110s timeout, HTTP 000).
3. `nvidia/z-ai/glm-5.3-flash`:
   - Hangs indefinitely (>110s timeout, HTTP 000).
4. `nvidia/deepseek-ai/deepseek-v4.1-flash`:
   - Active, responds 200 OK.
5. `kilocode/reflection/beam-free`:
   - Intermittently rate limited (429) or unavailable (503/400).

### 2.6 VPS SQLite Telemetry Audit
Querying the `failures` table in `/home/nerd/.config/ar/usage.sqlite`:
- 119 recorded failures for `minimax-m3.1-flash` with `status: 502, attempt_status: 400, classifier_rule: unclassified_400`.
- Over 3,400 failures on `glm-5.3` (429 and 502).
- Over 2,700 failures on `step-5-preview` (429 and 502 transport timeout).

---

## 3. Root Cause Analysis

### 3.1 Defect 1: HTTP 400 Rewritten to 502 Bad Gateway
In `crates/ar-server/src/routes.rs:3381-3385`:
```rust
let status = if status == StatusCode::BAD_REQUEST.as_u16() {
    StatusCode::BAD_GATEWAY
} else {
    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY)
};
```
And in `crates/ar-route/src/attempt.rs:117`:
```rust
Self::Failover { status, .. } if *status == 400 => StatusCode::BAD_GATEWAY,
```
**Mechanism:** The author intended to express that "the combo failed to find any provider to serve the request". However, rewriting a client-triggered 400 into a 502:
- Destroys semantics: 502 asserts that the gateway/proxy crashed, rather than the request being rejected.
- Triggers edge proxies (Cloudflare) to drop the JSON payload and substitute HTML.
- Confuses clients and automated agents into retrying indefinitely rather than adjusting their request (e.g. compacting context).

### 3.2 Defect 2: Cloudflare 502 HTML Replacement
Cloudflare Tunnel edge does not pass through origin 502 error bodies unless custom error page bypass rules are configured. Because `aroute` emitted 502, Cloudflare served HTML.

### 3.3 Defect 3: Advertised vs Real Model Context Window
In `~/.config/ar/config.yaml`:
```yaml
- id: minimax-m3.1-flash
  strategy: priority
  targets:
  - nvidia/minimaxai/minimax-m3.1-flash
  context_length: 1000000
```
While the MiniMax M3.1 Flash architecture can support 1M tokens, NVIDIA NIM's hosted trial API is deployed with `--context-length 262144`.
Advertising `1000000` to clients caused `jcode` to defer context compaction until ~350k tokens, consistently exceeding NVIDIA's 262,144 ceiling.

### 3.4 Defect 4: Classifier Blind Spot in `ar-route`
In `crates/ar-route/src/attempt.rs`:
```rust
const ADVANCE_400_ROWS: &[&str] = &[
    "context length",
    "maximum context",
    "too many tokens",
    "unsupported parameter",
    "unrecognized request argument",
    "does not support",
];
```
NVIDIA's error response uses `"type":"context_length"`. Because it uses an underscore, it does not match `"context length"`.
Furthermore, `json_error_has_code` only checked:
```rust
let root = value.as_object();
let error = value.get("error").and_then(serde_json::Value::as_object);
[root, error].into_iter().flatten().any(|object| {
    ["code", "type"].into_iter().any(|key| ...
```
In NVIDIA's JSON, the code is under `details.type`:
```json
{"message":"Invalid request","type":"Bad Request","code":400,"details":{"type":"context_length","limit":262144,"actual":270183}}
```
`json_error_has_code` never traversed `details`, so NVIDIA's context-limit 400 was categorized as `"unclassified_400"`.

---

## 4. Execution Plan

### Phase 1: Source Code Upgrades in `ar-server` and `ar-route`

#### 1.1 Preserve HTTP 400 on Exhausted Rejections
In `crates/ar-server/src/routes.rs`:
- When all targets rejected the request with HTTP 400 (or if `upstream_status == 400`), return `StatusCode::BAD_REQUEST` (400) to the client.
- Return a valid OpenAI-compatible JSON error envelope:
  ```json
  {
    "error": {
      "type": "invalid_request_error",
      "code": "upstream_request_rejected",
      "message": "all providers rejected the request; last was <provider>",
      "attempt_diagnostic": { ... }
    }
  }
  ```
- In `crates/ar-route/src/attempt.rs`:
  Update `AttemptOutcome::status(&self)` so that `Failover { status: 400, .. }` returns `StatusCode::BAD_REQUEST` instead of `BAD_GATEWAY`.

#### 1.2 Extend `classify_400` and `json_error_has_code`
In `crates/ar-route/src/attempt.rs`:
- Add `"context_length"` to `ADVANCE_400_ROWS`.
- Add `"context_length"` to `CAPABILITY_400_CODES`.
- In `json_error_has_code`, inspect `value.get("details")` in addition to `root` and `error` objects:
  ```rust
  let details = value.get("details").and_then(serde_json::Value::as_object);
  [root, error, details].into_iter().flatten().any(|object| ...
  ```
- This ensures NVIDIA's context-length 400 is correctly classified as `provider_capability_400`.

### Phase 2: Configuration & Target Resiliency

#### 2.1 Align Advertised Context Length
In `~/.config/ar/config.yaml` (both local and Racknerd VPS):
- Set `context_length: 262144` for combos targeting `nvidia/minimaxai/minimax-m3.1-flash`.
- Update any other NVIDIA-hosted models with their empirical limits.

#### 2.2 Add Redundant Fallbacks to Single-Target Combos
Combos must not rely on a single upstream target.
- For `minimax-m3.1-flash`: Add secondary fallback targets (e.g. `kilocode` or `openrouter` when keys exist).
- For `large-stack`: Remove retired `nvidia/z-ai/glm-5.3` (EOL 410); substitute working models such as `nvidia/deepseek-ai/deepseek-v4.1-flash`.
- For hanging models: Remove or place behind fast fallback timeouts `nvidia/stepfun-ai/step-5-preview` and `nvidia/z-ai/glm-5.3-flash`.

### Phase 3: Automated Verification Gates

1. **Unit & Integration Tests:**
   - Update `crates/ar-server/src/routes.rs` tests that asserted `StatusCode::BAD_GATEWAY` on 400 failovers; assert `StatusCode::BAD_REQUEST`.
   - Add unit test in `crates/ar-route/src/attempt.rs` validating classification of NVIDIA's exact 400 body:
     `{"message":"Invalid request","type":"Bad Request","code":400,"details":{"type":"context_length","limit":262144,"actual":270183}}` -> `provider_capability_400`.
   - Run `cargo test --release --locked`.
   - Run `cargo clippy --all-targets --all-features --locked -- -D warnings`.
2. **Loopback & Public Probe Verification:**
   - Send oversized mock request to loopback server; confirm response is HTTP 400 with `application/json` error envelope.
   - Deploy to Racknerd VPS; repeat probe through `https://aroute.ishanparihar.com/v1/chat/completions`. Confirm Cloudflare passes through HTTP 400 JSON and no HTML 502 is emitted.
   - Verify `jcode` and `opencode` client interactions succeed cleanly.

---

## 5. Ledger & Traceability Records

- **TODO Register:** `ROUTE-10`
- **Gap Ledger:** `docs/28-gap-ledger.md` row `D18`
- **Related Prior Items:** `ROUTE-09`, `D17`
