import { test, expect } from "./fixtures";

test("the mid-tier checker is off by default and its switch is kept by the daemon", async ({ open, page, daemon }) => {
  await open("/settings");
  const toggle = page.getByTestId("checker-setting").getByRole("switch", { name: "Check finished pages with the mid model" });
  await expect(toggle).toHaveAttribute("aria-checked", "false");
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  const saved = (await (await fetch(`${daemon.url}/api/settings`, { headers: { authorization: `Bearer ${daemon.token}` } })).json()) as { checker: { enabled: boolean } };
  expect(saved.checker.enabled).toBe(true);
  await page.reload();
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  await toggle.focus();
  await page.keyboard.press("Space");
  await expect(toggle).toHaveAttribute("aria-checked", "false");
});

test("Code Mode is off by default and can be switched on", async ({ open, page }) => {
  await open("/settings");
  const toggle = page.getByTestId("code-mode-setting").getByRole("switch", { name: "Use Code Mode for workers" });
  await expect(toggle).toHaveAttribute("aria-checked", "false");
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  await page.reload();
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-checked", "false");
});

test("the worker sandbox's shell and package indexes are on by default and each can be turned off", async ({ open, page, daemon }) => {
  await open("/settings");
  const shell = page.getByTestId("sandbox-shell-setting").getByRole("switch", { name: "Let builders run commands in the sandbox" });
  const indexes = page.getByTestId("sandbox-indexes-setting").getByRole("switch", { name: "Let fetch tools reach package indexes" });
  await expect(shell).toHaveAttribute("aria-checked", "true");
  await expect(indexes).toHaveAttribute("aria-checked", "true");
  await shell.click();
  await expect(shell).toHaveAttribute("aria-checked", "false");
  await page.reload();
  await expect(shell).toHaveAttribute("aria-checked", "false");
  await expect(indexes).toHaveAttribute("aria-checked", "true");
  const saved = (await (await fetch(`${daemon.url}/api/settings`, { headers: { authorization: `Bearer ${daemon.token}` } })).json()) as {
    sandbox: { shell: boolean; package_indexes: boolean };
  };
  expect(saved.sandbox).toMatchObject({ shell: false, package_indexes: true });
  await shell.click();
  await expect(shell).toHaveAttribute("aria-checked", "true");
});

test("in the desktop app, Settings shows an available update with its notes and installs it", async ({ open, page }) => {
  // The desktop shell's commands, as the window would have them.
  await page.addInitScript(() => {
    const calls: string[] = [];
    (window as unknown as { __calls: string[] }).__calls = calls;
    (window as unknown as { __TAURI__: unknown }).__TAURI__ = {
      core: {
        invoke: async (command: string) => {
          calls.push(command);
          if (command === "app_version") return "0.4.0";
          if (command === "check_update") return { version: "0.5.0", current: "0.4.0", date: "2026-11-20 10:00:00 +00:00", notes: "Faster plans.\nA token ledger per agent." };
          if (command === "install_update") return new Promise(() => undefined);
          throw new Error(`unknown ${command}`);
        },
      },
    };
  });
  await open("/settings");
  const updates = page.getByTestId("updates");
  await expect(updates).toContainText("This is Taskmaster 0.4.0");
  await expect(updates).toContainText("Update available: 0.5.0");
  await expect(updates).toContainText("A token ledger per agent.");
  await updates.getByRole("button", { name: "Install and restart" }).click();
  await expect(updates).toContainText("checking the signature");
  const calls = await page.evaluate(() => (window as unknown as { __calls: string[] }).__calls);
  expect(calls).toContain("install_update");
});

test("in a browser there is no Updates section", async ({ open, page }) => {
  await open("/settings");
  await expect(page.getByRole("heading", { name: "Mid-tier checker" })).toBeVisible();
  await expect(page.getByTestId("updates")).toHaveCount(0);
});
