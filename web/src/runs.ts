// The run list everyone shares: loaded once for the shell, grouped and ordered the same
// way in the sidebar, the gallery, the palette and Command-1 to Command-9.

import { createContext, useContext } from "react";
import type { RunSummary } from "./api";
import { undoable } from "./overlay";
import { prefs, setRunFlag, type RunFlags } from "./prefs";

export type Group = "Needs you" | "Working" | "Pinned" | "Runs" | "Settled" | "Archived";

export const WORKING = new Set(["planning", "building", "reviewing", "verifying"]);

export function groupOf(run: RunSummary, flags: RunFlags | undefined): Group {
  if (flags?.archived) return "Archived";
  if (run.needs_you || (run.pending_approvals ?? 0) > 0 || run.phase === "awaiting_approval") return "Needs you";
  if (WORKING.has(run.phase) && run.active) return "Working";
  if (flags?.pinned) return "Pinned";
  if (flags?.settled) return "Settled";
  return "Runs";
}

const ORDER: Group[] = ["Needs you", "Working", "Pinned", "Runs", "Settled", "Archived"];

/** Runs in the order they are shown, with their group. Archived ones only when asked. */
export function ordered(runs: RunSummary[], showArchived: boolean): { run: RunSummary; group: Group }[] {
  const flags = prefs().runFlags;
  return runs
    .map((run) => ({ run, group: groupOf(run, flags[run.id]) }))
    .filter((x) => showArchived || x.group !== "Archived")
    .sort((a, b) => ORDER.indexOf(a.group) - ORDER.indexOf(b.group) || b.run.id.localeCompare(a.run.id));
}


/** The home strip: last opened in this browser, then the usual gallery order. */
export function byRecent(list: { run: RunSummary; group: Group }[]): { run: RunSummary; group: Group }[] {
  const recent = prefs().recentRuns ?? {};
  return [...list].sort((a, b) => (recent[b.run.id] ?? 0) - (recent[a.run.id] ?? 0) || list.indexOf(a) - list.indexOf(b));
}

const VERB: Record<keyof RunFlags, [string, string]> = {
  pinned: ["Pinned", "Unpinned"],
  settled: ["Settled", "Unsettled"],
  archived: ["Archived", "Unarchived"],
};

/** Flip a flag, with Undo for five seconds. */
export function toggleFlag(id: string, flag: keyof RunFlags) {
  const before = !!prefs().runFlags[id]?.[flag];
  setRunFlag(id, flag, !before);
  undoable(`${VERB[flag][before ? 1 : 0]} ${id}`, () => setRunFlag(id, flag, before));
}

export interface RunsState {
  runs: RunSummary[];
  reload: () => void;
}

export const RunsContext = createContext<RunsState>({ runs: [], reload: () => {} });

export function useRuns(): RunsState {
  return useContext(RunsContext);
}
