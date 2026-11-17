import { test as base, expect, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";
import { STATE } from "./setup";

export interface Daemon {
  url: string;
  token: string;
  project: string;
}

export const test = base.extend<{ daemon: Daemon; open: (path: string) => Promise<Page> }>({
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
});

export { expect };

/** A server-sent event body that sends `events` and ends. */
export function sse(events: { event: string; data: unknown }[]): string {
  return events.map((e) => `event: ${e.event}\ndata: ${JSON.stringify(e.data)}\n\n`).join("");
}
