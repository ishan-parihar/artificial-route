// Behavior check for the /ui page: the playground (multi-turn, system prompt,
// failed send), mint-to-playground, favorites, and the request-log filters.
// Thirteen checks; exits 1 when any fails.
//
// Not run in CI. It drives a real browser, so run it by hand against a server
// you started for it:
//
//   npm i playwright && npx playwright install chromium
//   aroute --config <demo config.yaml> serve        # its own port
//   node scripts/ui/behave.mjs http://127.0.0.1:<port>
//
// The chat and mint routes are mocked in the page, so the demo config needs
// no provider key and no credential store.
import { chromium } from "playwright";
const B = process.argv[2]; let fails = 0;
const ok = (c, m) => { console.log((c ? "PASS " : "FAIL ") + m); if (!c) fails++; };
const br = await chromium.launch(); const pg = await br.newPage();
const sent = [];
await pg.route("**/v1/chat/completions", async (r) => {
  const b = JSON.parse(r.request().postData()); sent.push(b);
  const n = sent.length;
  await r.fulfill({ status: 200, contentType: "text/event-stream", body:
    `data: {"choices":[{"delta":{"content":"reply${n}"}}]}\n\ndata: {"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1}}\n\ndata: [DONE]\n\n` });
});
await pg.goto(B + "/ui/#/playground");
await pg.waitForSelector("#pg-combo option", { state: "attached" });
await pg.fill("#pg-system", "be brief");
await pg.fill("#pg-prompt", "first"); await pg.click("#pg-form button[type=submit]");
await pg.waitForFunction(() => document.querySelectorAll(".pg-turn").length === 2);
// The turn appears before the reply lands; Send stays disabled until it does.
await pg.waitForFunction(() => !document.querySelector("#pg-form button[type=submit]").disabled);
await pg.fill("#pg-prompt", "second"); await pg.click("#pg-form button[type=submit]");
await pg.waitForFunction(() => document.querySelectorAll(".pg-turn").length === 4);
ok(sent[1].messages.map(m => m.role + ":" + m.content).join("|") === "system:be brief|user:first|assistant:reply1|user:second", "turn 2 carries system + full history");
await pg.click("#pg-clear");
ok(await pg.locator(".pg-turn").count() === 0, "Clear resets");
// failed send keeps history untouched and prompt in box
await pg.unroute("**/v1/chat/completions");
await pg.route("**/v1/chat/completions", (r) => r.fulfill({ status: 502, contentType: "application/json", body: '{"error":{"message":"down"}}' }));
await pg.fill("#pg-prompt", "oops"); await pg.click("#pg-form button[type=submit]");
await pg.waitForSelector("#pg-error:not([hidden])");
ok(await pg.locator(".pg-turn").count() === 0 && await pg.inputValue("#pg-prompt") === "oops", "failed send leaves no ghost turn and keeps the prompt");
// mint -> playground
await pg.route("**/ui/api/keys/mint", (r) => r.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ key_id: "tmock", access: "ar_live_mocktoken_1234567890" }) }));
await pg.goto(B + "/ui/#/keys"); await pg.waitForSelector("#mint-id");
const id = "t" + Date.now() % 100000;
await pg.fill("#mint-id", id); await pg.click("#mint-form button[type=submit]");
await pg.waitForFunction(() => !document.getElementById("minted").hidden && document.getElementById("minted-token").textContent.length > 8);
ok(true, "minted token visible after refresh");
const tok = await pg.textContent("#minted-token");
await pg.click("#minted-use");
await pg.waitForSelector("#pg-token");
ok(await pg.inputValue("#pg-token") === tok, "playground bearer prefilled with minted token");
await pg.reload(); await pg.waitForSelector("#pg-token");
ok(await pg.inputValue("#pg-token") === "", "reload drops the token");
ok(!(await pg.evaluate(() => JSON.stringify(localStorage) + JSON.stringify(sessionStorage))).includes(tok), "token not in web storage");
// favorites
await pg.goto(B + "/ui/#/providers"); await pg.waitForSelector("button.star", { state: "attached" });
const first = await pg.locator("#catalog-body button.star").first();
await first.evaluate(e => { e.closest("details").open = true; });
const sid = await first.getAttribute("data-star"); await first.click();
ok(await pg.locator("#fav-body button.star").count() === 1, "star lands in favorites");
await pg.reload(); await pg.waitForSelector("#fav-body button.star", { state: "attached" });
ok(await pg.locator("#fav-body button.star").count() === 1, "favorite survives reload");
await pg.click("#fav-body button.star");
ok(await pg.locator("#fav-body button.star").count() === 0, "unstar removes it");
// log filters
await pg.goto(B + "/ui/#/usage"); await pg.waitForSelector("#usage-rows tr");
const all = await pg.locator("#usage-rows tr").count();
await pg.check("#lg-error"); await pg.waitForTimeout(600);
const errRows = await pg.locator("#usage-rows tr.failed").count();
ok(errRows >= 1 && await pg.locator("#usage-rows tr:not(.failed)").filter({ hasNot: pg.locator("td.state") }).count() === 0, `error filter shows only failed rows (${errRows}/${all})`);
ok(await pg.isVisible("#lg-clear"), "Clear filters appears when a filter is active");
await pg.click("#lg-clear"); await pg.waitForSelector("#lg-clear", { state: "hidden" });
await pg.selectOption("#lg-days", "7"); await pg.waitForTimeout(600);
ok((await pg.textContent("#usage-asof")).startsWith("last 7 days"), "range picker updates chart caption");
await br.close(); process.exit(fails ? 1 : 0);
