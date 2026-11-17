import { test, expect, sse } from "./fixtures";
import type { Route } from "@playwright/test";

// The Providers page against a real daemon, with /api/setup answered here: pressing
// Install in a test must never run a real installer.

const status = (id: string, name: string, extra: Record<string, unknown> = {}) => ({
  id,
  name,
  installed: true,
  path: `/home/u/.local/bin/${id}`,
  version: "1.2.3",
  signed_in: true,
  account: null,
  latest: "1.2.3",
  update_available: false,
  missing_runtimes: [],
  notes: [],
  ...extra,
});

const overview = {
  providers: [
    status("claude", "Claude Code", { account: "dev@example.com (pro)" }),
    status("agy", "Antigravity", { signed_in: null, latest: null, update_available: null }),
    status("grok", "Grok Build", { version: "1.0.77", latest: "1.0.88", update_available: true }),
    status("codex", "Codex", { signed_in: false }),
    { ...status("opencode", "OpenCode"), installed: false, path: null, version: null, signed_in: null, latest: null, update_available: null },
  ],
  runtimes: [
    { id: "node", name: "Node.js", installed: true, version: "24.11.0", used_for: "JavaScript builds" },
    { id: "bun", name: "Bun", installed: false, version: null, used_for: "Bun projects" },
    { id: "python", name: "Python", installed: true, version: "3.12.3", used_for: "Python projects" },
  ],
  jobs: [],
};

const job = (over: Record<string, unknown>) => ({
  id: "job-1",
  kind: "provider",
  target: "opencode",
  action: "install",
  state: "running",
  steps: [
    { label: "Installing OpenCode", command: "curl -fsSL https://opencode.ai/v2/install | bash", state: "running" },
    { label: "Checking the install", command: "opencode --version", state: "waiting" },
  ],
  output: "",
  prompt: { url: null, code: null },
  accepts_input: false,
  exit_code: null,
  error: null,
  started: "2026-11-17T10:00:00Z",
  finished: null,
  ...over,
});

async function mockSetup(page: import("@playwright/test").Page, streams: Record<string, string>, posts: { url: string; body: unknown }[]) {
  await page.route("**/api/setup", (route) => route.fulfill({ json: overview }));
  await page.route("**/api/providers", (route) =>
    route.fulfill({ json: { providers: overview.providers.map((p) => ({ id: p.id, name: p.name, installed: p.installed, install: "curl …", login: `${p.id} login`, models: [], error: null })) } }),
  );
  await page.route("**/api/setup/jobs/*/stream*", (route: Route) => {
    const id = new URL(route.request().url()).pathname.split("/")[4]!;
    return route.fulfill({ status: 200, headers: { "content-type": "text/event-stream" }, body: streams[id] ?? "" });
  });
  await page.route("**/api/setup/{providers,runtimes,jobs}/**", (route) => {
    if (route.request().method() !== "POST") return route.fallback();
    const url = new URL(route.request().url()).pathname;
    posts.push({ url, body: route.request().postDataJSON() });
    if (url.endsWith("/opencode/install")) return route.fulfill({ status: 202, json: job({}) });
    if (url.endsWith("/codex/login"))
      return route.fulfill({ status: 202, json: job({ id: "job-2", target: "codex", action: "login", steps: [{ label: "Signing in to Codex", command: "codex login --device-auth", state: "running" }] }) });
    return route.fulfill({ json: { ok: true } });
  });
}

test("each provider shows installed, version, sign-in and update state as plain text", async ({ open, page }) => {
  await mockSetup(page, {}, []);
  await open("/providers");
  const cards = page.getByTestId("provider");
  await expect(cards).toHaveCount(5);
  const claude = cards.filter({ hasText: "Claude Code" });
  await expect(claude).toContainText("Installed 1.2.3");
  await expect(claude).toContainText("Signed in as dev@example.com (pro)");
  await expect(claude).toContainText("Up to date");
  const grok = cards.filter({ hasText: "Grok Build" });
  await expect(grok).toContainText("1.0.88 available");
  await expect(grok.getByRole("button", { name: "Update" })).toBeVisible();
  await expect(cards.filter({ hasText: "Codex" })).toContainText("Not signed in");
  await expect(cards.filter({ hasText: "Antigravity" })).toContainText("Sign-in not reported");
  const opencode = cards.filter({ hasText: "OpenCode" });
  await expect(opencode).toContainText("Not installed");
  await expect(opencode.getByRole("button", { name: "Install" })).toBeVisible();
  await expect(page.getByTestId("runtime")).toHaveCount(3);
  await expect(page.getByTestId("runtime").filter({ hasText: "Bun" }).getByRole("button", { name: "Install" })).toBeVisible();
});

test("Install streams the installer's output and finishes", async ({ open, page }) => {
  const posts: { url: string; body: unknown }[] = [];
  const done = job({
    state: "done",
    output: "$ curl -fsSL https://opencode.ai/v2/install | bash\ndownloading opencode 1.4.0\ninstalled to ~/.opencode/bin",
    steps: [
      { label: "Installing OpenCode", command: "curl …", state: "done" },
      { label: "Checking the install", command: "opencode --version", state: "done" },
    ],
    finished: "2026-11-17T10:00:09Z",
  });
  await mockSetup(page, { "job-1": sse([{ event: "job", data: job({ output: "downloading opencode 1.4.0" }) }, { event: "job", data: done }]) }, posts);
  await open("/providers");
  const opencode = page.getByTestId("provider").filter({ hasText: "OpenCode" });
  await opencode.getByRole("button", { name: "Install" }).click();
  const panel = opencode.getByTestId("job");
  await expect(panel).toContainText("installed to ~/.opencode/bin");
  await expect(panel).toContainText("Installed");
  expect(posts.map((p) => p.url)).toContain("/api/setup/providers/opencode/install");
});

test("Sign in shows the device code and the sign-in page, and sends what is typed", async ({ open, page }) => {
  const posts: { url: string; body: unknown }[] = [];
  const waiting = job({
    id: "job-2",
    target: "codex",
    action: "login",
    steps: [{ label: "Signing in to Codex", command: "codex login --device-auth", state: "running" }],
    output: "Open https://auth.example.com/codex/device\nEnter this one-time code: KQ7P-ZD2M",
    prompt: { url: "https://auth.example.com/codex/device", code: "KQ7P-ZD2M" },
    accepts_input: true,
  });
  await mockSetup(page, { "job-2": sse([{ event: "job", data: waiting }]) }, posts);
  // The stream ends while the job is still running; the page asks for the job once more.
  await page.route("**/api/setup/jobs/job-2", (route) => route.fulfill({ json: waiting }));
  await open("/providers");
  const codex = page.getByTestId("provider").filter({ hasText: "Codex" });
  await codex.getByRole("button", { name: "Sign in" }).click();
  await expect(codex.getByTestId("device-code")).toHaveText("KQ7P-ZD2M");
  await expect(codex.getByRole("link", { name: "Open the sign-in page" })).toHaveAttribute("href", "https://auth.example.com/codex/device");
  const box = codex.getByLabel("Type into the sign-in screen");
  await box.fill("y");
  await box.press("Enter");
  await codex.getByRole("button", { name: "Down" }).click();
  await expect.poll(() => posts.filter((p) => p.url.endsWith("/input")).map((p) => p.body)).toEqual([{ text: "y", key: "enter" }, { key: "down" }]);
});

test("the provider actions are reachable from the keyboard with a visible focus ring", async ({ open, page }) => {
  await mockSetup(page, {}, []);
  await open("/providers");
  const install = page.getByTestId("provider").filter({ hasText: "OpenCode" }).getByRole("button", { name: "Install" });
  await install.focus();
  await page.keyboard.press("Shift+Tab");
  await page.keyboard.press("Tab");
  await expect(install).toBeFocused();
  const ring = await install.evaluate((el) => getComputedStyle(el).boxShadow);
  expect(ring).not.toBe("none");
});
