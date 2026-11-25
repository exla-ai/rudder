#!/usr/bin/env node
// Phone-width screenshots of the signed-in web app, for a visual check before
// a release. Read-only: it opens screens, types a draft and opens the model
// picker, and never sends or changes anything.
//
//   RUDDER_QA_TOKEN_FILE=~/.rudder-qa-token node scripts/phone-qa.mjs <out-dir> [base-url]
//
// The token file holds one `rdr_` token (mode 600). To make one: POST
// /api/cli/login on the control plane, open its approveUrl signed in, then
// poll its pollUrl with the deviceCode. PLAYWRIGHT and CHROMIUM name the
// Playwright module and the browser binary where they are not the defaults.
import fs from "node:fs";
import path from "node:path";

const [outDir, base = "https://rudder-cloud-control.fly.dev"] = process.argv.slice(2);
const tokenFile = process.env.RUDDER_QA_TOKEN_FILE;
if (!outDir || !tokenFile) {
  console.error("usage: RUDDER_QA_TOKEN_FILE=<file> node scripts/phone-qa.mjs <out-dir> [base-url]");
  process.exit(2);
}
const token = fs.readFileSync(tokenFile, "utf8").trim();
if (!token.startsWith("rdr_")) throw new Error(`${tokenFile} does not hold a Rudder token`);
const { chromium } = await import(process.env.PLAYWRIGHT || "playwright");
fs.mkdirSync(outDir, { recursive: true });

const browser = await chromium.launch({ executablePath: process.env.CHROMIUM || undefined, args: ["--no-sandbox"] });
const context = await browser.newContext({
  viewport: { width: 393, height: 852 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true, colorScheme: "dark",
  userAgent: "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
});
await context.addInitScript((value) => localStorage.setItem("rudder.token", JSON.stringify(value)), token);
const page = await context.newPage();
const failures = [];
async function screen(name, route, act) {
  await page.goto(base + route, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(4500);
  if (act) await act().catch((error) => failures.push(`${name}: ${error.message.split("\n")[0]}`));
  // A page wider than the phone is the commonest layout bug: report it.
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - innerWidth);
  if (overflow > 1) failures.push(`${name}: the page is ${overflow}px wider than the screen`);
  await page.screenshot({ path: path.join(outDir, `${name}.png`) });
}

await screen("01-home", "/app");
const chat = await page.$$eval("a[href*='/a/']", (links) => links[0]?.getAttribute("href"));
const project = await page.$$eval("a[href^='/app/p/']", (links) => links.map((a) => a.getAttribute("href")).find((href) => !href.includes("/a/")));
await screen("02-all-agents", "/app/all");
if (project) await screen("03-project", project);
await screen("04-team", "/app/team");
await screen("05-files", "/app/files");
if (chat) {
  await screen("06-chat", chat);
  await screen("07-chat-typing", chat, async () => {
    await page.locator(".composer textarea").first().fill("A long draft, to see how the box holds several lines of text on a phone without squeezing it into a column");
    await page.waitForTimeout(400);
  });
  await screen("08-model-picker", chat, async () => {
    await page.locator(".composer-meta button, .model-chip, [aria-label*='odel']").first().click({ timeout: 4000 });
    await page.waitForTimeout(2500);
  });
}
await screen("09-settings", "/app/settings");
await screen("10-machine", "/app/machine");
await screen("11-usage", "/app/usage");
await browser.close();
for (const failure of failures) console.error(failure);
console.log(`${fs.readdirSync(outDir).length} screenshots in ${outDir}`);
process.exit(failures.length ? 1 : 0);
