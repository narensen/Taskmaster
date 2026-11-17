// The commands behind the / menu and the command palette. A run page registers what can be
// done on it; everything else is navigation.

import { useSyncExternalStore } from "react";
import { navigate } from "./router";

export interface Command {
  id: string;
  label: string;
  group: "Go" | "Run" | "Panel" | "Agent";
  hint?: string;
  run: () => void;
}

export interface RunContext {
  id: string;
  pages: string[];
  focusAgent: (agent: string) => void;
  openTab: (tab: string) => void;
  approve?: () => void;
  reject?: () => void;
}

let runContext: RunContext | null = null;
const listeners = new Set<() => void>();

export function setRunContext(ctx: RunContext | null) {
  runContext = ctx;
  listeners.forEach((l) => l());
}

export function useRunContext(): RunContext | null {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => runContext,
  );
}

export function commands(ctx: RunContext | null = runContext): Command[] {
  const go = (id: string, label: string, to: string): Command => ({ id, label, group: "Go", run: () => navigate(to) });
  const list: Command[] = [
    go("new-run", "New run", "/runs/new"),
    go("gallery", "Gallery", "/"),
    go("dictionary", "Dictionary", "/dictionary"),
    go("errorography", "Errorography", "/errorography"),
    go("reviews", "Reviews", "/reviews"),
    go("skills", "Skills", "/skills"),
    go("providers", "Providers", "/providers"),
    go("settings", "Settings", "/settings"),
  ];
  if (ctx) {
    list.push({ id: "agent-master", label: "Master", group: "Agent", run: () => ctx.focusAgent("master") });
    for (const page of ctx.pages) {
      list.push({ id: `agent-page-${page}`, label: `Worker: ${page}`, group: "Agent", run: () => ctx.focusAgent(`page:${page}`) });
    }
    list.push({ id: "agent-reviewer", label: "Reviewer", group: "Agent", run: () => ctx.focusAgent("reviewer") });
    list.push({ id: "agent-verifier", label: "Verifier", group: "Agent", run: () => ctx.focusAgent("verifier") });
    for (const [tab, label] of [
      ["plan", "Plan"],
      ["diff", "Diff"],
      ["browser", "Browser"],
      ["search", "Web search"],
      ["terminal", "Terminal"],
    ] as const) {
      list.push({ id: `tab-${tab}`, label, group: "Panel", run: () => ctx.openTab(tab) });
    }
    if (ctx.approve) list.push({ id: "approve", label: "Approve", group: "Run", run: ctx.approve });
    if (ctx.reject) list.push({ id: "reject", label: "Reject", group: "Run", run: ctx.reject });
  }
  return list;
}
