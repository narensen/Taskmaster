import { test as base, expect, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";
import { STATE } from "./setup";

/** Nothing is chosen for a person, so a test chooses a provider for every role. */
export const CHOSEN = { master: "grok", mid: "grok", worker: "grok", reviewer: "grok", verifier: "grok" };

export interface Daemon {
  url: string;
  token: string;
  project: string;
  /** The scripted daemon (examples/ui_fixture.rs), when it was built. */
  fixture: { url: string; token: string } | null;
}

/** Start a run on the scripted daemon and wait until it has finished. */
export async function finishedRun(fixture: { url: string; token: string }, id: string, prompt: string): Promise<void> {
  const headers = { "content-type": "application/json", authorization: `Bearer ${fixture.token}` };
  const made = await fetch(`${fixture.url}/api/runs`, { method: "POST", headers, body: JSON.stringify({ id, prompt, yes: true, roles: CHOSEN }) });
  if (made.status !== 202 && made.status !== 409) throw new Error(`run not started: ${made.status} ${await made.text()}`);
  for (let i = 0; i < 600; i++) {
    const run = (await (await fetch(`${fixture.url}/api/runs/${id}`, { headers })).json()) as { phase: string; active: boolean };
    if (!run.active && (run.phase === "done" || run.phase === "failed")) return;
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`run ${id} did not finish`);
}

export const test = base.extend<{ daemon: Daemon; open: (path: string) => Promise<Page>; openFixture: (path: string) => Promise<Page> }>({
  daemon: async ({}, use) => {
    await use(JSON.parse(readFileSync(STATE, "utf8")) as Daemon);
  },
  open: async ({ page, daemon }, use) => {
    await page.addInitScript((token) => localStorage.setItem("taskmaster.token", token), daemon.token);
    await use(async (path: string) => {
      await page.goto(daemon.url + path);
      return page;
    });
  },
  openFixture: async ({ page, daemon }, use) => {
    await use(async (path: string) => {
      if (!daemon.fixture) throw new Error("build the scripted daemon: cargo build -p tm-daemon --example ui_fixture");
      await page.addInitScript((token) => localStorage.setItem("taskmaster.token", token), daemon.fixture.token);
      await page.goto(daemon.fixture.url + path);
      return page;
    });
  },
});

export { expect };

/** WCAG contrast of each text style on the page against the background it sits on. */
export async function lowContrast(page: Page): Promise<string[]> {
  return page.evaluate(() => {
    const lum = (rgb: number[]) => {
      const [r, g, b] = rgb.map((v) => {
        const c = v / 255;
        return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
      });
      return 0.2126 * r! + 0.7152 * g! + 0.0722 * b!;
    };
    const parse = (s: string) => (s.match(/[\d.]+/g) ?? []).map(Number);
    const bgOf = (el: Element | null): number[] => {
      while (el) {
        const c = parse(getComputedStyle(el).backgroundColor);
        if (c.length >= 3 && (c.length < 4 || c[3]! > 0.5)) return c.slice(0, 3);
        el = el.parentElement;
      }
      return parse(getComputedStyle(document.body).backgroundColor).slice(0, 3);
    };
    const bad: string[] = [];
    for (const el of Array.from(document.querySelectorAll("h1, h2, h3, p, li, a, span, th, td, dt, dd, summary, button, code, figcaption"))) {
      if (!el.textContent?.trim() || (el as HTMLElement).offsetParent === null) continue;
      const fg = parse(getComputedStyle(el).color);
      const bg = bgOf(el);
      const [a, b] = [lum(fg), lum(bg)].sort((x, y) => y - x);
      const ratio = (a! + 0.05) / (b! + 0.05);
      if (ratio < 4.5) bad.push(`${el.tagName.toLowerCase()}.${(el as HTMLElement).className} "${el.textContent.trim().slice(0, 30)}" ${ratio.toFixed(2)}`);
    }
    return bad;
  });
}


/** A server-sent event body that sends `events` and ends. */
export function sse(events: { event: string; data: unknown }[]): string {
  return events.map((e) => `event: ${e.event}\ndata: ${JSON.stringify(e.data)}\n\n`).join("");
}
