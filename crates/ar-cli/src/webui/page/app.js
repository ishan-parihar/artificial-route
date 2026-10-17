/* aroute control page. Vanilla JS on purpose: no build step, no remote
   fetches, works on an offline VPS. Phase A shell: hash-routed panes over
   one state snapshot; single-threaded DOM through the H() builder keeps
   everything XHR-safe with zero deps. */

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

async function api(path, body) {
  const opts = body === undefined
    ? {}
    : { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) };
  const res = await fetch(path, opts);
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw Object.assign(new Error(data.error || res.statusText), { data });
  return data;
}

function showError(id, err) {
  const p = $(id);
  p.textContent = err.message + (err.data && err.data.help ? " - " + err.data.help : "");
  p.hidden = false;
  setTimeout(() => { p.hidden = true; }, 8000);
}

/* ── chrome (sidebar, banner) ─────────────────────────────────────────────── */

function renderChrome(st) {
  STATE = st;
  $("ver").textContent = st.bin + " " + st.version;
  const gate = $("gate-badge");
  gate.textContent = st.gate_armed ? "gate armed" : "gate unarmed";
  gate.className = "badge " + (st.gate_armed ? "on" : "off");
  $("banner").hidden = !st.pending_restart;
  $("foot-config").textContent = st.config_path;
  $("foot-env").textContent = st.env_path;
}

/* ── routing ──────────────────────────────────────────────────────────────── */

const VIEWS = { dashboard: paneDashboard, combos: paneCombos, providers: paneProviders, keys: paneKeys, usage: paneUsage, playground: panePlayground };
const TITLES = {
  dashboard: ["Dashboard", "What the router is serving, on one screen."],
  combos: ["Routing combos", "A combo is the model name a client asks for. Targets are tried in the strategy's order."],
  providers: ["Providers", "Credentials go to the env file the service loads. The value never appears on this page again."],
  keys: ["Client keys", "Tokens minted from the gate master, verified on every request. The token shows once."],
  usage: ["Usage", "Straight from the ledger the proxy bills against, with the requests that failed beside it."],
  playground: ["Playground", "Chat completions through the real gateway, same origin. Streams natively; no proxy in between."],
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

async function refresh() {
  renderChrome(await api("/ui/api/state"));
  await route();
  await fillUsage(); // stats on dashboard + rows on usage pane
}

/* ── usage fetch (dashboard band + usage pane) ────────────────────────────── */

let USAGE = null;
async function fillUsage() {
  USAGE = await api("/ui/api/usage").catch(() => null);
  const el = $("st-spend");
  if (el && USAGE) el.textContent = "$" + Number(USAGE.totals.usd).toFixed(4);
}

/* ── pane: dashboard ──────────────────────────────────────────────────────── */

function stat(label, valueId) {
  return H("div", { class: "stat" },
    H("dt", { text: label }), H("dd", { id: valueId, text: "-" }));
}

function paneDashboard(root, st) {
  root.append(
    H("section", { class: "stats", "aria-label": "Status" },
      stat("Providers", "st-providers"),
      stat("Combos", "st-combos"),
      stat("Client keys", "st-keys"),
      stat("Spend, all keys", "st-spend")),
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
      H("div", { class: "block-head" }, H("h2", { text: "Configured" })),
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
      H("div", { class: "block-body", id: "catalog-body" })));
  renderProviders(st);
  wireProviderAdd();
  wireProviderCustom();
  wireProviderActions();
  wireSetKeys();
  loadCatalog();
}

/* The compiled-in registry: one GET, rendered per provider on demand. */

async function loadCatalog() {
  const host = $("catalog-body");
  let data;
  try {
    data = await api("/ui/api/models");
  } catch (err) {
    host.append(H("p", { class: "error", text: err.message }));
    return;
  }
  const providers = data.providers || [];
  $("catalog-head").textContent = "Registry catalog (" + providers.length + ")";
  const frag = providers.map((p) => H("details", { class: "catalog-row", dataset: { pid: p.id } },
    H("summary", null,
      H("span", { class: "cta", text: p.configured ? "configured" : "" }),
      H("strong", { text: p.id }),
      H("span", { class: "varname", text: " " + p.wire + " · " + p.models.length + " models " }),
      H("button", { type: "button", class: "btn ghost small", text: "use",
        dataset: { useId: p.id } })),
    H("div", { class: "keycell" }, ...p.models.map((m) =>
      H("span", { class: "varname", text: m.id + (m.in == null ? "" : " $" + m.in + "/$" + m.out + " per mtok") })))));
  host.append(...frag);
  $("catalog-filter").addEventListener("input", (e) => {
    const q = e.target.value.toLowerCase();
    for (const row of host.children) {
      const hit = row.textContent.toLowerCase().includes(q);
      row.hidden = !hit;
    }
  });
  $("catalog-body").addEventListener("click", (e) => {
    const id = e.target && e.target.dataset ? e.target.dataset.useId : null;
    if (id) { $("new-provider").value = id; }
  });
}

function renderProviders(st) {
  const body = $("provider-rows");
  if (!body) return;
  body.textContent = "";
  for (const p of st.providers) {
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
          H("label", { for: "mint-rpm", text: "rpm" }),
          H("input", { id: "mint-rpm", type: "number", min: "1", max: "100000", placeholder: "unset", style: "width:90px" }),
          H("button", { type: "submit", class: "btn btn-primary", "data-cta": "primary", text: "Mint key" })),
        H("details", { id: "minted", class: "minted", hidden: true, open: "open" },
          H("summary", null, "Token for ", H("span", { id: "minted-id" }), " - copy it now"),
          H("code", { id: "minted-token" }),
          H("button", { id: "minted-use", type: "button", class: "btn small ghost", text: "Use in playground",
            onclick: () => { if (MINTED) { PG_TOKEN = MINTED.access; location.hash = "#/playground"; } } })),
        H("p", { id: "keys-error", class: "error", role: "alert", hidden: true }))),
    H("section", { class: "block" },
      H("div", { class: "block-head" },
        H("h2", { text: "Limits" }),
        H("span", { class: "hint", text: "Per-key ceilings in `limits:`, enforced at the edge. Blank an arm to clear it; all-clear removes the row. A combo scope limits the spend arms to requests for that combo." })),
      H("div", { class: "block-body" },
        H("div", { class: "table-wrap" }, H("table", null,
          H("thead", null, H("tr", null,
            H("th", { text: "Key" }), H("th", { text: "rpm" }), H("th", { text: "$ cap" }),
            H("th", { text: "token cap" }), H("th", { text: "unpriced" }), H("th", { text: "scope" }), H("th", { text: "Spend, all time" }),
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
}

function renderKeys(st) {
  const rows = (st.limits && st.limits.keys) || {};
  const body = $("key-rows");
  body.textContent = "";
  const names = Object.keys(rows);
  if (!names.length) {
    body.append(H("tr", null, H("td", { colspan: "8", class: "varname",
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
    const unpriced = H("input", { type: "checkbox", "aria-label": "refuse_unpriced for " + name });
    unpriced.checked = !!cap.refuse_unpriced;
    const scope = H("input", { "aria-label": "combo scope for " + name, placeholder: "all combos", spellcheck: "false", autocomplete: "off", style: "width:110px" });
    scope.value = cap.model || "";
    body.append(H("tr", null,
      H("td", { text: name }),
      H("td", { class: "num", text: cap.rpm == null ? "unset" : String(cap.rpm) }),
      H("td", { class: "num", text: cap.usd_micros == null ? "unset" : "$" + cap.usd_micros / 1000000 }),
      H("td", { class: "num", text: cap.tokens == null ? "unset" : String(cap.tokens) }),
      H("td", { class: "num", text: cap.refuse_unpriced ? "refuse" : "allow" }),
      H("td", { class: "varname", text: cap.model || "all" }),
      H("td", { class: "num", text: "$" + spend.toFixed(6) }),
      H("td", null, H("form", { class: "row-form", onsubmit: async (e) => {
        e.preventDefault();
        const send = (input) => input.type === "checkbox" ? input.checked : (input.value === "" ? null : Number(input.value));
        try {
          await api("/ui/api/limits", { key: name,
            rpm: send(rpmInput), usd_micros: send(usdInput), tokens: send(tokensInput),
            refuse_unpriced: send(unpriced), model: scope.value.trim() || null });
          await refresh();
        } catch (err) { showError("keys-error", err); }
      } }, rpmInput, usdInput, tokensInput, unpriced, scope, H("button", { class: "btn small", type: "submit", text: "Set" })))));
  }
}

function wireMint() {
  $("mint-form").addEventListener("submit", async (e) => {
    e.preventDefault();
    const key_id = $("mint-id").value.trim();
    const rpm = $("mint-rpm").value === "" ? null : Number($("mint-rpm").value);
    try {
      const res = await api("/ui/api/keys/mint", { key_id });
      if (rpm != null) await api("/ui/api/limits", { key: key_id, rpm });
      MINTED = { key_id: res.key_id, access: res.access };
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
  paintSuggestions(usage.analytics, log.rows);
  paintLog(log);
}

function paintSuggestions(analytics, rows) {
  const fill = (id, names) => {
    const dl = $(id);
    dl.textContent = "";
    for (const n of new Set(names)) if (n && n !== "-") dl.append(H("option", { value: n }));
  };
  fill("lg-models", [...(analytics.models || []).map((m) => m.name), ...rows.map((r) => r.model)]);
  fill("lg-providers", [...(analytics.providers || []).map((p) => p.name), ...rows.map((r) => r.provider)]);
}

const TIME_FMT = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });

function paintLog(log) {
  const body = $("usage-rows");
  body.textContent = "";
  const failed = log.rows.filter((r) => r.status >= 400).length;
  $("usage-totals").textContent = log.rows.length
    ? "Showing " + log.rows.length + (log.rows.length >= log.limit ? " (newest " + log.limit + ")" : "") +
      " · " + failed + " failed"
    : "";
  if (!log.rows.length) {
    body.append(stateRow(8, LOGQ.error || LOGQ.model || LOGQ.provider
      ? "No requests match these filters. Clear them to see the whole range."
      : "No requests yet. Traffic through the proxy lands here.", "empty"));
    return;
  }
  for (const r of log.rows) {
    const when = new Date(r.at * 1000);
    const bad = r.status >= 400;
    body.append(H("tr", { class: bad ? "failed" : "" },
      H("td", { class: "num", text: TIME_FMT.format(when), title: when.toISOString() }),
      H("td", { text: r.key }),
      H("td", { text: r.provider }),
      H("td", { text: r.model }),
      H("td", null, H("span", { class: "status " + (bad ? "bad" : "good"), text: bad ? r.status + " error" : "ok" })),
      H("td", { class: "num", text: Number(r.tokens).toLocaleString() }),
      H("td", { class: "num", text: bad ? "-" : r.priced ? "$" + Number(r.usd).toFixed(6) : "unpriced" }),
      H("td", { class: "detail", text: r.error, title: r.error })));
  }
}

/* Pure-div bars, max-normalized; heights carry the data, title carries detail. */
function paintUsageChart(analytics) {
  const days = (analytics && analytics.days) || [];
  const chart = $("usage-days");
  if (!chart) return;
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
  if (!body || !analytics) return;
  const rows = analytics[tbodyId.replace("usage-", "")]; // models | providers
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

/* ── boot ─────────────────────────────────────────────────────────────────── */

$("copy-restart").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText($("banner-cmd").textContent);
    $("copy-restart").textContent = "Copied";
    setTimeout(() => { $("copy-restart").textContent = "Copy command"; }, 1500);
  } catch { /* the command stays visible to copy by hand */ }
});

window.addEventListener("hashchange", route);
refresh().catch(() => {
  const root = $("view");
  root.textContent = "";
  root.append(H("p", { class: "error", text: "The page could not read /ui/api/state. Is the proxy running?" }));
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
