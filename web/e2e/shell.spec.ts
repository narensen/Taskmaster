import { test, expect, lowContrast } from "./fixtures";

// The shell and the new controls: the title bar, choosing providers with nothing chosen
// for you, pages at once, Command-K, the quick terminal and the files panel.

test("the title bar shows the project, then where you are, on the app's own surface", async ({ open, page }) => {
  await open("/settings");
  const crumbs = page.getByRole("navigation", { name: "Where you are" });
  await expect(crumbs.getByRole("link", { name: "project" })).toBeVisible();
  await expect(crumbs.locator("[aria-current=page]")).toHaveText("Settings");
  const [bar, canvas] = await page.evaluate(() => [
    getComputedStyle(document.querySelector(".titlebar")!).backgroundColor,
    getComputedStyle(document.querySelector(".main")!).backgroundColor,
  ]);
  expect(bar).toBe(canvas);
});

test("a fresh install chooses no provider and asks for each role", async ({ open, page, daemon }) => {
  const settings = await (await fetch(`${daemon.url}/api/settings`, { headers: { authorization: `Bearer ${daemon.token}` } })).json();
  expect(settings.roles).toEqual({});
  await open("/runs/new");
  const deck = page.getByRole("region", { name: "Who plays each role" });
  await expect(deck).toContainText("only do this once");
  for (const seat of ["Master", "Mid", "Worker", "Reviewer"]) {
    await expect(deck.getByRole("button", { name: `${seat} provider: not chosen` })).toBeVisible();
  }
  await expect(deck.getByText("Remember for this project")).toBeVisible();
});

test("pages at once is one click, and Max is what this machine runs", async ({ open, page, daemon }) => {
  const { machine } = await (await fetch(`${daemon.url}/api/settings`, { headers: { authorization: `Bearer ${daemon.token}` } })).json();
  await open("/runs/new");
  const group = page.getByRole("radiogroup", { name: "Pages at once" });
  await group.getByRole("radio", { name: "2" }).click();
  await expect(group.getByRole("radio", { name: "2" })).toHaveAttribute("aria-checked", "true");
  await group.getByRole("radio", { name: "Max" }).click();
  await expect(page.getByText(`${machine.max_parallel} on this machine`)).toBeVisible();
});

test("a provider and model are picked in place, and the first run saves them", async ({ openFixture, page, daemon }) => {
  test.setTimeout(60_000);
  await openFixture("/runs/new");
  for (const seat of ["Master", "Mid", "Worker", "Reviewer"]) {
    await page.getByRole("button", { name: new RegExp(`^${seat} provider`) }).click();
    const pop = page.locator(".prov-pop").last();
    await expect(pop).toBeVisible();
    await pop.getByRole("listbox", { name: "Providers" }).getByRole("option", { name: /Grok/ }).click();
    await pop.getByRole("option", { name: /Provider default/ }).click();
  }
  await expect(page.getByRole("button", { name: /^Master provider: Grok/ })).toBeVisible();
  await page.getByRole("radiogroup", { name: "Pages at once" }).getByRole("radio", { name: "2" }).click();
  await page.locator(".composer-input").click();
  await page.keyboard.type("A one page site");
  await page.keyboard.press("Enter");
  await page.waitForURL(/\/runs\/run-/);
  const settings = await (await fetch(`${daemon.fixture!.url}/api/settings`, { headers: { authorization: `Bearer ${daemon.fixture!.token}` } })).json();
  expect(settings.roles).toMatchObject({ master: "grok", mid: "grok", worker: "grok", reviewer: "grok", verifier: "grok" });
});

test("Command-K finds a file and a command, and switches the theme", async ({ open, page }) => {
  await open("/");
  await page.keyboard.press("Control+k");
  const palette = page.getByRole("dialog", { name: "Command palette" });
  await expect(palette).toBeVisible();
  await page.keyboard.type("readme");
  await expect(palette.getByRole("option", { name: /README\.md/ })).toBeVisible();
  await page.keyboard.press("Control+a");
  await page.keyboard.type(">light theme");
  await page.keyboard.press("Enter");
  await expect(page.locator(".shell")).toHaveClass(/theme-light/);
  await page.evaluate(() => {
    const p = JSON.parse(localStorage.getItem("taskmaster.prefs") ?? "{}");
    localStorage.setItem("taskmaster.prefs", JSON.stringify({ ...p, theme: "dark" }));
  });
});

test("Ctrl-J opens a shell in the project, and hiding it keeps the shell", async ({ open, page, daemon }) => {
  await open("/");
  await page.locator(".main").click({ position: { x: 20, y: 20 } });
  await page.keyboard.press("Control+j");
  const drawer = page.getByRole("region", { name: "Terminal" });
  await expect(drawer.locator(".xterm")).toBeVisible();
  await page.waitForTimeout(500);
  await page.keyboard.type("echo marker-$((40+2)) && pwd\n");
  await expect(drawer.locator(".xterm-rows")).toContainText("marker-42");
  await expect(drawer.locator(".xterm-rows")).toContainText(daemon.project.split("/").pop()!);
  await page.keyboard.press("Control+j");
  // Hidden from the page and from assistive tech, but not closed.
  await expect(page.locator(".qterm")).toHaveAttribute("aria-hidden", "true");
  const shells = await (await fetch(`${daemon.url}/api/terminals`, { headers: { authorization: `Bearer ${daemon.token}` } })).json();
  expect(shells.terminals.filter((t: { alive: boolean }) => t.alive).length).toBeGreaterThan(0);
});

test("the files panel opens a file in a tab with its path, and finds in it", async ({ open, page }) => {
  await open("/files?path=README.md");
  await expect(page.getByRole("tab", { name: "README.md" })).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("navigation", { name: "File path" })).toContainText("README.md");
  await expect(page.getByTestId("preview")).toContainText("# e2e");
  await page.getByTestId("preview").click();
  await page.keyboard.press("Control+f");
  await page.getByLabel("Find in this file").fill("e2e");
  await expect(page.locator(".findbar-count")).toHaveText("1 of 1");
});

test("? opens the keyboard sheet", async ({ open, page }) => {
  await open("/");
  await page.locator(".main").click({ position: { x: 20, y: 20 } });
  await page.keyboard.press("?");
  await expect(page.getByRole("dialog", { name: "Keyboard" })).toBeVisible();
});

for (const theme of ["dark", "light"] as const) {
  test(`every text on the main screens reads at 4.5:1 or better, ${theme}`, async ({ open, page }) => {
    await page.addInitScript((t) => {
      const p = JSON.parse(localStorage.getItem("taskmaster.prefs") ?? "{}");
      localStorage.setItem("taskmaster.prefs", JSON.stringify({ ...p, theme: t, motion: false }));
    }, theme);
    for (const path of ["/", "/runs/new", "/settings", "/files?path=README.md"]) {
      await open(path);
      await page.waitForTimeout(600);
      expect(await lowContrast(page), path).toEqual([]);
    }
  });
}
