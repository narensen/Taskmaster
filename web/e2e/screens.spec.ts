import { test } from "./fixtures";
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

for (const theme of ["dark", "light"] as const) {
  test(`providers page, ${theme}`, async ({ open, page }) => {
    test.skip(!shots, "set SHOTS=1");
    test.setTimeout(90_000);
    await page.addInitScript((t) => {
      const saved = JSON.parse(localStorage.getItem("taskmaster.prefs") ?? "{}");
      localStorage.setItem("taskmaster.prefs", JSON.stringify({ ...saved, theme: t }));
    }, theme);
    await anonymous(page);
    await open("/providers");
    await page.getByTestId("provider").first().waitFor();
    await page.waitForFunction(() => !document.body.innerText.includes("Checking…"), null, { timeout: 60_000 });
    await page.screenshot({ path: `${out}/30-providers-${theme}.png`, fullPage: true });
  });
}
