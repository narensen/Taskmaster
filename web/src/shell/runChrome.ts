// What the run page puts in the title bar: the token total and a per-role split.
// A store, not React context, so a streaming transcript does not re-render the shell.

import { useSyncExternalStore } from "react";
import type { Totals } from "../api";

export interface RunChrome {
  totals: Totals;
}

let chrome: RunChrome | null = null;
const listeners = new Set<() => void>();

export function setRunChrome(next: RunChrome | null) {
  chrome = next;
  listeners.forEach((l) => l());
}

export function useRunChrome(): RunChrome | null {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => chrome,
  );
}
