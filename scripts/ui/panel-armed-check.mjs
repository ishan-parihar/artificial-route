// Behaviour check for the /ui control panel on an ARMED gate: the panel must
// admit a valid token and refuse everything else, and the token prompt must
// be the way back in.
//
// Ten checks; exits 1 when any fails.
//
// Not run in CI. It drives a real browser, so run it by hand against a
// server you started for it, with a gate armed:
//
//   aroute --config <demo config.yaml> keys arm-gate --key-id probe
//   aroute --config <demo config.yaml> serve
//   TOKEN=$(aroute --config <demo config.yaml> keys mint --key-id probe \
//            | sed -n 's/^token: //p')
//   node scripts/ui/panel-armed-check.mjs http://127.0.0.1:<port> "$TOKEN"
//
// The unarmed counterpart is `panel-check.mjs`.
import { chromium } from "playwright";

const B = process.argv[2] || "http://127.0.0.1:20128";
const TOKEN = process.argv[3];
if (!TOKEN) {
  console.error("usage: panel-armed-check.mjs <base-url> <token>");
  process.exit(2);
}
const results = [];
function ok(name, pass, detail = "") {
  results.push({ name, pass });
  console.log(`${pass ? "PASS " : "FAIL "} ${name}${detail ? "  — " + detail : ""}`);
}

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });

// ── no credential: the API refuses and the prompt appears ─────────────────
await page.goto(`${B}/ui/#/dashboard`, { waitUntil: "networkidle" });
await page.waitForSelector("#gate:not([hidden])", { timeout: 15000 });
ok("an armed panel asks for a credential", await page.locator("#gate").isVisible());
const why = await page.locator("#gate-why").innerText();
ok("the prompt says what to do", why.includes("aroute keys mint"), why);

// ── a wrong credential: still refused ─────────────────────────────────────
await page.fill("#gate-token", "not-a-real-token");
await page.click("#gate-form button[type=submit]");
await page.waitForTimeout(900);
ok("a wrong credential is refused", await page.locator("#gate").isVisible(),
  "the prompt stays up");

// ── the right credential: the panel opens ─────────────────────────────────
await page.fill("#gate-token", TOKEN);
await page.click("#gate-form button[type=submit]");
// The panel opens when the credential prompt CLOSES and the view renders.
// Waiting for the prompt to close is the honest signal: the prompt is
// hidden the moment `refresh()` succeeds, and the view is drawn right after.
await page.waitForSelector("#gate", { state: "hidden", timeout: 15000 });
await page.waitForSelector("#view .stat", { timeout: 15000 });
ok("the right credential opens the panel", (await page.locator("#view .stat").count()) >= 4);
ok("the prompt is gone once unlocked", !(await page.locator("#gate").isVisible()));
const stored = await page.evaluate(() => sessionStorage.getItem("ar-panel-token"));
ok("the credential is kept for the tab", stored === TOKEN);

// ── the ?token= first paint, with a real token ────────────────────────────
const page2 = await browser.newPage();
await page2.goto(`${B}/ui/?token=${TOKEN}#dashboard`, { waitUntil: "networkidle" });
await page2.waitForSelector("#view .stat", { timeout: 15000 });
ok("the ?token= first paint opens the panel", (await page2.locator("#view .stat").count()) >= 4);
ok("the token is stripped from the URL", !page2.url().includes("token="), page2.url());

// ── the write path is reachable with a credential ─────────────────────────
await page.goto(`${B}/ui/#/keys`, { waitUntil: "networkidle" });
await page.waitForSelector("#mint-form", { timeout: 15000 });
await page.fill("#mint-id", "armed-probe");
await page.click("#mint-form button[type=submit]");
await page.waitForSelector("#minted:not([hidden])", { timeout: 15000 });
const minted = await page.locator("#minted-token").innerText();
ok("the mint works with a credential", minted.length > 20, `${minted.slice(0, 12)}...`);

// ── the state endpoint agrees with the badge ──────────────────────────────
// One gate state, one source: the badge and the gate cannot disagree.
const state = await page.evaluate(async () => {
  const res = await fetch("/ui/api/state", {
    headers: { authorization: "Bearer " + sessionStorage.getItem("ar-panel-token") },
  });
  return await res.json();
});
ok("the state endpoint reports the gate armed", state.gate_armed === true, state.gate_badge);

await browser.close();
const failed = results.filter((r) => !r.pass);
console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
process.exit(failed.length ? 1 : 0);
