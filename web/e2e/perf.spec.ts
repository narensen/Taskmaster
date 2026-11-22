import { test, expect, finishedRun, CHOSEN } from "./fixtures";

// P12 performance guardrails: home request budget, fast POST /api/runs with warm providers,
// bounded cold run open without loading the diff viewer chunk upfront, incremental delta APIs,
// and zero infinite animations under prefers-reduced-motion.

test("home and run open stay within request, latency, transfer, and animation budgets", async ({ openFixture, page, daemon }) => {
  test.setTimeout(90_000);
  const f = daemon.fixture!;
  const headers = { "content-type": "application/json", authorization: `Bearer ${f.token}` };
  const before = ((await (await fetch(`${f.url}/api/settings`, { headers })).json()) as { project: string }).project;
  const made = await fetch(`${f.url}/api/projects`, {
    method: "POST",
    headers,
    body: JSON.stringify({ name: `perf${Date.now().toString().slice(-6)}`, switch: true }),
  });
  expect(made.ok).toBe(true);

  try {
    // Seed a finished run so home and run open both exercise real data.
    await finishedRun(f, "perf-seed", "Plan this small product quickly. A bookshop with home, books, and cart.");

    // Warm provider cache once, then measure POST /api/runs latency (budget <= 50ms).
    await fetch(`${f.url}/api/providers`, { headers });
    const t0 = performance.now();
    const postRes = await fetch(`${f.url}/api/runs`, {
      method: "POST",
      headers,
      body: JSON.stringify({ id: "perf-fast-start", prompt: "A bookshop with home, books, and cart.", yes: true, roles: CHOSEN }),
    });
    const postMs = performance.now() - t0;
    expect(postRes.ok).toBe(true);
    expect(postMs).toBeLessThanOrEqual(50);

    // Wait for perf-fast-start to settle before measuring home & run open cleanly.
    const deadline = Date.now() + 20_000;
    while (Date.now() < deadline) {
      const r = (await (await fetch(`${f.url}/api/runs/perf-fast-start`, { headers })).json()) as { active: boolean };
      if (!r.active) break;
      await new Promise((res) => setTimeout(res, 100));
    }

    // Measure home page: <= 12 API requests, none > 300ms.
    const homeApi: { url: string; ms: number }[] = [];
    const onHomeReq = (req: import("@playwright/test").Request) => {
      if (req.url().includes("/api/")) (req as unknown as { __t0?: number }).__t0 = performance.now();
    };
    const onHomeRes = (res: import("@playwright/test").Response) => {
      const url = res.url();
      if (!url.includes("/api/") || url.includes("/stream")) return;
      const start = (res.request() as unknown as { __t0?: number }).__t0 ?? performance.now();
      homeApi.push({ url, ms: performance.now() - start });
    };
    page.on("request", onHomeReq);
    page.on("response", onHomeRes);
    await openFixture("/");
    await expect(page.getByText("perf-seed").first()).toBeVisible();
    await page.waitForTimeout(250);
    page.off("request", onHomeReq);
    page.off("response", onHomeRes);

    expect(homeApi.length).toBeLessThanOrEqual(12);
    for (const req of homeApi) {
      expect(req.ms, `home request ${req.url} took ${req.ms.toFixed(1)}ms`).toBeLessThanOrEqual(300);
    }

    // Measure run cold open: <= 1.2 MiB API transfer, no @pierre/diffs chunk fetched upfront, no API > 300ms.
    let runApiBytes = 0;
    const loadedAssets: { url: string; bytes: number }[] = [];
    const runApi: { url: string; ms: number }[] = [];
    const onRunReq = (req: import("@playwright/test").Request) => {
      if (req.url().includes("/api/")) (req as unknown as { __t0?: number }).__t0 = performance.now();
    };
    const onRunRes = async (res: import("@playwright/test").Response) => {
      const url = res.url();
      const len = Number(res.headers()["content-length"] ?? 0);
      const bytes = Number.isFinite(len) && len > 0 ? len : 0;
      if (url.includes("/api/") && !url.includes("/stream")) {
        const start = (res.request() as unknown as { __t0?: number }).__t0 ?? performance.now();
        runApi.push({ url, ms: performance.now() - start });
        runApiBytes += bytes;
      } else if (url.includes("/assets/")) {
        loadedAssets.push({ url, bytes });
      }
    };
    page.on("request", onRunReq);
    page.on("response", onRunRes);
    await openFixture("/runs/perf-seed");
    await expect(page.getByTestId("run-headline")).toBeVisible();
    await page.waitForTimeout(300);
    page.off("request", onRunReq);
    page.off("response", onRunRes);

    expect(runApiBytes).toBeLessThanOrEqual(1.2 * 1024 * 1024);
    // P6: @pierre/diffs & highlights chunks must not load on initial run open (only when entering diff mode).
    expect(loadedAssets.some((a) => /diffs|pierre|highlights/i.test(a.url))).toBe(false);
    const filesChunk = loadedAssets.find((a) => /FilesPanel/i.test(a.url));
    if (filesChunk) expect(filesChunk.bytes).toBeLessThan(200 * 1024);
    for (const req of runApi) {
      expect(req.ms, `run open request ${req.url} took ${req.ms.toFixed(1)}ms`).toBeLessThanOrEqual(300);
    }

    // P9 + P11: verify delta /api/runs?since= and incremental /api/runs/{id}/ledger?after= endpoints.
    const runs1 = (await (await fetch(`${f.url}/api/runs`, { headers })).json()) as { runs: unknown[]; seq: number };
    expect(typeof runs1.seq).toBe("number");
    const runsDelta = (await (await fetch(`${f.url}/api/runs?since=${runs1.seq}`, { headers })).json()) as {
      runs: unknown[];
      delta: boolean;
      all_ids: string[];
    };
    expect(runsDelta.delta).toBe(true);
    expect(runsDelta.runs.length).toBe(0);
    expect(runsDelta.all_ids).toContain("perf-seed");

    const ledgerFull = (await (await fetch(`${f.url}/api/runs/perf-seed/ledger`, { headers })).json()) as {
      entries: unknown[];
      total_entries: number;
    };
    const ledgerTail = (await (await fetch(`${f.url}/api/runs/perf-seed/ledger?after=${ledgerFull.entries.length}`, { headers })).json()) as {
      entries: unknown[];
      total_entries: number;
      after: number;
    };
    expect(ledgerTail.entries.length).toBe(0);
    expect(ledgerTail.total_entries).toBe(ledgerFull.entries.length);

    // P10: reduced motion stops all CSS animations.
    await page.emulateMedia({ reducedMotion: "reduce" });
    const runningInfinite = await page.evaluate(() => {
      return document.getAnimations().filter((a) => {
        const timing = a.effect?.getTiming();
        return a.playState === "running" && timing?.iterations === Infinity;
      }).length;
    });
    expect(runningInfinite).toBe(0);
  } finally {
    await fetch(`${f.url}/api/projects/open`, {
      method: "POST",
      headers,
      body: JSON.stringify({ path: before }),
    });
  }
});
