import { test, finishedRun } from "./fixtures";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

// Screenshots for the README and release notes, of the real app on this machine.
// Only with SHOTS=1: `SHOTS=1 npx playwright test screens`.
const out = resolve(dirname(fileURLToPath(import.meta.url)), "../../assets/screenshots");
const shots = !!process.env.SHOTS;

/** The real state, with any signed-in account replaced: screenshots are published. */
async function anonymous(page: import("@playwright/test").Page) {
  await page.route("**/api/setup", async (route) => {
    const real = await route.fetch();
    const body = (await real.json()) as { providers: { account: string | null }[] };
    for (const p of body.providers) if (p.account && p.account.includes("@")) p.account = p.account.replace(/[^\s(]+@[^\s)]+/, "you@example.com");
    await route.fulfill({ response: real, json: body });
  });
}

async function themed(page: import("@playwright/test").Page, theme: string) {
  await page.addInitScript((t) => {
    const saved = JSON.parse(localStorage.getItem("taskmaster.prefs") ?? "{}");
    localStorage.setItem("taskmaster.prefs", JSON.stringify({ ...saved, theme: t }));
  }, theme);
}

for (const theme of ["dark", "light"] as const) {
  test(`run view with the token split, and home, ${theme}`, async ({ openFixture, page, daemon }) => {
    test.skip(!shots, "set SHOTS=1");
    test.setTimeout(120_000);
    await themed(page, theme);
    await finishedRun(daemon.fixture!, "bookshop", "A bookshop: a home page with featured books, a searchable list, and a cart with a total.");
    await openFixture("/runs/bookshop");
    await page.getByTestId("token-ledger").scrollIntoViewIfNeeded();
    await page.waitForTimeout(400);
    await page.screenshot({ path: `${out}/33-run-tokens-${theme}.png` });
    await page.getByTestId("token-ledger").screenshot({ path: `${out}/34-token-ledger-${theme}.png` });
    await page.goto(daemon.fixture!.url + "/");
    await page.getByRole("textbox", { name: /Tell the master/ }).waitFor();
    await page.waitForTimeout(600);
    await page.screenshot({ path: `${out}/35-home-${theme}.png` });
  });

  test(`skills picker and page, ${theme}`, async ({ open, page }) => {
    test.skip(!shots, "set SHOTS=1");
    await themed(page, theme);
    await page.route("**/api/skills/search?*", async (route) => route.fulfill({ response: await route.fetch() }));
    await open("/");
    const box = page.getByRole("textbox", { name: /Tell the master/ });
    await box.click();
    await page.keyboard.type("A recipe box with tags and search. Follow $");
    await page.getByRole("listbox").waitFor();
    await page.waitForTimeout(300);
    await page.screenshot({ path: `${out}/31-skills-picker-${theme}.png` });
    await open("/skills?q=accessibility");
    await page.getByTestId("skill").first().waitFor();
    await page.getByTestId("skill-hit").first().waitFor({ timeout: 20_000 }).catch(() => undefined);
    await page.screenshot({ path: `${out}/32-skills-${theme}.png`, fullPage: true });
  });

  test(`providers page, ${theme}`, async ({ open, page }) => {
    test.skip(!shots, "set SHOTS=1");
    test.setTimeout(90_000);
    await themed(page, theme);
    await anonymous(page);
    await open("/providers");
    await page.getByTestId("provider").first().waitFor();
    await page.waitForFunction(() => !document.body.innerText.includes("Checking…"), null, { timeout: 60_000 });
    await page.screenshot({ path: `${out}/30-providers-${theme}.png`, fullPage: true });
  });
}
