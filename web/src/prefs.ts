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
};

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
