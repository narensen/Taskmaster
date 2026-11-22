// The run list everyone shares: loaded once for the shell, grouped and ordered the same
// way in the sidebar, the gallery, the palette and Command-1 to Command-9.

import { createContext, useContext } from "react";
import { api, type RunSummary, type ServerRunFlags } from "./api";
import { toast, undoable } from "./overlay";
import { prefs, setPrefs, type RunFlags } from "./prefs";

export type Group = "Needs you" | "Working" | "Pinned" | "Runs" | "Settled" | "Archived";

export const WORKING = new Set(["planning", "building", "reviewing", "verifying"]);

/** A run's flags: the daemon's (A8). A browser's own flags from before count until they move up. */
export function flagsOf(run: RunSummary): ServerRunFlags {
  const server = run.flags;
  if (server && Object.keys(server).length) return server;
  return prefs().runFlags[run.id] ?? server ?? {};
}

export function groupOf(run: RunSummary, flags: ServerRunFlags | RunFlags | undefined = flagsOf(run)): Group {
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
  return runs
    .map((run) => ({ run, group: groupOf(run) }))
    .filter((x) => showArchived || x.group !== "Archived")
    .sort((a, b) => ORDER.indexOf(a.group) - ORDER.indexOf(b.group) || b.run.id.localeCompare(a.run.id));
}


/** The home strip: last opened in this browser, then the usual gallery order. */
export function byRecent(list: { run: RunSummary; group: Group }[]): { run: RunSummary; group: Group }[] {
  const recent = prefs().recentRuns ?? {};
  return [...list].sort((a, b) => (recent[b.run.id] ?? 0) - (recent[a.run.id] ?? 0) || list.indexOf(a) - list.indexOf(b));
}

type Flag = "pinned" | "settled" | "archived" | "never_settle";

const VERB: Record<Flag, [string, string]> = {
  pinned: ["Pinned", "Unpinned"],
  settled: ["Settled", "Unsettled"],
  archived: ["Archived", "Unarchived"],
  never_settle: ["Taskmaster will not settle", "Taskmaster may settle"],
};

let reloadRuns: () => void = () => {};
/** The shell's run list registers how to reload it, so a flag change shows at once. */
export function setRunsReloader(fn: () => void) {
  reloadRuns = fn;
}

async function setFlag(id: string, flag: Flag, value: boolean) {
  try {
    await api.setRunFlags(id, { [flag]: value });
  } catch (err) {
    toast("The run was not changed", err instanceof Error ? err.message : String(err));
  }
  reloadRuns();
}

/** Flip a flag on the daemon, with Undo for five seconds. */
export function toggleFlag(run: RunSummary, flag: Flag) {
  const before = !!flagsOf(run)[flag];
  void setFlag(run.id, flag, !before);
  undoable(`${VERB[flag][before ? 1 : 0]} ${run.id}`, () => void setFlag(run.id, flag, before));
}

/** Move this browser's flags from before A8 up to the daemon, once per run it knows. */
let migrating = false;
export function migrateLocalFlags(runs: RunSummary[]) {
  const local = prefs().runFlags;
  const ids = Object.keys(local).filter((id) => runs.some((r) => r.id === id));
  if (!ids.length || migrating) return;
  migrating = true;
  void Promise.all(
    ids.map((id) => {
      const mine = local[id] ?? {};
      const theirs = runs.find((r) => r.id === id)?.flags ?? {};
      const missing = Object.fromEntries((["pinned", "settled", "archived"] as const).filter((f) => mine[f] && !theirs[f]).map((f) => [f, true]));
      return Object.keys(missing).length ? api.setRunFlags(id, missing).catch(() => null) : null;
    }),
  ).then(() => {
    setPrefs((p) => ({ runFlags: Object.fromEntries(Object.entries(p.runFlags).filter(([id]) => !ids.includes(id))) }));
    migrating = false;
    reloadRuns();
  });
}

export interface RunsState {
  runs: RunSummary[];
  reload: () => void;
}

export const RunsContext = createContext<RunsState>({ runs: [], reload: () => {} });

export function useRuns(): RunsState {
  return useContext(RunsContext);
}
