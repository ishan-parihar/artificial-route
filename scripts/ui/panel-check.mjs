// Behaviour check for the /ui control panel: every pane, the panel's own
// gate, and the settings save.
//
// Twenty checks; exits 1 when any fails.
//
// Not run in CI. It drives a real browser, so run it by hand against a
// server you started for it:
//
//   npm i playwright && npx playwright install chromium
//   aroute --config <demo config.yaml> serve        # its own port
//   node scripts/ui/panel-check.mjs http://127.0.0.1:<port>
//
// It needs a config with at least one provider and one combo. It writes
// nothing except a settings POST, so point it at a scratch config, never a
// production one: `POST /ui/api/settings` rewrites the `server:` and
// `limits:` sections of the file.
//
// The ARMED counterpart is `panel-armed-check.mjs`, which needs a token and
// an armed gate.
import { chromium } from "playwright";

const B = process.argv[2] || "http://127.0.0.1:20128";
const results = [];
function ok(name, pass, detail = "") {
  results.push({ name, pass });
  console.log(`${pass ? "PASS " : "FAIL "} ${name}${detail ? "  — " + detail : ""}`);
}

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
const errors = [];
const settingsPosts = [];
page.on("pageerror", (e) => errors.push(String(e)));
page.on("console", (m) => {
  // A refused write is a 4xx the panel HANDLES and shows — the browser logs
  // it as a console error, but it is the panel working, not the panel broken.
  const text = m.text();
  if (m.type() === "error" && !/Failed to load resource.*\b(4\d\d|5\d\d)\b/.test(text)) {
    errors.push(text);
  }
});
// Catch the settings save so the check can prove the button is WIRED. A dead
// button is invisible to a probe: it renders, it is the right size, it has
// the right label, and clicking it does nothing.
await page.route("**/ui/api/settings", async (route) => {
  if (route.request().method() === "POST") {
    settingsPosts.push(route.request().postData());
  }
  await route.continue();
});

// ── the panel renders ──────────────────────────────────────────────────────
await page.goto(`${B}/ui/#/dashboard`, { waitUntil: "networkidle" });
await page.waitForSelector("#view .stats", { timeout: 15000 });
ok("dashboard renders its stat band", (await page.locator("#view .stat").count()) >= 4);
ok("the sidebar carries every view", (await page.locator("#nav a").count()) === 10);

// ── every pane loads its OWN data ─────────────────────────────────────────
for (const [view, marker, label] of [
  ["resilience", "#res-rows tr", "resilience view"],
  ["health", "#health-rows tr", "health view"],
  ["budgets", "#budget-rows tr", "budgets view"],
  ["settings", "#env-rows tr", "settings view"],
]) {
  await page.goto(`${B}/ui/#/${view}`, { waitUntil: "networkidle" });
  await page.waitForSelector(marker, { timeout: 15000 });
  ok(`${label} renders rows`, (await page.locator(marker).count()) > 0);
}

// ── the settings view names the env-backed variables ──────────────────────
await page.goto(`${B}/ui/#/settings`, { waitUntil: "networkidle" });
await page.waitForSelector("#env-rows tr");
const envText = await page.locator("#env-rows").innerText();
ok("settings names the env-backed variables",
  envText.includes("AR_STREAM_TIMEOUT_SECS") &&
  envText.includes("AR_MODEL_DISCOVERY") &&
  envText.includes("AR_AUTH_MODE"));
ok("settings marks the boot-read values", envText.includes("read at boot"));

// ── the settings Save button is WIRED ─────────────────────────────────────
ok("the settings pane has a Save server button",
  (await page.locator("#settings-form button[type=submit]").count()) === 1);
await page.click("#settings-form button[type=submit]");
await page.waitForTimeout(1200);
ok("clicking Save server actually posts", settingsPosts.length > 0,
  `${settingsPosts.length} POST(s)`);

// ── the budgets view does not over-claim ──────────────────────────────────
await page.goto(`${B}/ui/#/budgets`, { waitUntil: "networkidle" });
await page.waitForSelector("#budget-rows tr");
const budgetText = await page.locator("#budget-rows").innerText();
ok("the budgets view does not claim every key is unrestricted",
  !budgetText.includes("Every key is unrestricted"));

// ── the usage view shows the per-combo table ──────────────────────────────
await page.goto(`${B}/ui/#/usage`, { waitUntil: "networkidle" });
await page.waitForSelector("#usage-combos", { timeout: 15000 });
ok("usage carries a per-combo block", (await page.locator("#usage-combos").count()) === 1);

// ── a refused write says WHY, and persists ────────────────────────────────
// The panel's own gate is unarmed on a default install, so the write path
// is reachable; the mint endpoint answers 409 (no store) or 403 (not
// armed), which proves the request reached the handler rather than a guard.
await page.goto(`${B}/ui/#/keys`, { waitUntil: "networkidle" });
await page.waitForSelector("#mint-form", { timeout: 15000 });
await page.fill("#mint-id", "panel-check");
await page.click("#mint-form button[type=submit]");
await page.waitForSelector("#keys-error:not([hidden])", { timeout: 10000 }).catch(() => {});
const errText = await page.locator("#keys-error").innerText().catch(() => "");
ok("a refused write leaves a persistent message", errText.length > 0, errText.slice(0, 90));

// The old panel hid the error after 8 seconds. A failed write that vanishes
// before it is read is indistinguishable from one that succeeded.
await page.waitForTimeout(9000);
ok("the error is still there after 9 seconds", await page.locator("#keys-error").isVisible());

// ── the token gate: a ?token= is taken and stripped from the URL ─────────
// BOTH url forms, because an operator will paste one with a trailing slash.
for (const [url, label] of [
  [`${B}/ui/?token=probe-token#dashboard`, "trailing slash"],
  [`${B}/ui?token=probe-token#dashboard`, "no trailing slash"],
]) {
  await page.goto(url, { waitUntil: "networkidle" });
  await page.waitForTimeout(600);
  const after = page.url();
  ok(`the token is stripped from the URL (${label})`, !after.includes("token="), after);
  const stored = await page.evaluate(() => sessionStorage.getItem("ar-panel-token"));
  ok(`the token is kept in sessionStorage (${label})`, stored === "probe-token");
}

// ── the page has no console errors ────────────────────────────────────────
ok("no console errors", errors.length === 0, errors.join(" | "));

await browser.close();
const failed = results.filter((r) => !r.pass);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
process.exit(failed.length ? 1 : 0);
