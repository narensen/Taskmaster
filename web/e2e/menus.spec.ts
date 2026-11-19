import { test, expect } from "./fixtures";

// The provider menu is positioned by the popover library. An opacity fade must not
// move it, and each of the five roles can be chosen on a fresh install.

const ROLES = ["Master", "Mid", "Worker", "Reviewer", "Verifier"] as const;

test("provider menus stay put, and each role can be chosen", async ({ openFixture, page, daemon }) => {
  test.setTimeout(60_000);
  await openFixture("/settings");
  for (const role of ROLES) {
    const trigger = page.getByRole("button", { name: new RegExp(`^${role} provider`) });
    await trigger.scrollIntoViewIfNeeded();
    const before = await trigger.boundingBox();
    await trigger.click();
    const pop = page.locator(".prov-pop").last();
    await expect(pop).toBeVisible();
    const first = await pop.boundingBox();
    await page.waitForTimeout(80);
    const second = await pop.boundingBox();
    const after = await trigger.boundingBox();
    expect(first, `${role} menu`).not.toBeNull();
    expect(second).not.toBeNull();
    expect(Math.abs(first!.x - second!.x)).toBeLessThan(2);
    expect(Math.abs(first!.y - second!.y)).toBeLessThan(2);
    expect(Math.abs((before?.y ?? 0) - (after?.y ?? 0))).toBeLessThan(2);
    await pop.getByRole("listbox", { name: "Providers" }).getByRole("option", { name: /Grok/ }).click();
    await pop.getByRole("option", { name: /Provider default/ }).click();
    await expect(pop).toBeHidden();
  }
  const settings = await (await fetch(`${daemon.fixture!.url}/api/settings`, { headers: { authorization: `Bearer ${daemon.fixture!.token}` } })).json();
  expect(settings.roles).toMatchObject({ master: "grok", mid: "grok", worker: "grok", reviewer: "grok", verifier: "grok" });
});
