import { test as base, expect, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";
import { STATE } from "./setup";

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
  const made = await fetch(`${fixture.url}/api/runs`, { method: "POST", headers, body: JSON.stringify({ id, prompt, yes: true }) });
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

/** A server-sent event body that sends `events` and ends. */
export function sse(events: { event: string; data: unknown }[]): string {
  return events.map((e) => `event: ${e.event}\ndata: ${JSON.stringify(e.data)}\n\n`).join("");
}
