// What this browser remembers: settings that are the person's, not the daemon's. All of
// it is in localStorage, so it never leaves the device. No secret is kept here except the
// pairing token, which api.ts owns and nothing displays.

import { useSyncExternalStore } from "react";
import type { RoleName } from "./api";

export type Density = "comfortable" | "compact";
export type ThemeName = "dark" | "light";

export interface RoleDefault {
  harness: string;
  model: string | null;
}

export interface OtherDaemon {
  url: string;
  token: string | null;
  pinned: boolean;
}

export interface RunFlags {
  pinned?: boolean;
  settled?: boolean;
  archived?: boolean;
}

export interface Prefs {
  editor: string | null;
  searchEndpoint: string;
  motion: boolean;
  density: Density;
  theme: ThemeName;
  roleDefaults: Partial<Record<RoleName, RoleDefault>>;
  daemons: OtherDaemon[];
  runFlags: Record<string, RunFlags>;
  history: string[];
  panelTab: Record<string, string>;
  viewed: Record<string, string>;
  showArchived: boolean;
  /** When this browser last opened a run, for the home strip. */
  recentRuns: Record<string, number>;
  /** The provider and model last used for each role, per project path. */
  lastRoles: Record<string, Partial<Record<RoleName, RoleDefault>>>;
  /** Pages at once last chosen: a number, or "max" for what this machine can run. */
  parallel: number | "max" | null;
  /** A soft tick when a run finishes. */
  sound: boolean;
  /** Desktop notifications when a run finishes, fails or waits on the person. */
  notify: boolean;
  /** The quick terminal's height in pixels, and whether it was open. */
  termHeight: number;
  /** The project this browser had open, opened again on the next launch. */
  lastProject: string | null;
  /** Files open in the files panel, per project, and the active one. */
  openFiles: Record<string, { tabs: string[]; active: string | null }>;
  /** Whether the files panel shows a run's change to the file instead of the file. */
  fileDiff: boolean;
  /** Whether a run's side panel is open. */
  runPanel: boolean;
  /** The composer's mode last chosen: master and workers, one agent, or auto. */
  runMode: "auto" | "code" | "agent";
  /** Default action on Enter while a run works: Queue for next turn or Steer active run (A1). */
  chatSendMode: "queue" | "steer";
  /** Last seen terminal state fingerprint per run id, so runs finishing elsewhere stay bold until opened (H1). */
  seenRuns: Record<string, string>;
}

const KEY = "taskmaster.prefs";

const DEFAULTS: Prefs = {
  editor: null,
  searchEndpoint: "",
  motion: true,
  density: "comfortable",
  theme: "dark",
  roleDefaults: {},
  daemons: [],
  runFlags: {},
  history: [],
  panelTab: {},
  viewed: {},
  showArchived: false,
  recentRuns: {},
  lastRoles: {},
  parallel: null,
  sound: false,
  notify: true,
  termHeight: 280,
  lastProject: null,
  openFiles: {},
  fileDiff: false,
  runPanel: true,
  runMode: "auto",
  chatSendMode: "queue",
  seenRuns: {},
};

/** Stable key representing a run's latest finished state, or null while still running/idle (H1). */
export function runFinishKey(run: { phase: string; active: boolean; last_at?: string | null; phase_at?: string | number | null }): string | null {
  if (run.active) return null;
  if (run.phase !== "done" && run.phase !== "failed") return null;
  return `${run.phase}:${run.last_at ?? run.phase_at ?? ""}`;
}

export function isRunUnread(
  run: { id: string; phase: string; active: boolean; last_at?: string | null; phase_at?: string | number | null },
  seen: Record<string, string> | undefined,
): boolean {
  const key = runFinishKey(run);
  if (!key) return false;
  return seen?.[run.id] !== key;
}

export function markRunSeen(run: { id: string; phase: string; active: boolean; last_at?: string | null; phase_at?: string | number | null }) {
  const key = runFinishKey(run);
  if (!key) return;
  const cur = prefs().seenRuns ?? {};
  if (cur[run.id] === key) return;
  setPrefs({ seenRuns: { ...cur, [run.id]: key } });
}

function load(): Prefs {
  try {
    const raw = localStorage.getItem(KEY);
    return raw ? { ...DEFAULTS, ...(JSON.parse(raw) as Partial<Prefs>) } : DEFAULTS;
  } catch {
    return DEFAULTS;
  }
}

let current = load();
const listeners = new Set<() => void>();

export function prefs(): Prefs {
  return current;
}

export function setPrefs(change: Partial<Prefs> | ((p: Prefs) => Partial<Prefs>)) {
  const patch = typeof change === "function" ? change(current) : change;
  current = { ...current, ...patch };
  try {
    localStorage.setItem(KEY, JSON.stringify(current));
  } catch {
    // A full or blocked storage keeps the change for this tab only.
  }
  listeners.forEach((l) => l());
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

window.addEventListener("storage", (event) => {
  if (event.key === KEY) {
    current = load();
    listeners.forEach((l) => l());
  }
});

export function usePrefs(): Prefs {
  return useSyncExternalStore(subscribe, prefs);
}

export function rememberPrompt(prompt: string) {
  const text = prompt.trim();
  if (!text) return;
  setPrefs((p) => ({ history: [...p.history.filter((h) => h !== text), text].slice(-50) }));
}


export function rememberRun(id: string) {
  if (!id) return;
  setPrefs((p) => ({ recentRuns: { ...p.recentRuns, [id]: Date.now() } }));
}

export function setRunFlag(id: string, flag: keyof RunFlags, value: boolean) {
  setPrefs((p) => ({ runFlags: { ...p.runFlags, [id]: { ...p.runFlags[id], [flag]: value } } }));
}

/** Whether animation should run: the system setting and the switch in Settings. */
export function useMotion(): boolean {
  const { motion } = usePrefs();
  const reduced = useSyncExternalStore(
    (l) => {
      const q = window.matchMedia("(prefers-reduced-motion: reduce)");
      q.addEventListener("change", l);
      return () => q.removeEventListener("change", l);
    },
    () => window.matchMedia("(prefers-reduced-motion: reduce)").matches,
  );
  return motion && !reduced;
}

export function motionOn(): boolean {
  return current.motion && !window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}
