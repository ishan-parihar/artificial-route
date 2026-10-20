/* aroute control page. Vanilla JS on purpose: no build step, no remote
   fetches, works on an offline VPS. Hash-routed panes over one state
   snapshot; single-threaded DOM through the H() builder keeps everything
   XHR-safe with zero deps. */

"use strict";

const $ = (id) => document.getElementById(id);

/** Element builder: null/undefined kids are skipped, every text slot goes
 *  through textContent, so provider and combo names can never inject. */
function H(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === "class") el.className = v;
    else if (k === "text") el.textContent = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "dataset") for (const [dk, dv] of Object.entries(v)) el.dataset[dk] = dv;
    else el.setAttribute(k, v);
  }
  for (const kid of kids) if (kid != null) el.append(kid);
  return el;
}

let STATE = null;
let EDITING = null;
/** Live secrets, memory only (never storage): the just-minted token, and the
 *  bearer the playground sends. A reload drops both by design. */
let MINTED = null;
let PG_TOKEN = "";

/* ── the panel's credential ───────────────────────────────────────────────── */

/* The panel's own bearer, kept in sessionStorage: it is a live secret and it
   must not outlive the tab, but it also must not be typed again on every
   reload of the same tab. sessionStorage (not localStorage) is the right
   scope  -  it dies with the tab, so a shared browser does not keep it.

   A `?token=` on the first URL is the one path a token can arrive by that
   does not involve typing it. The query is read once, moved into
   sessionStorage, and then STRIPPED from the address bar: a token left in
   the URL ends up in the browser history, in the "copy this URL" a support
   request pastes, and in the Referer of the next navigation. The page sets
   `Referrer-Policy: no-referrer` too, so even a stripped URL cannot leak
   one. */
const TOKEN_KEY = "ar-panel-token";

function token() {
  try { return sessionStorage.getItem(TOKEN_KEY) || ""; } catch { return ""; }
}

function setToken(value) {
  try {
    if (value) sessionStorage.setItem(TOKEN_KEY, value);
    else sessionStorage.removeItem(TOKEN_KEY);
  } catch { /* a blocked storage is a panel that asks again */ }
}

/** Reads a `?token=` off the URL, stores it, and removes it from the URL. */
function takeTokenFromUrl() {
  const params = new URLSearchParams(location.search);
  const found = params.get("token");
  if (!found) return "";
  setToken(found);
  params.delete("token");
  const query = params.toString();
  history.replaceState(null, "", location.pathname + (query ? "?" + query : "") + location.hash);
  return found;
}

/* ── the api client ──────────────────────────────────────────────────────── */

/** A 401 from the panel's own API: the panel needs a credential. */
class NeedsToken extends Error {}

async function api(path, body) {
  const headers = {};
  if (body !== undefined) headers["content-type"] = "application/json";
  const bearer = token();
  if (bearer) headers.authorization = "Bearer " + bearer;
  const opts = body === undefined
    ? { headers }
    : { method: "POST", headers, body: JSON.stringify(body) };
  const res = await fetch(path, opts);
  const data = await res.json().catch(() => ({}));
  if (res.status === 401 && path.startsWith("/ui/api/")) {
    // The panel's own gate. The message says which, so the page can show
    // the token field rather than a generic failure.
    throw new NeedsToken(data?.error?.message || "this panel needs a credential");
  }
  if (!res.ok) throw Object.assign(new Error(data.error || res.statusText), { data });
  return data;
}

/* ── honest states ───────────────────────────────────────────────────────── */

/* Four states, and the vocabulary is OmniRoute's (the reference dashboard's
   own `EmptyState` / `CardSkeleton` split): loading, empty, error, populated.
   The rule this panel follows is that a state must be DISTINGUISHABLE  -  an
   error that vanishes after eight seconds is not a state, because a failed
   write that is gone before it is read is indistinguishable from a write
   that succeeded. So errors here persist until the operator dismisses them,
   and every figure says when it was read. */

function showError(id, err) {
  const p = $(id);
  if (!p) return;
  p.textContent = "";
  p.append(
    document.createTextNode(err.message +
      (err.data && err.data.help ? " - " + err.data.help : "")),
    H("button", { class: "btn small ghost", type: "button", text: "Dismiss",
      onclick: () => { p.hidden = true; } }));
  p.hidden = false;
}

/** The panel's token prompt, shown when the API refuses for want of one. */
function showTokenGate(message) {
  const gate = $("gate");
  if (!gate) return;
  const why = $("gate-why");
  if (why) why.textContent = message || "paste a token from `aroute keys mint`";
  gate.hidden = false;
  const input = $("gate-token");
  if (input && !input.value) input.focus();
}

function hideTokenGate() {
  const gate = $("gate");
  if (gate) gate.hidden = true;
}

/** One cell-wide message row: the loading, empty and error states share it. */
function stateRow(cols, text, cls) {
  return H("tr", null, H("td", { colspan: String(cols), class: "state " + cls, text }));
}

/** The four states as one helper, so every pane says them the same way. */
function stateBlock(cols, { loading, empty, error, retry }) {
  if (error) return stateRow(cols, error, "failed");
  if (loading) return stateRow(cols, loading, "loading");
  return stateRow(cols, empty, "empty");
}

/** The "as of" stamp every figure carries. */
function asOf() {
  return new Date().toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

/* ── chrome (sidebar, banner) ─────────────────────────────────────────────── */

function renderChrome(st) {
  STATE = st;
  $("ver").textContent = st.bin + " " + st.version;
  // The badge names the gate's ACTUAL state, including the broken one: a row
  // that exists and will not decrypt is a gate the server is not enforcing,
  // and the operator needs to see that at a glance rather than read "unarmed"
  // and wonder which of the two it is.
  const gate = $("gate-badge");
  const broken = !!st.gate_broken;
  gate.textContent = st.gate_badge || (st.gate_armed ? "gate armed" : "gate unarmed");
  gate.className = "badge " + (broken ? "bad" : st.gate_armed ? "on" : "off");
  if (broken) gate.title = st.gate_broken;
  $("banner").hidden = !st.pending_restart;
  $("foot-config").textContent = st.config_path;
  $("foot-config").title = st.config_path;
  $("foot-env").textContent = st.env_path;
  $("foot-env").title = st.env_path;
}

/* ── routing ──────────────────────────────────────────────────────────────── */

const VIEWS = {
  dashboard: paneDashboard,
  combos: paneCombos,
  providers: paneProviders,
  keys: paneKeys,
  usage: paneUsage,
  playground: panePlayground,
  resilience: paneResilience,
  health: paneHealth,
  budgets: paneBudgets,
  settings: paneSettings,
};
const TITLES = {
  dashboard: ["Dashboard", "What the router is serving, on one screen."],
  combos: ["Routing combos", "A combo is the model name a client asks for. Targets are tried in the strategy's order."],
  providers: ["Providers", "Credentials go to the env file the service loads. The value never appears on this page again."],
  keys: ["Client keys", "Tokens minted from the gate master, verified on every request. The token shows once."],
  usage: ["Usage", "Straight from the ledger the proxy bills against, with the requests that failed beside it."],
  playground: ["Playground", "Chat completions through the real gateway, same origin. Streams natively; no proxy in between."],
  resilience: ["Resilience", "What is out of rotation right now, and for how long. Read from the router's own table."],
  health: ["Health", "Whether each credential is present, readable, and bound to a live session."],
  budgets: ["Budgets", "What each key has spent against its ceiling, and what each provider's allowance says."],
  settings: ["Settings", "The yaml the proxy boots from. Environment-backed values name the variable that carries them."],
};

function currentView() {
  const h = location.hash;
  if (!h.startsWith("#/")) return "dashboard";
  const name = h.slice(2).split("?")[0];
  return VIEWS[name] ? name : "dashboard";
}

async function route() {
  const view = currentView();
  if (view !== "keys" && view !== "playground") MINTED = null; // shows once
  for (const a of $("nav").querySelectorAll("a"))
    a.classList.toggle("active", a.dataset.view === view);
  const root = $("view");
  root.textContent = "";
  const [title, hint] = TITLES[view];
  root.append(
    H("div", { class: "view-head" },
      H("h1", { text: title }),
      H("p", { class: "hint", text: hint })));
  VIEWS[view](root, STATE);
}

/** The refresh, with the panel's gate handled once at the top.
 *
 *  A 401 is not a per-pane error: it means the whole panel is locked, so
 *  the token prompt is shown and the panes are not rebuilt. Every other
 *  failure is a per-pane error and stays where it happened. */
async function refresh() {
  try {
    renderChrome(await api("/ui/api/state"));
  } catch (err) {
    if (err instanceof NeedsToken) {
      showTokenGate(err.message);
      return;
    }
    const root = $("view");
    root.textContent = "";
    root.append(H("p", { class: "error", role: "alert",
      text: "The page could not read /ui/api/state. Is the proxy running?" }));
    return;
  }
  hideTokenGate();
  await fillUsage(); // before route(): the dashboard's totals are built from USAGE
  await route();
}

/* ── usage fetch (dashboard band + usage pane) ────────────────────────────── */

let USAGE = null;
async function fillUsage() {
  USAGE = await api("/ui/api/usage").catch(() => null);
  const el = $("st-spend");
  if (el && USAGE) el.textContent = "$" + Number(USAGE.totals.usd).toFixed(4);
}

/* ── pane: dashboard ──────────────────────────────────────────────────────── */

function stat(label, value, valueId) {
  return H("div", { class: "stat" },
    H("dt", { text: label }), H("dd", { id: valueId, text: value }));
}

function paneDashboard(root, st) {
  // Client keys are signed tokens and never stored, so the state carries no
  // count for them. The card shows the gate they are verified against.
  root.append(
    H("section", { class: "stats", "aria-label": "Status" },
      stat("Providers", String(st.providers.length), "st-providers"),
      stat("Combos", String(st.combos.length), "st-combos"),
      stat("Client keys", st.gate_armed ? "armed" : "unarmed", "st-keys"),
      stat("Spend, all keys", USAGE ? "$" + Number(USAGE.totals.usd).toFixed(4) : "-", "st-spend")),
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Routing" }),
        H("a", { class: "hint", href: "#/combos", text: "edit combos \u2192" })),
      H("div", { class: "block-body", style: "padding:0" }, H("ol", { id: "combo-list", class: "rails" })),
      H("p", { id: "combo-error", class: "error", role: "alert", hidden: true })),
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Recent usage" }),
        H("a", { class: "hint", href: "#/usage", text: "full log \u2192" })),
      H("div", { class: "block-body" }, H("p", { class: "totals" }, usageSummary()))));
  renderCombos(st); // the Routing block lists the combos, same as the combos pane
}

function usageSummary() {
  if (!USAGE) return document.createTextNode("");
  return H("strong", { text: "$" + Number(USAGE.totals.usd).toFixed(6) },
  document.createTextNode(" across " + Number(USAGE.totals.tokens).toLocaleString() +
    " tokens, all keys, all time"));
}

/* ── pane: combos (rail render shared with dashboard) ─────────────────────── */

let RAIL_TARGET = ""; // pane or dashboard that wants the rail list+editor

function paneCombos(root, st) {
  root.append(
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Rails" }),
        H("button", { id: "add-combo", class: "btn", type: "button", text: "New combo" })),
      H("div", { class: "block-body", style: "padding:0" },
        H("ol", { id: "combo-list", class: "rails" }),
        H("div", { style: "padding: 0 16px 14px" },
          H("p", { id: "combo-error", class: "error", role: "alert", hidden: true })))));
  wireAddCombo();
  renderCombos(st);
}

function renderCombos(st) {
  const list = $("combo-list");
  if (!list || STATE !== st) return;
  list.textContent = "";
  RAIL_TARGET = currentView();
  st.combos.forEach((combo, i) => list.append(rail(combo, st, i)));
}

function rail(combo, st, idx) {
  const chain = H("div", { class: "chain" });
  combo.targets.forEach((t, i) => {
    if (i > 0) chain.append(H("span", { class: "link", "aria-hidden": "true", text: "\u2192" }));
    const pill = H("span", { class: "pill", style: `--i:${i}` },
      H("span", { class: "n", text: String(i + 1) }),
      H("span", { text: t }));
    if (EDITING === combo.id) pill.classList.add("flip"); // FLIP easing on idle pills
    chain.append(pill);
  });
  for (const p of combo.pool || []) chain.append(H("span", { class: "pill pool", text: p }));

  const meta = [];
  if (combo.context_length) meta.push("ctx " + combo.context_length);
  if (combo.pool && combo.pool.length) meta.push(combo.pool.length + " pool");
  if (combo.compression) meta.push("compress");
  if (combo.judge_model) meta.push("judge");

  const head = H("div", { class: "rail-head" },
    H("span", { class: "rail-id", text: combo.id }),
    H("span", { class: "rail-strategy", text: combo.strategy }),
    meta.length ? H("span", { class: "rail-meta", text: meta.join(" \u00b7 ") }) : null,
    H("div", { class: "rail-actions" },
      H("button", { class: "btn small ghost", type: "button", text: "Edit",
        onclick: () => { EDITING = combo.id; renderCombos(STATE); } }),
      H("button", { class: "btn small ghost", type: "button", text: "Delete",
        onclick: async () => {
          try {
            await api("/ui/api/combos", STATE.combos.filter((c) => c !== combo));
            EDITING = null;
            await refresh();
          } catch (err) { showError("combo-error", err); }
        } })));

  const li = H("li", { class: "rail" }, head, chain,
    EDITING === combo.id ? editor(combo, st) : null);
  if (EDITING === combo.id) li.classList.add("editing");
  if (idx < 6) li.classList.add("stagger");
  return li;
}

/** FLIP helper: record rects, mutate, redraw, then ease from the old spots. */
function flip(container, mutate) {
  const before = new Map();
  for (const el of container.children) {
    const key = el.getAttribute("aria-label") || el.textContent || "";
    before.set(key, el.getBoundingClientRect());
  }
  mutate();
  drawTargets(); // editor list redesigned below
  for (const el of container.children) {
    const key = el.getAttribute("aria-label") || el.textContent || "";
    const a = before.get(key), b = el.getBoundingClientRect();
    if (a && (a.top !== b.top)) {
      el.style.transition = "none";
      el.style.transform = `translateY(${a.top - b.top}px)`;
      requestAnimationFrame(() => {
        el.style.transition = "";
        el.style.transform = "";
      });
    }
  }
}

function editor(combo, st) {
  const strategies = Array.from(new Set([...(st.strategies || []), ...(st.strategies_in_use || [])]));
  const sel = H("select", { "aria-label": "strategy" },
    ...strategies.map((s) => H("option", { value: s, text: s })));
  sel.value = combo.strategy;

  const working = [...combo.targets];
  const targets = H("div", { class: "targets" });
  const drawTargets = () => {
    targets.textContent = "";
    working.forEach((t, i) => {
      const input = H("input", { value: t, spellcheck: "false", "aria-label": "target " + (i + 1) });
      input.addEventListener("change", () => { working[i] = input.value; });
      targets.append(H("div", { class: "target-row" }, input,
        H("button", { class: "btn small ghost icon-btn", type: "button", text: "\u2191", title: "move up", "aria-label": "move up target " + (i + 1),
          onclick: () => { if (i > 0) { [working[i - 1], working[i]] = [working[i], working[i - 1]]; flip(targets, () => {}); } } }),
        H("button", { class: "btn small ghost icon-btn", type: "button", text: "\u2193", title: "move down", "aria-label": "move down target " + (i + 1),
          onclick: () => { if (i < working.length - 1) { [working[i + 1], working[i]] = [working[i], working[i + 1]]; flip(targets, () => {}); } } }),
        H("button", { class: "btn small ghost icon-btn", type: "button", text: "\u00d7", "aria-label": "remove target " + (i + 1),
          onclick: () => { working.splice(i, 1); drawTargets(); } })));
    });
  };
  drawTargets();

  const idInput = H("input", { value: combo.id, spellcheck: "false", "aria-label": "combo id" });
  const ctxInput = H("input", { type: "number", min: "0", "aria-label": "context length" });
  ctxInput.value = combo.context_length || "";

  return H("form", { class: "editor", onsubmit: async (e) => {
    e.preventDefault();
    const next = structuredClone(combo);
    next.id = idInput.value.trim();
    next.strategy = sel.value;
    next.targets = working.map((t) => t.trim()).filter(Boolean);
    next.context_length = Number(ctxInput.value) > 0 ? Number(ctxInput.value) : null;
    try {
      await api("/ui/api/combos", STATE.combos.map((c) => (c === combo ? next : c)));
      EDITING = null;
      await refresh();
    } catch (err) {
      showError("combo-error", err);
    }
  } },
    H("div", null, H("label", { text: "Combo id (the model a client asks for)" }), idInput),
    H("div", null, H("label", { text: "Strategy" }), sel),
    H("div", null, H("label", { text: "Targets, in try order (provider/model)" }), targets,
      H("button", { class: "btn small ghost", type: "button", text: "Add target",
        onclick: () => { working.push(""); drawTargets(); } })),
    H("div", null, H("label", { text: "Context length (tokens, optional)" }), ctxInput),
    H("div", { class: "actions" },
      H("button", { class: "btn", type: "submit", text: "Save combo" }),
      H("button", { class: "btn ghost", type: "button", text: "Cancel",
        onclick: () => { EDITING = null; renderCombos(STATE); } })));
}

function wireAddCombo() {
  const btn = $("add-combo");
  if (!btn) return;
  btn.addEventListener("click", async () => {
    try {
      await api("/ui/api/combos", [...STATE.combos, freshCombo()]);
      EDITING = "new-combo";
      await refresh();
    } catch (err) { showError("combo-error", err); }
  });
}

/** A fresh combo, every optional field explicitly empty: cloning an existing
 *  combo would inherit its compression and judge settings. */
function freshCombo() {
  return {
    id: "new-combo", strategy: "priority", targets: [], weights: {},
    pool: [], steps: [], compression: null, judge_model: null, context_length: null,
  };
}

/* ── pane: providers ──────────────────────────────────────────────────────── */

function paneProviders(root, st) {
  root.append(
    H("section", { class: "block" },
      H("div", { class: "block-head" }, H("h2", { text: "Configured" }),
        H("input", { id: "provider-filter", type: "search", placeholder: "filter providers",
          "aria-label": "filter providers", value: PROVIDER_FILTER,
          oninput: (e) => { PROVIDER_FILTER = e.target.value; PROVIDER_ALL = false; renderProviders(STATE); } }),
        H("button", { id: "provider-more", type: "button", class: "btn ghost small", hidden: true,
          onclick: () => { PROVIDER_ALL = true; renderProviders(STATE); } })),
      H("div", { class: "block-body", style: "padding:0" },
        H("div", { class: "table-wrap" }, H("table", null,
          H("thead", null, H("tr", null,
            H("th", { text: "Provider" }), H("th", { text: "Credential" }),
            H("th", { text: "Env vars" }), H("th", { text: "Paste key" }),
            H("th", { text: "" }))),
          H("tbody", { id: "provider-rows" })))),
      H("div", { class: "block-body" },
        H("form", { id: "provider-add", class: "row-form" },
          H("label", { for: "new-provider", text: "Registry id" }),
          H("input", { id: "new-provider", name: "new-provider", autocomplete: "off", spellcheck: "false", placeholder: "openrouter", style: "width:auto" }),
          H("label", { for: "new-provider-key", text: "API key" }),
          H("input", { id: "new-provider-key", name: "new-provider-key", type: "password", autocomplete: "off" }),
          H("button", { type: "submit", class: "btn", text: "Add and set key" })),
        H("form", { id: "provider-custom", class: "row-form" },
          H("label", { for: "custom-id", text: "Custom id" }),
          H("input", { id: "custom-id", autocomplete: "off", spellcheck: "false", placeholder: "my-gateway", style: "width:auto" }),
          H("label", { for: "custom-protocol", text: "Protocol" }),
          H("select", { id: "custom-protocol" },
            H("option", { value: "openai-compatible", text: "openai-compatible" }),
            H("option", { value: "anthropic-compatible", text: "anthropic-compatible" })),
          H("label", { for: "custom-base", text: "Base URL" }),
          H("input", { id: "custom-base", autocomplete: "off", spellcheck: "false", placeholder: "https://host/v1", style: "width:auto" }),
          H("label", { for: "custom-endpoint", text: "Endpoint" }),
          H("input", { id: "custom-endpoint", autocomplete: "off", spellcheck: "false", placeholder: "default", style: "width:auto" }),
          H("label", { for: "custom-key", text: "API key" }),
          H("input", { id: "custom-key", type: "password", autocomplete: "off" }),
          H("button", { type: "submit", class: "btn", text: "Save custom provider" })),
        H("p", { id: "provider-error", class: "error", role: "alert", hidden: true }))),
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { id: "catalog-head", text: "Registry catalog" }),
        H("span", { class: "hint", text: "Read-only. Use an id in the Registry id form above." }),
        H("input", { id: "catalog-filter", autocomplete: "off", spellcheck: "false", placeholder: "filter providers/models", style: "width:auto" })),
      H("details", { class: "block-body fav", open: "open" },
        H("summary", null, "\u2605 Favorites ", H("span", { id: "fav-count", class: "varname", text: "0" })),
        H("div", { id: "fav-body" })),
      H("div", { class: "block-body", id: "catalog-body" })));
  renderProviders(st);
  wireProviderAdd();
  wireProviderCustom();
  wireProviderActions();
  wireSetKeys();
  loadCatalog();
}

/* The compiled-in registry: one GET, rendered per provider on demand. */

const FAV_KEY = "ar-favorites";
let CATALOG = [];

/** Starred "provider/model" ids. Browser-local; storage may be blocked. */
function favs() {
  try { return JSON.parse(localStorage.getItem(FAV_KEY)) || []; } catch { return []; }
}

function toggleFav(id) {
  const cur = favs();
  const next = cur.includes(id) ? cur.filter((f) => f !== id) : [...cur, id];
  try { localStorage.setItem(FAV_KEY, JSON.stringify(next)); } catch { /* unstarred next load */ }
  paintFavs();
}

function priceText(m) {
  return m.in == null ? "" : " $" + m.in + "/$" + m.out + " per mtok";
}

function starBtn(id, on) {
  return H("button", { type: "button", class: "star" + (on ? " on" : ""),
    "aria-pressed": on ? "true" : "false", "aria-label": (on ? "unstar " : "star ") + id,
    text: on ? "\u2605" : "\u2606", dataset: { star: id } });
}

/** Favorites list, then sync every star in the catalog below it. */
function paintFavs() {
  const host = $("fav-body");
  if (!host) return;
  const on = new Set(favs());
  host.textContent = "";
  const rows = [];
  for (const p of CATALOG) for (const m of p.models)
    if (on.has(p.id + "/" + m.id)) rows.push([p.id + "/" + m.id, m]);
  $("fav-count").textContent = String(rows.length);
  if (!rows.length) {
    host.append(H("p", { class: "hint", text: "No favorites yet. Star a model in the catalog below to pin it here." }));
  }
  for (const [id, m] of rows)
    host.append(H("span", { class: "keycell" }, starBtn(id, true), H("span", { class: "varname", text: id + priceText(m) })));
  for (const b of $("catalog-body").querySelectorAll("button.star")) {
    const isOn = on.has(b.dataset.star);
    b.classList.toggle("on", isOn);
    b.setAttribute("aria-pressed", isOn ? "true" : "false");
    b.textContent = isOn ? "\u2605" : "\u2606";
  }
}

async function loadCatalog() {
  const host = $("catalog-body");
  let data;
  try {
    data = await api("/ui/api/models");
  } catch (err) {
    if (!host.isConnected) return; // the operator navigated away mid-flight
    host.append(H("p", { class: "error", text: err.message }));
    return;
  }
  CATALOG = data.providers || [];
  if (!host.isConnected) return; // the operator navigated away mid-flight
  $("catalog-head").textContent = "Registry catalog (" + CATALOG.length + ")";
  host.append(...CATALOG.map((p) => H("details", { class: "catalog-row", dataset: { pid: p.id } },
    H("summary", null,
      H("span", { class: "cta", text: p.configured ? "configured" : "" }),
      H("strong", { text: p.id }),
      H("span", { class: "varname", text: " " + p.wire + " \u00b7 " + p.models.length + " models " }),
      H("button", { type: "button", class: "btn ghost small", text: "use",
        dataset: { useId: p.id } })),
    H("div", { class: "keycell" }, ...p.models.map((m) =>
      H("span", { class: "keycell" }, starBtn(p.id + "/" + m.id, false),
        H("span", { class: "varname", text: m.id + priceText(m) })))))));
  paintFavs();
  $("catalog-filter").addEventListener("input", (e) => {
    const q = e.target.value.toLowerCase();
    for (const row of host.children) {
      const hit = row.textContent.toLowerCase().includes(q);
      row.hidden = !hit;
    }
  });
  // one delegate for both lists: star toggles, "use" fills the add form
  const onClick = (e) => {
    const d = e.target && e.target.dataset ? e.target.dataset : {};
    if (d.star) toggleFav(d.star);
    else if (d.useId) $("new-provider").value = d.useId;
  };
  host.addEventListener("click", onClick);
  $("fav-body").addEventListener("click", onClick);
}

/* The providers table is capped: each row is a password field and two buttons, so a
 * long config would otherwise put thousands of controls on one page. The filter and
 * "show all" survive a refresh, since refresh() rebuilds this table from STATE. */
const PROVIDER_CAP = 40;
let PROVIDER_FILTER = "";
let PROVIDER_ALL = false;

function renderProviders(st) {
  const body = $("provider-rows");
  if (!body) return;
  body.textContent = "";
  const q = PROVIDER_FILTER.trim().toLowerCase();
  const matches = st.providers.filter((p) => !q || p.id.toLowerCase().includes(q));
  const shown = PROVIDER_ALL || q ? matches : matches.slice(0, PROVIDER_CAP);
  const more = $("provider-more");
  if (more) {
    more.hidden = shown.length === matches.length;
    more.textContent = `Show all ${matches.length}`;
  }
  for (const p of shown) {
    const cred = H("td");
    for (const ref of p.key_refs) {
      cred.append(H("span", { class: "keycell" },
        H("span", { class: "dot" + (ref.set ? " set" : "") }),
        H("span", { text: ref.ref }),
        ref.var ? H("span", { class: "varname", text: " " + ref.var }) : null));
    }
    body.append(H("tr", null,
      H("td", null, H("span", { text: p.id }),
        p.custom ? H("span", { class: "varname", text: " custom " + (p.base_url || "") }) : null),
      cred,
      H("td", { class: "varname", text: p.key_refs.map((r) => r.var).join(", ") || "-" }),
      H("td", null, H("input", { type: "password", autocomplete: "off",
        "aria-label": "key for " + p.id, placeholder: "paste key",
        dataset: { providerId: p.id } })),
      H("td", { class: "row-actions" },
        p.custom ? H("button", { type: "button", class: "btn ghost small", text: "Edit",
          dataset: { editId: p.id, editBase: p.base_url || "", editProtocol: p.protocol || "openai-compatible",
            editEndpoint: p.endpoint || "" } }) : null,
        H("button", { type: "button", class: "btn ghost small", text: "Remove",
          dataset: { removeId: p.id } }))));
  }
}

/* one delegated submit for every provider row's Set action */
function wireSetKeys() {
  $("provider-rows").addEventListener("submit", async (e) => {
    e.preventDefault();
    const input = e.target.querySelector("input[type=password]");
    const id = input.dataset.providerId;
    try {
      await api("/ui/api/provider-key", { id, key: input.value });
      input.value = "";
      await refresh();
    } catch (err) { showError("provider-error", err); }
  });
}

function wireProviderAdd() {
  $("provider-add").addEventListener("submit", async (e) => {
    e.preventDefault();
    try {
      await api("/ui/api/provider-key", {
        id: $("new-provider").value.trim(),
        key: $("new-provider-key").value,
      });
      $("new-provider").value = "";
      $("new-provider-key").value = "";
      await refresh();
    } catch (err) { showError("provider-error", err); }
  });
}

/* registers or edits one custom provider; an empty key keeps a row's binding */
function wireProviderCustom() {
  $("provider-custom").addEventListener("submit", async (e) => {
    e.preventDefault();
    try {
      await api("/ui/api/provider-custom", {
        id: $("custom-id").value.trim(),
        protocol: $("custom-protocol").value,
        base_url: $("custom-base").value.trim(),
        endpoint: $("custom-endpoint").value.trim(),
        key: $("custom-key").value,
      });
      for (const id of ["custom-id", "custom-base", "custom-endpoint", "custom-key"]) $(id).value = "";
      await refresh();
    } catch (err) { showError("provider-error", err); }
  });
}

/* Edit loads a custom row back into the form; Remove drops it from config.yaml */
function wireProviderActions() {
  $("provider-rows").addEventListener("click", async (e) => {
    const btn = e.target.closest("button");
    if (!btn) return;
    if (btn.dataset.editId) {
      $("custom-id").value = btn.dataset.editId;
      $("custom-protocol").value = btn.dataset.editProtocol;
      $("custom-base").value = btn.dataset.editBase;
      $("custom-endpoint").value = btn.dataset.editEndpoint;
      $("custom-key").value = ""; // empty key = keep the binding's variable
      $("custom-id").focus();
      return;
    }
    if (btn.dataset.removeId) {
      if (!confirm("Remove provider " + btn.dataset.removeId + " from config.yaml?")) return;
      try {
        await api("/ui/api/provider-remove", { id: btn.dataset.removeId });
        await refresh();
      } catch (err) { showError("provider-error", err); }
    }
  });
}

/* ── pane: keys ───────────────────────────────────────────────────────────── */

function paneKeys(root, st) {
  root.append(
    H("section", { class: "block" },
      H("div", { class: "block-head" }, H("h2", { text: "Mint" })),
      H("div", { class: "block-body" },
        H("form", { id: "mint-form", class: "row-form" },
          H("label", { for: "mint-id", text: "Key id" }),
          H("input", { id: "mint-id", autocomplete: "off", spellcheck: "false", placeholder: "ci-runner", style: "width:auto" }),
          H("label", { for: "mint-ttl", text: "days" }),
          H("input", { id: "mint-ttl", type: "number", min: "0", max: "365", value: "7", style: "width:80px" }),
          H("label", { for: "mint-rpm", text: "rpm" }),
          H("input", { id: "mint-rpm", type: "number", min: "1", max: "100000", placeholder: "unset", style: "width:90px" }),
          H("button", { type: "submit", class: "btn btn-primary", "data-cta": "primary", text: "Mint key" })),
        H("p", { class: "hint", text: "The token lives for the number of days above. 0 mints one that never expires, which is the wrong default for anything that might leak. Revoking a token needs the token itself, so a shorter life is what bounds a leak you did not keep a copy of." }),
        H("details", { id: "minted", class: "minted", hidden: true, open: "open" },
          H("summary", null, "Token for ", H("span", { id: "minted-id" }), " - copy it now"),
          H("code", { id: "minted-token" }),
          H("p", { id: "minted-life", class: "hint" }),
          H("button", { id: "minted-use", type: "button", class: "btn small ghost", text: "Use in playground",
            onclick: () => { if (MINTED) { PG_TOKEN = MINTED.access; location.hash = "#/playground"; } } })),
        H("p", { id: "keys-error", class: "error", role: "alert", hidden: true }))),
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Limits" }),
        H("span", { class: "hint", text: "Per-key ceilings in `limits:`, enforced at the edge. Blank an arm to clear it; all-clear removes the row. A combo scope limits the spend arms to requests for that combo. A per-model cap (`target=cap`) is measured against THAT model's spend, so a model that burns its budget fails over to a sibling instead of taking the key down with it. A window (`24h`) makes the caps a rate rather than a total." })),
      H("div", { class: "block-body" },
        H("div", { class: "table-wrap" }, H("table", null,
          H("thead", null, H("tr", null,
            H("th", { text: "Key" }), H("th", { text: "rpm" }), H("th", { text: "$ cap" }),
            H("th", { text: "token cap" }), H("th", { text: "unpriced" }), H("th", { text: "scope" }),
            H("th", { text: "per-model caps" }), H("th", { text: "window" }), H("th", { text: "Spend, all time" }),
            H("th", { text: "Set ceilings" }))),
          H("tbody", { id: "key-rows" }))))));
  renderKeys(st);
  wireMint();
  showMinted();
}

/* refresh() rebuilds this pane after a mint, so the token lives in MINTED */
function showMinted() {
  if (!MINTED) return;
  $("minted").hidden = false;
  $("minted-id").textContent = MINTED.key_id;
  $("minted-token").textContent = MINTED.access;
  // The lifetime, in days — the form the operator set it in. A raw unix
  // timestamp is not actionable, and `ttl_days === 0` means "never", which
  // has to read as such rather than as "0 days".
  const life = $("minted-life");
  if (life) {
    life.textContent = MINTED.ttl_days === 0
      ? "never expires"
      : "lives for " + MINTED.ttl_days + (MINTED.ttl_days === 1 ? " day" : " days");
  }
}

function renderKeys(st) {
  const rows = (st.limits && st.limits.keys) || {};
  const body = $("key-rows");
  body.textContent = "";
  const names = Object.keys(rows);
  if (!names.length) {
    body.append(H("tr", null, H("td", { colspan: "10", class: "varname",
      text: "No limits rows yet. Mint a key to start." })));
  }
  for (const name of names) {
    const cap = rows[name];
    const used = USAGE && USAGE.rows ? USAGE.rows.filter((r) => r.key === name) : [];
    const spend = used.reduce((s, r) => s + (r.priced ? Number(r.usd) : 0), 0);
    const num = (label) => H("input", { type: "number", min: "1", "aria-label": label + " for " + name, placeholder: "unset", style: "width:86px" });
    const rpmInput = num("rpm");
    const usdInput = num("usd_micros");
    const tokensInput = num("tokens");
    // Prefilled with the row's current values: blank clears an arm, so a form
    // that started empty would wipe every other arm on each Set.
    rpmInput.value = cap.rpm == null ? "" : cap.rpm;
    usdInput.value = cap.usd_micros == null ? "" : cap.usd_micros;
    tokensInput.value = cap.tokens == null ? "" : cap.tokens;
    const unpriced = H("input", { type: "checkbox", id: "unpriced-" + name, "aria-label": "refuse_unpriced for " + name });
    unpriced.checked = !!cap.refuse_unpriced;
    const scope = H("input", { "aria-label": "combo scope for " + name, placeholder: "all combos", spellcheck: "false", autocomplete: "off", style: "width:110px" });
    scope.value = cap.model || "";
    // The per-target ceilings, in ONE field: `target=ceiling` pairs separated
    // by commas. A field per target would make the row's width depend on the
    // config, and the map IS the scope — the target names the model it caps.
    const modelTokens = H("input", { "aria-label": "per-model token caps for " + name, placeholder: "target=cap,target=cap", spellcheck: "false", autocomplete: "off", style: "width:220px" });
    modelTokens.value = Object.entries(cap.model_tokens || {}).map(([t, c]) => t + "=" + c).join(",");
    const windowInput = H("input", { "aria-label": "spend window for " + name, placeholder: "24h", spellcheck: "false", autocomplete: "off", style: "width:70px" });
    windowInput.value = cap.spend_window || "";
    body.append(H("tr", null,
      H("td", { text: name }),
      H("td", { class: "num", text: cap.rpm == null ? "unset" : String(cap.rpm) }),
      H("td", { class: "num", text: cap.usd_micros == null ? "unset" : "$" + cap.usd_micros / 1000000 }),
      H("td", { class: "num", text: cap.tokens == null ? "unset" : String(cap.tokens) }),
      H("td", { class: "num", text: cap.refuse_unpriced ? "refuse" : "allow" }),
      H("td", { class: "varname", text: cap.model || "all" }),
      H("td", { class: "varname", text: Object.keys(cap.model_tokens || {}).length ? Object.keys(cap.model_tokens).length + " capped" : "none" }),
      H("td", { class: "varname", text: cap.spend_window || "all time" }),
      H("td", { class: "num", text: "$" + spend.toFixed(6) }),
      H("td", null, H("form", { class: "row-form", onsubmit: async (e) => {
        e.preventDefault();
        const send = (input) => input.type === "checkbox" ? input.checked : (input.value === "" ? null : Number(input.value));
        // The per-target field is parsed here so the server gets a map, and a
        // malformed pair is refused before anything is sent.
        let caps = {};
        for (const pair of modelTokens.value.split(",").map((s) => s.trim()).filter(Boolean)) {
          const at = pair.lastIndexOf("=");
          if (at < 1) { showError("keys-error", new Error("per-model cap " + JSON.stringify(pair) + " is not target=cap")); return; }
          const ceiling = Number(pair.slice(at + 1));
          if (!Number.isFinite(ceiling) || ceiling <= 0) { showError("keys-error", new Error("per-model cap " + JSON.stringify(pair) + " is not a positive number")); return; }
          caps[pair.slice(0, at).trim()] = ceiling;
        }
        try {
          await api("/ui/api/limits", { key: name,
            rpm: send(rpmInput), usd_micros: send(usdInput), tokens: send(tokensInput),
            refuse_unpriced: send(unpriced), model: scope.value.trim() || null,
            model_tokens: caps, spend_window: windowInput.value.trim() || null });
          await refresh();
        } catch (err) { showError("keys-error", err); }
      } }, rpmInput, usdInput, tokensInput, unpriced, scope, modelTokens, windowInput, H("button", { class: "btn small", type: "submit", text: "Set" })))));
  }
}

function wireMint() {
  $("mint-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const key_id = $("mint-id").value.trim();
    // The lifetime is omitted when the field is empty, so the server's own
    // default applies. Sending 0 instead would mint a token that NEVER
    // expires — the exact opposite of what a cleared field should mean.
    const ttlText = $("mint-ttl").value.trim();
    const body = { key_id };
    if (ttlText !== "") body.ttl_days = Number(ttlText);
    const rpm = $("mint-rpm").value === "" ? null : Number($("mint-rpm").value);
    try {
      const res = await api("/ui/api/keys/mint", body);
      if (rpm != null) await api("/ui/api/limits", { key: key_id, rpm });
      MINTED = { key_id: res.key_id, access: res.access, ttl_days: res.ttl_days, expires_at: res.expires_at };
      showMinted();
      await refresh();
    } catch (err) { showError("keys-error", err); }
  });
}

/* ── pane: usage ──────────────────────────────────────────────────────────── */

/** The filter bar's state; survives a refresh() so a rebuilt pane keeps its view. */
const LOGQ = { days: 30, model: "", provider: "", error: false };
const RANGES = [7, 30, 90];

function logQuery() {
  const q = new URLSearchParams({ days: String(LOGQ.days) });
  if (LOGQ.model) q.set("model", LOGQ.model);
  if (LOGQ.provider) q.set("provider", LOGQ.provider);
  if (LOGQ.error) q.set("status", "error");
  return q;
}

function filterActive() {
  return LOGQ.model !== "" || LOGQ.provider !== "" || LOGQ.error || LOGQ.days !== 30;
}

function paneUsage(root) {
  const field = (label, control) => H("div", { class: "fl" }, H("label", { for: control.id, text: label }), control);
  const range = H("select", { id: "lg-days" },
    ...RANGES.map((d) => H("option", { value: String(d), text: "last " + d + " days" })));
  range.value = String(LOGQ.days);
  const model = H("input", { id: "lg-model", list: "lg-models", autocomplete: "off", spellcheck: "false", placeholder: "any", value: LOGQ.model });
  const provider = H("input", { id: "lg-provider", list: "lg-providers", autocomplete: "off", spellcheck: "false", placeholder: "any", value: LOGQ.provider });
  const errs = H("input", { id: "lg-error", type: "checkbox" });
  errs.checked = LOGQ.error;
  const apply = () => {
    LOGQ.days = Number(range.value);
    LOGQ.model = model.value.trim();
    LOGQ.provider = provider.value.trim();
    LOGQ.error = errs.checked;
    loadUsageView();
  };
  for (const el of [range, errs]) el.addEventListener("change", apply);
  for (const el of [model, provider]) el.addEventListener("change", apply);
  root.append(
    H("section", { class: "block" },
      H("form", { id: "lg-form", class: "filters", onsubmit: (e) => { e.preventDefault(); apply(); } },
        field("Range", range), field("Model", model), field("Provider", provider),
        H("div", { class: "fl check" }, errs, H("label", { for: "lg-error", text: "Failed requests only" })),
        H("button", { id: "lg-clear", class: "btn ghost small", type: "button", text: "Clear filters", hidden: true,
          onclick: () => { Object.assign(LOGQ, { days: 30, model: "", provider: "", error: false }); route(); } }),
        // The reload sits in the FILTER BAR, not at the head of the log: the
        // log is the last block on the page, and a primary action the operator
        // has to scroll to find is not a primary action.
        H("button", { id: "usage-reload", class: "btn btn-primary small", "data-cta": "primary",
          type: "submit", text: "Reload" }),
        H("datalist", { id: "lg-models" }), H("datalist", { id: "lg-providers" }))),
    H("p", { id: "usage-error", class: "error", role: "alert", hidden: true }),
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Spend by day" }),
        H("span", { id: "usage-asof", class: "hint" })),
      H("div", { class: "block-body" },
        H("div", { class: "chart", id: "usage-days", role: "img", "aria-label": "daily spend bar chart" }),
        H("p", { class: "chart-caption", id: "usage-days-caption" }))),
    H("div", { class: "col2" },
      rankBlock("Top models", "usage-models"),
      rankBlock("Top providers", "usage-providers")),
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Spend by combo" }),
        H("span", { class: "hint", text: "What each name the client asked for cost." })),
      H("div", { class: "block-body", style: "padding:0" },
        H("div", { class: "table-wrap" }, H("table", null,
          H("thead", null, H("tr", null,
            H("th", { scope: "col", text: "Combo" }),
            H("th", { scope: "col", class: "num", text: "Req" }),
            H("th", { scope: "col", class: "num", text: "Tokens" }),
            H("th", { scope: "col", class: "num", text: "USD" }))),
          H("tbody", { id: "usage-combos" }))))),
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Request log" }),
        H("span", { id: "usage-totals", class: "hint" })),
      H("div", { class: "block-body", style: "padding:0" },
        H("div", { class: "table-wrap" }, H("table", null,
          H("thead", null, H("tr", null,
            H("th", { scope: "col", text: "Time" }), H("th", { scope: "col", text: "Key" }),
            H("th", { scope: "col", text: "Provider" }), H("th", { scope: "col", text: "Model" }),
            H("th", { scope: "col", text: "Status" }),
            H("th", { scope: "col", class: "num", text: "Tokens" }), H("th", { scope: "col", class: "num", text: "USD" }),
            H("th", { scope: "col", text: "Detail" }))),
          H("tbody", { id: "usage-rows" }))))));
  loadUsageView();
}

function rankBlock(title, tbodyId) {
  return H("section", { class: "block" },
    H("div", { class: "block-head" }, H("h2", { text: title })),
    H("div", { class: "block-body", style: "padding:0" },
      H("div", { class: "table-wrap" }, H("table", null,
        H("thead", null, H("tr", null,
          H("th", { scope: "col", text: "Name" }), H("th", { scope: "col", class: "num", text: "Req" }),
          H("th", { scope: "col", class: "num", text: "Tokens" }), H("th", { scope: "col", class: "num", text: "USD" }))),
        H("tbody", { id: tbodyId })))));
}

/** One cell-wide message row: the loading, empty and error states share it. */
function stateRow(cols, text, cls) {
  return H("tr", null, H("td", { colspan: String(cols), class: "state " + cls, text }));
}

let USAGE_SEQ = 0;

/** Loads chart + ranks + log for the current filter. Last request wins, so a
 *  slow earlier answer cannot overwrite a newer filter. */
async function loadUsageView() {
  const seq = ++USAGE_SEQ;
  const body = $("usage-rows");
  if (!body) return;
  $("usage-error").hidden = true;
  $("lg-clear").hidden = !filterActive();
  body.textContent = "";
  body.append(stateRow(8, "Loading…", "loading"));
  let usage, log;
  try {
    [usage, log] = await Promise.all([
      api("/ui/api/usage?days=" + LOGQ.days),
      api("/ui/api/log?" + logQuery()),
    ]);
  } catch (err) {
    if (seq !== USAGE_SEQ || !$("usage-rows")) return;
    body.textContent = "";
    body.append(stateRow(8, "Could not load the log.", "failed"));
    const box = $("usage-error");
    box.textContent = "";
    box.append(err.message + " ", H("button", { class: "btn small", type: "button", text: "Retry", onclick: loadUsageView }));
    box.hidden = false;
    return;
  }
  if (seq !== USAGE_SEQ || !$("usage-rows")) return;
  if (usage.error || log.error) {
    body.textContent = "";
    body.append(stateRow(8, "The ledger could not be read.", "failed"));
    $("usage-error").textContent = usage.error || log.error;
    $("usage-error").hidden = false;
    return;
  }
  paintUsageChart(usage.analytics);
  paintRank("usage-models", usage.analytics);
  paintRank("usage-providers", usage.analytics);
  paintRank("usage-combos", usage.analytics);
  paintSuggestions(usage.analytics, log.rows);
  paintLog(log);
}

function paintSuggestions(analytics, rows) {
  const fill = (id, names) => {
    const dl = $(id);
    if (!dl || !dl.isConnected) return;
    dl.textContent = "";
    for (const n of new Set(names)) if (n && n !== "-") dl.append(H("option", { value: n }));
  };
  fill("lg-models", [...(analytics.models || []).map((m) => m.name), ...rows.map((r) => r.model)]);
  fill("lg-providers", [...(analytics.providers || []).map((p) => p.name), ...rows.map((r) => r.provider)]);
}

const TIME_FMT = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });

/** Rows shown so far for the current filter; older pages append to them. */
let LOG_SHOWN = 0;
let LOG_FAILED = 0;

function logRow(r) {
  const when = new Date(r.at * 1000);
  const bad = r.status >= 400;
  return H("tr", { class: bad ? "failed" : "" },
    H("td", { class: "num", text: TIME_FMT.format(when), title: when.toISOString() }),
    H("td", { text: r.key }),
    H("td", { text: r.provider }),
    H("td", { text: r.model }),
    H("td", null, H("span", { class: "status " + (bad ? "bad" : "good"), text: bad ? r.status + " error" : "ok" })),
    H("td", { class: "num", text: Number(r.tokens).toLocaleString() }),
    H("td", { class: "num", text: bad ? "-" : r.priced ? "$" + Number(r.usd).toFixed(6) : "unpriced" }),
    H("td", { class: "detail", text: r.error, title: r.error }));
}
function logMoreRow(offset) {
  return H("tr", { class: "more" },
    H("td", { colspan: "8" },
      H("button", { class: "btn small", type: "button", text: "Load older", onclick: () => loadOlderLog(offset) })));
}

function paintLogTotals() {
  const box = $("usage-totals");
  if (!box) return;
  box.textContent = LOG_SHOWN ? "Showing " + LOG_SHOWN + " · " + LOG_FAILED + " failed" : "";
}

function paintLog(log) {
  const body = $("usage-rows");
  if (!body || !body.isConnected) return;
  body.textContent = "";
  LOG_SHOWN = log.rows.length;
  LOG_FAILED = log.rows.filter((r) => r.status >= 400).length;
  paintLogTotals();
  if (!log.rows.length) {
    body.append(stateRow(8, LOGQ.error || LOGQ.model || LOGQ.provider
      ? "No requests match these filters. Clear them to see the whole range."
      : "No requests yet. Traffic through the proxy lands here.", "empty"));
    return;
  }
  for (const r of log.rows) body.append(logRow(r));
  if (log.next) body.append(logMoreRow(log.next));
}

/** Appends the page after the last one shown, under the same filter. */
async function loadOlderLog(offset) {
  const seq = USAGE_SEQ;
  const q = logQuery();
  q.set("offset", String(offset));
  let log;
  try {
    log = await api("/ui/api/log?" + q);
  } catch (err) {
    if (seq !== USAGE_SEQ) return;
    $("usage-error").textContent = err.message;
    $("usage-error").hidden = false;
    return;
  }
  if (seq !== USAGE_SEQ || !$("usage-rows")) return;
  if (log.error) {
    $("usage-error").textContent = log.error;
    $("usage-error").hidden = false;
    return;
  }
  const body = $("usage-rows");
  body.querySelector("tr.more")?.remove();
  for (const r of log.rows) body.append(logRow(r));
  if (log.next) body.append(logMoreRow(log.next));
  LOG_SHOWN += log.rows.length;
  LOG_FAILED += log.rows.filter((r) => r.status >= 400).length;
  paintLogTotals();
}

/* Pure-div bars, max-normalized; heights carry the data, title carries detail. */
function paintUsageChart(analytics) {
  const days = (analytics && analytics.days) || [];
  const chart = $("usage-days");
  if (!chart || !chart.isConnected) return;
  chart.textContent = "";
  $("usage-days-caption").textContent = "";
  $("usage-asof").textContent = days.length
    ? "last " + LOGQ.days + " days, as of " + days[days.length - 1].name
    : "last " + LOGQ.days + " days";
  if (!days.length) {
    chart.append(H("p", { class: "chart-empty", text: "No usage in this range yet." }));
    return;
  }
  const max = Math.max(...days.map((d) => Number(d.usd)), 1e-9);
  for (const d of days) {
    const pct = Math.max(Number(d.usd) / max * 100, Number(d.usd) > 0 ? 2 : 0.8);
    chart.append(H("div", { class: "bar",
      style: "height:" + pct.toFixed(1) + "%",
      title: d.name + " · $" + Number(d.usd).toFixed(6) + " · " +
        Number(d.requests).toLocaleString() + " req, " +
        Number(d.tokens).toLocaleString() + " tok" }));
  }
  $("usage-days-caption").append(
    H("span", { text: days[0].name }),
    H("span", { text: "max $" + max.toFixed(2) }),
    H("span", { text: days[days.length - 1].name }));
}

function paintRank(tbodyId, analytics) {
  const body = $(tbodyId);
  if (!body || !body.isConnected || !analytics) return;
  const rows = analytics[tbodyId.replace("usage-", "")]; // models | providers | combos
  if (!rows) return;
  body.textContent = "";
  if (!rows.length) {
    body.append(stateRow(4, "No usage in this range.", "empty"));
    return;
  }
  for (const r of rows) {
    body.append(H("tr", null,
      H("td", { text: r.name }),
      H("td", { class: "num", text: Number(r.requests).toLocaleString() }),
      H("td", { class: "num", text: Number(r.tokens).toLocaleString() }),
      H("td", { class: "num", text: "$" + Number(r.usd).toFixed(4) })));
  }
}

/* ── pane: resilience ─────────────────────────────────────────────────────── */

/* The failover rail on the dashboard shows the ORDER targets are tried in.
   This shows what that order currently MEANS: which targets are out of
   rotation, and for how long. Both are read from the router's own tables,
   so the panel reports the resolver's actual state rather than a guess. */

function paneResilience(root, st) {
  root.append(
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Breakers and cooldowns" }),
        H("span", { id: "res-asof", class: "hint" }),
        H("button", { id: "res-refresh", class: "btn ghost small", type: "button",
          text: "Refresh", onclick: () => loadResilience() })),
      H("div", { class: "block-body", style: "padding:0" },
        H("div", { class: "table-wrap" }, H("table", null,
          H("thead", null, H("tr", null,
            H("th", { scope: "col", text: "Provider" }),
            H("th", { scope: "col", text: "Breaker" }),
            H("th", { scope: "col", text: "Failures" }),
            H("th", { scope: "col", text: "Resets in" }),
            H("th", { scope: "col", text: "Cooldowns" }),
            H("th", { scope: "col", text: "Locks" }),
            H("th", { scope: "col", text: "Retired" }))),
          H("tbody", { id: "res-rows" })))),
      H("div", { class: "block-body" }, H("p", { id: "res-error", class: "error", role: "alert", hidden: true }))));
  loadResilience();
}

async function loadResilience() {
  const body = $("res-rows");
  if (!body) return;
  body.textContent = "";
  body.append(stateRow(7, "Loading the router's tables…", "loading"));
  let data;
  try {
    data = await api("/ui/api/resilience");
  } catch (err) {
    if (err instanceof NeedsToken) { showTokenGate(err.message); return; }
    body.textContent = "";
    body.append(stateRow(7, "Could not read the router's resilience tables.", "failed"));
    showError("res-error", err);
    return;
  }
  body.textContent = "";
  if (!body.isConnected) return; // the operator navigated away mid-flight
  $("res-asof").textContent = "as of " + asOf();
  const rows = data.providers || [];
  if (!rows.length) {
    body.append(stateRow(7, "No providers are configured, so there is nothing to be out of rotation.", "empty"));
    return;
  }
  const cooling = (list) => {
    if (!list || !list.length) return H("span", { class: "varname", text: "-" });
    return H("span", { class: "cooldowns" }, ...list.map((c) =>
      H("span", { class: "varname", text: (c.model || "provider") + " " + c.remaining_secs + "s" })));
  };
  for (const r of rows) {
    const breaker = r.breaker;
    const cls = breaker === "closed" ? "good" : breaker === "half_open" ? "warn" : "bad";
    const label = breaker === "closed" ? "closed" : breaker === "half_open" ? "half-open" : "open";
    body.append(H("tr", null,
      H("td", { text: r.provider }),
      H("td", null, H("span", { class: "status " + cls, text: label })),
      H("td", { class: "num", text: r.breaker_failures + " / " + r.breaker_threshold }),
      H("td", { class: "num", text: r.breaker === "closed" ? "-" : r.breaker_reset_secs + "s" }),
      H("td", null, cooling(r.coolings)),
      H("td", null, cooling(r.locks)),
      H("td", { class: "varname", text: r.retired ? "provider" : (r.retired_models || []).join(", ") || "-" })));
  }
}

/* ── pane: health ────────────────────────────────────────────────────────── */

/* Whether each credential is present, readable and bound to a live session.
   The store is asked, and its refusal is the answer  -  a credential that will
   not decrypt is BROKEN, which is a different fact from a missing one and
   needs different advice. */

function paneHealth(root, st) {
  root.append(
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Credentials" }),
        H("span", { id: "health-asof", class: "hint" }),
        H("button", { class: "btn ghost small", type: "button", text: "Refresh",
          onclick: () => loadHealth() })),
      H("div", { class: "block-body", style: "padding:0" },
        H("div", { class: "table-wrap" }, H("table", null,
          H("thead", null, H("tr", null,
            H("th", { scope: "col", text: "Provider" }),
            H("th", { scope: "col", text: "Binding" }),
            H("th", { scope: "col", text: "Env" }),
            H("th", { scope: "col", text: "Store" }))),
          H("tbody", { id: "health-rows" }))))),
    H("section", { class: "block" },
      H("div", { class: "block-head" }, H("h2", { text: "Sessions" })),
      H("div", { class: "block-body", style: "padding:0" },
        H("div", { class: "table-wrap" }, H("table", null,
          H("thead", null, H("tr", null,
            H("th", { scope: "col", text: "Provider" }),
            H("th", { scope: "col", text: "Kind" }),
            H("th", { scope: "col", text: "Terminal status" }),
            H("th", { scope: "col", text: "Reason" }))),
          H("tbody", { id: "session-rows" }))))),
    H("p", { id: "health-error", class: "error", role: "alert", hidden: true }));
  loadHealth();
}

async function loadHealth() {
  const body = $("health-rows");
  if (!body) return;
  body.textContent = "";
  body.append(stateRow(4, "Reading the credential store…", "loading"));
  let data;
  try {
    data = await api("/ui/api/health");
  } catch (err) {
    if (err instanceof NeedsToken) { showTokenGate(err.message); return; }
    body.textContent = "";
    body.append(stateRow(4, "Could not read the credential store.", "failed"));
    showError("health-error", err);
    return;
  }
  body.textContent = "";
  if (!body.isConnected) return; // the operator navigated away mid-flight
  $("health-asof").textContent = "as of " + asOf();
  const providers = data.providers || [];
  if (!providers.length) {
    body.append(stateRow(4, "No providers are configured in this file.", "empty"));
  }
  const storeClass = (s) => s === "ok" ? "good" : s === "missing" ? "warn" : "bad";
  const storeLabel = (s) => s === "ok" ? "readable" : s === "missing" ? "absent" : s;
  for (const p of providers) {
    for (const c of p.credentials || []) {
      body.append(H("tr", null,
        H("td", { text: p.id }),
        H("td", { class: "varname", text: c.ref + (c.var ? " → " + c.var : "") }),
        H("td", null, H("span", { class: "status " + (c.in_env ? "good" : "warn"),
          text: c.in_env ? "set" : "unset" })),
        H("td", null, H("span", { class: "status " + storeClass(c.store),
          text: storeLabel(c.store) }))));
    }
  }
  const sessions = $("session-rows");
  sessions.textContent = "";
  const list = data.sessions || [];
  if (!list.length) {
    sessions.append(stateRow(4, "No browser sessions are stored. API-key providers do not use one.", "empty"));
  }
  for (const s of list) {
    sessions.append(H("tr", null,
      H("td", { text: s.provider }),
      H("td", { class: "varname", text: s.kind }),
      H("td", { class: "num", text: s.terminal_status == null ? "-" : String(s.terminal_status) }),
      H("td", { class: "detail", text: s.terminal_reason || s.error || "-" })));
  }
}

/* ── pane: budgets ───────────────────────────────────────────────────────── */

/* Two questions on one screen: what a key has SPENT against its ceiling, and
   what a provider's own ALLOWANCE says. They are different budgets  -  one is
   ours, one is the provider's  -  and the panel says which is which. */

function paneBudgets(root, st) {
  root.append(
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Key budgets" }),
        H("span", { id: "budget-asof", class: "hint" }),
        H("button", { class: "btn ghost small", type: "button", text: "Refresh",
          onclick: () => loadBudgets() })),
      H("div", { class: "block-body", style: "padding:0" },
        H("div", { class: "table-wrap" }, H("table", null,
          H("thead", null, H("tr", null,
            H("th", { scope: "col", text: "Key" }),
            H("th", { scope: "col", class: "num", text: "Spent" }),
            H("th", { scope: "col", class: "num", text: "$ cap" }),
            H("th", { scope: "col", class: "num", text: "Tokens" }),
            H("th", { scope: "col", class: "num", text: "Token cap" }),
            H("th", { scope: "col", text: "Scope" }),
            H("th", { scope: "col", text: "Used" }))),
          H("tbody", { id: "budget-rows" }))))),
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Provider allowances" }),
        H("span", { class: "hint", text: "The freshest reading the credential store holds." })),
      H("div", { class: "block-body", style: "padding:0" },
        H("div", { class: "table-wrap" }, H("table", null,
          H("thead", null, H("tr", null,
            H("th", { scope: "col", text: "Provider" }),
            H("th", { scope: "col", class: "num", text: "Remaining" }),
            H("th", { scope: "col", text: "Resets" }),
            H("th", { scope: "col", text: "Reading" }))),
          H("tbody", { id: "quota-rows" }))))),
    H("p", { id: "budget-error", class: "error", role: "alert", hidden: true }));
  loadBudgets();
}

async function loadBudgets() {
  const body = $("budget-rows");
  if (!body) return;
  body.textContent = "";
  body.append(stateRow(7, "Reading the ledger and the store…", "loading"));
  let data;
  try {
    data = await api("/ui/api/budgets");
  } catch (err) {
    if (err instanceof NeedsToken) { showTokenGate(err.message); return; }
    body.textContent = "";
    body.append(stateRow(7, "Could not read the budgets.", "failed"));
    showError("budget-error", err);
    return;
  }
  body.textContent = "";
  if (!body.isConnected) return; // the operator navigated away mid-flight
  $("budget-asof").textContent = "as of " + asOf();
  const rows = data.keys || [];
  if (!rows.length) {
    body.append(stateRow(7, "No key has a ceiling row of its own.", "empty"));
  }
  // The default row governs every key without one, so it is drawn FIRST and
  // named. Leaving it out while claiming "every key is unrestricted" would
  // be false the moment a `limits.default` exists.
  const def = data.default;
  if (def && (def.rpm != null || def.usd_cap != null || def.token_cap != null)) {
    body.append(H("tr", { class: "default-row" },
      H("td", { text: "every other key" }),
      H("td", { class: "num", text: "-" }),
      H("td", { class: "num", text: def.usd_cap == null ? "unset" : "$" + def.usd_cap / 1000000 }),
      H("td", { class: "num", text: "-" }),
      H("td", { class: "num", text: def.token_cap == null ? "unset" : Number(def.token_cap).toLocaleString() }),
      H("td", { class: "varname", text: def.model || "all" }),
      H("td", { class: "varname", text: "the default ceiling" })));
  }
  // The "used" column reports BOTH ceilings, each against its own arm.
  // Falling back from the dollar percentage to the token percentage would
  // conflate two different budgets into one number, and an operator reading
  // "40%" would not know which ceiling it referred to.
  const usedCell = (r) => {
    const parts = [];
    if (r.usd_pct != null) {
      parts.push(H("span", { class: "status " + pctClass(r.usd_pct), text: "$ " + r.usd_pct.toFixed(0) + "%" }));
    }
    if (r.token_pct != null) {
      parts.push(H("span", { class: "status " + pctClass(r.token_pct), text: r.token_pct.toFixed(0) + "% tok" }));
    }
    if (!parts.length) return H("span", { class: "varname", text: "no ceiling" });
    return H("span", { class: "used-cell" }, ...parts);
  };
  const pctClass = (pct) => (pct >= 90 ? "bad" : pct >= 60 ? "warn" : "good");
  for (const r of rows) {
    body.append(H("tr", null,
      H("td", { text: r.key }),
      H("td", { class: "num", text: "$" + Number(r.usd).toFixed(6) }),
      H("td", { class: "num", text: r.usd_cap == null ? "unset" : "$" + r.usd_cap / 1000000 }),
      H("td", { class: "num", text: Number(r.tokens).toLocaleString() }),
      H("td", { class: "num", text: r.token_cap == null ? "unset" : Number(r.token_cap).toLocaleString() }),
      H("td", { class: "varname", text: r.model || "all" }),
      H("td", null, usedCell(r))));
  }
  const quotas = $("quota-rows");
  quotas.textContent = "";
  const list = data.quotas || [];
  if (!list.length) {
    quotas.append(stateRow(4, "No provider has reported an allowance. The store holds no quota reading yet.", "empty"));
  }
  for (const q of list) {
    const reset = q.next_reset_at
      ? new Date(q.next_reset_at * 1000).toLocaleString()
      : "-";
    quotas.append(H("tr", null,
      H("td", { text: q.provider }),
      H("td", { class: "num", text: q.remaining_pct == null ? "-" : (q.remaining_pct * 100).toFixed(0) + "%" }),
      H("td", { class: "varname", text: reset }),
      H("td", { class: "varname", text: (q.stale ? "stale" : "fresh") + (q.exhausted ? " - EXHAUSTED" : "") })));
  }
}

/* ── pane: settings ──────────────────────────────────────────────────────── */

/* The yaml the proxy boots from. Two kinds of setting live in two different
   places, and the pane says which is which: the yaml-backed ones are
   editable here, and the environment-backed ones are shown with the variable
   that carries them, because a form that wrote them to the yaml would edit
   a file nothing reads. */

function paneSettings(root, st) {
  root.append(
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Server" }),
        H("span", { class: "hint", text: "Written to config.yaml. The proxy applies it at boot." })),
      H("div", { class: "block-body" },
        H("form", { id: "settings-form", class: "row-form" },
          H("label", { for: "set-host", text: "Host" }),
          H("input", { id: "set-host", spellcheck: "false", style: "width:160px" }),
          H("label", { for: "set-port", text: "Port" }),
          H("input", { id: "set-port", type: "number", min: "1", max: "65535", style: "width:100px" }),
          H("button", { type: "submit", class: "btn", text: "Save server" })),
        H("p", { id: "settings-error", class: "error", role: "alert", hidden: true }))),
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Default ceilings" }),
        H("span", { class: "hint", text: "The ceiling every key without a row of its own inherits." })),
      H("div", { class: "block-body" },
        H("form", { id: "limits-default-form", class: "row-form" },
          H("label", { for: "def-rpm", text: "rpm" }),
          H("input", { id: "def-rpm", type: "number", min: "1", placeholder: "unset", style: "width:100px" }),
          H("label", { for: "def-usd", text: "$ cap (µ$)" }),
          H("input", { id: "def-usd", type: "number", min: "0", placeholder: "unset", style: "width:120px" }),
          H("label", { for: "def-tokens", text: "token cap" }),
          H("input", { id: "def-tokens", type: "number", min: "0", placeholder: "unset", style: "width:120px" }),
          H("label", { for: "def-unpriced", text: "refuse unpriced" }),
          H("input", { id: "def-unpriced", type: "checkbox" }),
          H("button", { type: "submit", class: "btn", text: "Save ceilings" })))),
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Environment-backed" }),
        H("span", { class: "hint", text: "Read from the environment at boot. Edit the variable, then restart." })),
      H("div", { class: "block-body", style: "padding:0" },
        H("div", { class: "table-wrap" }, H("table", null,
          H("thead", null, H("tr", null,
            H("th", { scope: "col", text: "Setting" }),
            H("th", { scope: "col", text: "Value" }),
            H("th", { scope: "col", text: "Variable" }))),
          H("tbody", { id: "env-rows" }))))));
  loadSettings();
  // The form exists only now, so the submit listeners are attached here and
  // not at boot: at boot `#settings-form` is null, the listener was never
  // attached, and "Save server" did nothing.
  wireSettings();
}

async function loadSettings() {
  let data;
  try {
    data = await api("/ui/api/settings");
  } catch (err) {
    if (err instanceof NeedsToken) { showTokenGate(err.message); return; }
    showError("settings-error", err);
    return;
  }
  // The operator navigated away while this was in flight: the pane's fields
  // are gone, and writing to them would be a crash on a page that is fine.
  if (!$("set-host")) return;
  $("set-host").value = data.host || "";
  $("set-port").value = data.port || "";
  const def = (data.limits && data.limits.default) || {};
  $("def-rpm").value = def.rpm == null ? "" : def.rpm;
  $("def-usd").value = def.usd_micros == null ? "" : def.usd_micros;
  $("def-tokens").value = def.tokens == null ? "" : def.tokens;
  $("def-unpriced").checked = !!def.refuse_unpriced;

  const env = $("env-rows");
  env.textContent = "";
  const timeouts = data.timeouts || {};
  // These are read AT BOOT from the environment, not from the yaml. The
  // column says so, so the operator does not look for a yaml key that is
  // not there.
  const rows = [
    ["Model deadlines", Object.keys(timeouts).length
      ? Object.entries(timeouts).map(([k, v]) => k + " " + v + "s").join(", ")
      : "none", "AR_STREAM_TIMEOUT_SECS"],
    ["Request timeout", data.request_timeout_secs + "s", "(built in)"],
    ["Model discovery", data.model_discovery ? "on" : "off", "AR_MODEL_DISCOVERY"],
    ["Auth mode", data.auth_mode || "required", "AR_AUTH_MODE"],
  ];
  for (const [label, value, variable] of rows) {
    env.append(H("tr", null,
      H("td", { text: label }),
      H("td", { class: "varname", text: String(value) }),
      H("td", { class: "varname", text: variable + " (read at boot)" })));
  }
}

function wireSettings() {
  const form = $("settings-form");
  if (form) form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const host = $("set-host").value.trim();
    const port = Number($("set-port").value);
    try {
      await api("/ui/api/settings", { server: { host, port } });
      await refresh();
    } catch (err) { showError("settings-error", err); }
  });
  const limits = $("limits-default-form");
  if (limits) limits.addEventListener("submit", async (e) => {
    e.preventDefault();
    const num = (id) => $(id).value === "" ? null : Number($(id).value);
    try {
      await api("/ui/api/settings", { limits: { default: {
        rpm: num("def-rpm"), usd_micros: num("def-usd"), tokens: num("def-tokens"),
        refuse_unpriced: $("def-unpriced").checked,
      } } });
      await refresh();
    } catch (err) { showError("settings-error", err); }
  });
}

/* ── boot ─────────────────────────────────────────────────────────────────── */

$("copy-restart").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText($("banner-cmd").textContent);
    $("copy-restart").textContent = "Copied";
    setTimeout(() => { $("copy-restart").textContent = "Copy command"; }, 1500);
  } catch { /* the command stays visible to copy by hand */ }
});

window.addEventListener("hashchange", route);

/* The panel's token prompt. A `?token=` on the URL is accepted once, moved
   into sessionStorage and stripped from the address bar  -  a token left in a
   URL ends up in the browser history and in the Referer of the next
   navigation. */
takeTokenFromUrl();

$("gate-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const value = $("gate-token").value.trim();
  if (!value) return;
  setToken(value);
  $("gate-token").value = "";
  await refresh();
});

/* The dashboard's numbers move while it sits open. Only the spend and usage totals
 * are refreshed, never the whole view, so a form the operator is typing into survives. */
setInterval(async () => {
  if (document.hidden || currentView() !== "dashboard") return;
  await fillUsage().catch(() => {});
  const totals = document.querySelector("#view .totals");
  if (totals) totals.replaceChildren(usageSummary());
}, 15000);

refresh().catch(() => {
  const root = $("view");
  root.textContent = "";
  root.append(H("p", { class: "error", role: "alert",
    text: "The page could not read /ui/api/state. Is the proxy running?" }));
});

/* ── pane: playground ───────────────────────────────────────────────────── */

let PG_ABORT = null;

/** Conversation so far, sent whole each turn. Memory only: gone on reload. */
let PG_MESSAGES = [];

/** One transcript line: a role tag, then the text. */
function pgTurn(role, content) {
  return H("div", { class: "pg-turn " + role },
    H("span", { class: "pg-role", text: role }),
    H("span", { class: "pg-text", text: content }));
}

function pgRender() {
  const out = $("pg-out");
  if (!out) return;
  out.textContent = "";
  for (const m of PG_MESSAGES) out.append(pgTurn(m.role, m.content));
}

function panePlayground(root, st) {
  const ids = (st.combos || []).map((c) => c.id);
  root.append(
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Playground" }),
        H("span", { class: "hint", text: "Same-origin chat completions. History lives in this tab only." })),
      H("div", { class: "block-body" },
        H("form", { id: "pg-form", class: "row-form" },
          H("label", { for: "pg-combo", text: "Combo" }),
          H("select", { id: "pg-combo" }, ...ids.map((id) => H("option", { value: id, text: id }))),
          H("label", { for: "pg-token", text: "Bearer" }),
          H("input", { id: "pg-token", type: "password", autocomplete: "off", placeholder: "minted token (optional)", style: "width:auto" }),
          H("label", { for: "pg-stream", text: "stream" }),
          H("input", { id: "pg-stream", type: "checkbox", checked: "checked" }),
          H("button", { type: "submit", class: "btn btn-primary", "data-cta": "primary", text: "Send" }),
          H("button", { type: "button", class: "btn ghost small", id: "pg-stop", text: "Stop", disabled: "disabled" }),
          H("button", { type: "button", class: "btn ghost small", id: "pg-clear", text: "Clear" })),
        H("label", { for: "pg-system", class: "pg-label", text: "System prompt (optional)" }),
        H("textarea", { id: "pg-system", rows: "2", spellcheck: "false", class: "pg-area",
          placeholder: "You are…" }),
        H("label", { for: "pg-prompt", class: "pg-label", text: "Message" }),
        H("textarea", { id: "pg-prompt", rows: "4", spellcheck: "false", class: "pg-area",
          placeholder: "Ask the gateway something…" }),
        H("p", { id: "pg-error", class: "error", role: "alert", hidden: true }))),
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Conversation" }),
        H("span", { id: "pg-meta", class: "hint" })),
      H("div", { class: "block-body pg-out", id: "pg-out", "aria-live": "polite" })));
  if (PG_TOKEN) $("pg-token").value = PG_TOKEN;
  pgRender();
  wirePlayground();
}

function wirePlayground() {
  $("pg-token").addEventListener("input", (e) => { PG_TOKEN = e.target.value; });
  $("pg-clear").addEventListener("click", () => {
    if (PG_ABORT) PG_ABORT.abort();
    PG_MESSAGES = [];
    pgRender();
    $("pg-meta").textContent = "";
    $("pg-error").hidden = true;
  });
  $("pg-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const btn = e.target.querySelector("button[type=submit]");
    const combo = $("pg-combo").value;
    const prompt = $("pg-prompt").value.trim();
    if (!prompt) return;
    const system = $("pg-system").value.trim();
    const wantStream = $("pg-stream").checked;
    const meta = $("pg-meta");
    const turn = { role: "user", content: prompt };
    pgRender();
    const out = $("pg-out");
    const live = pgTurn("assistant", "");
    out.append(pgTurn("user", prompt), live);
    const liveText = live.querySelector(".pg-text");
    meta.textContent = wantStream ? "streaming…" : "waiting…";
    $("pg-error").hidden = true;
    PG_ABORT = new AbortController();
    $("pg-stop").disabled = false;
    btn.disabled = true;
    const headers = { "content-type": "application/json" };
    if ($("pg-token").value.trim()) headers.authorization = "Bearer " + $("pg-token").value.trim();
    const messages = [...(system ? [{ role: "system", content: system }] : []), ...PG_MESSAGES, turn];
    const body = { model: combo, messages, stream: wantStream };
    if (wantStream) body.stream_options = { include_usage: true };
    let text = "";
    const push = (chunk) => { text += chunk; liveText.textContent = text; out.scrollTop = out.scrollHeight; };
    const finish = () => { btn.disabled = false; $("pg-stop").disabled = true; PG_ABORT = null; };
    try {
      const res = await fetch("/v1/chat/completions", {
        method: "POST", headers, body: JSON.stringify(body), signal: PG_ABORT.signal,
      });
      if (!res.ok) {
        const data = await res.json().catch(() => ({}));
        const msg = typeof data.error === "object" && data.error ? data.error.message : data.error;
        throw Object.assign(new Error(msg || res.statusText), { data });
      }
      if (!wantStream) {
        const data = await res.json();
        push(data.choices && data.choices[0] ? data.choices[0].message.content : "");
        const u = data.usage || {};
        meta.textContent = (data.model || combo) + " · " + (u.prompt_tokens || 0) + " in / " + (u.completion_tokens || 0) + " out";
        return;
      }
      const reader = res.body.getReader();
      const dec = new TextDecoder();
      let buf = "";
      let usage = null;
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buf += dec.decode(value, { stream: true });
        let at;
        while ((at = buf.indexOf("\n\n")) >= 0) {
          const ev = buf.slice(0, at).trim();
          buf = buf.slice(at + 2);
          for (const line of ev.split("\n")) {
            if (!line.startsWith("data:")) continue;
            const payload = line.slice(5).trim();
            if (payload === "[DONE]") continue;
            const json = JSON.parse(payload);
            if (json.error) throw new Error(json.error.message || JSON.stringify(json.error));
            if (json.usage) usage = json.usage;
            const choice = json.choices && json.choices[0];
            if (choice && choice.delta && choice.delta.content) push(choice.delta.content);
            if (choice && choice.message && choice.message.content) push(choice.message.content);
          }
        }
      }
      meta.textContent = combo + (usage ? " · " + usage.prompt_tokens + " in / " + usage.completion_tokens + " out" : " · done");
    } catch (err) {
      if (err.name === "AbortError") {
        meta.textContent = "stopped";
      } else {
        $("pg-error").textContent = err.message + (err.data && err.data.help ? " - " + err.data.help : "");
        $("pg-error").hidden = false;
      }
    } finally {
      // Keep the turn only when something came back; a failed send leaves the
      // prompt in the box for a retry and the history untouched.
      if (text) {
        PG_MESSAGES.push(turn, { role: "assistant", content: text });
        $("pg-prompt").value = "";
      } else {
        live.remove();
        out.lastElementChild && out.lastElementChild.remove();
      }
      finish();
    }
  });
  $("pg-stop").addEventListener("click", () => {
    if (PG_ABORT) PG_ABORT.abort();
  });
}
