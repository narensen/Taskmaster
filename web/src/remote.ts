// H5: several machines, one run list. Each paired daemon (Connect → Other machines) is
// polled on its own, with its own reconnect, so the sidebar can show its runs and its own
// "Needs you" beside this daemon's.
//
// Cost: machines × one small request every few seconds, only while the tab is visible.
// Each poll asks for the runs changed since the last one (`?since=`), and a machine that
// does not answer is asked less and less often until it does.

import { useEffect, useRef, useState } from "react";
import { callAt, type RunSummary } from "./api";
import type { OtherDaemon } from "./prefs";

const EVERY_MS = 5000;
const BACKOFF_MS = [5000, 10000, 30000, 60000];

export interface MachineState {
  url: string;
  /** `null` until the first answer. */
  runs: RunSummary[] | null;
  online: boolean;
  /** When an unreachable machine is asked again (ms since the epoch). */
  retryAt: number | null;
  error: string | null;
}

interface Delta {
  runs: RunSummary[];
  seq?: number;
  all_ids?: string[];
  delta?: boolean;
}

/** A machine's run list after one answer: a full list, or the rows that changed since `seq`. */
export function applyRuns(prev: Map<string, RunSummary>, res: Delta): Map<string, RunSummary> | null {
  if (res.delta && res.all_ids) {
    const next = new Map(prev);
    for (const run of res.runs) next.set(run.id, run);
    const alive = new Set(res.all_ids);
    for (const id of [...next.keys()]) if (!alive.has(id)) next.delete(id);
    // A row it never sent: ask for everything.
    return res.all_ids.every((id) => next.has(id)) ? next : null;
  }
  return new Map(res.runs.map((run) => [run.id, run]));
}

function host(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

export const machineName = host;

/** The run lists of the paired daemons, each kept up to date on its own. */
export function useMachines(daemons: OtherDaemon[], signedIn: boolean): { machines: MachineState[]; retry: (url: string) => void } {
  const [machines, setMachines] = useState<Record<string, MachineState>>({});
  const pokes = useRef(new Map<string, () => void>());
  const key = daemons.map((d) => `${d.url} ${d.token ?? ""}`).join("|");

  useEffect(() => {
    if (!signedIn || !daemons.length) {
      setMachines({});
      return;
    }
    const stops: (() => void)[] = [];
    for (const daemon of daemons) {
      let seq: number | undefined;
      let rows = new Map<string, RunSummary>();
      let failures = 0;
      let timer = 0;
      let stopped = false;
      const set = (patch: Partial<MachineState>) =>
        setMachines((all) => ({
          ...all,
          [daemon.url]: { ...(all[daemon.url] ?? { url: daemon.url, runs: null, online: true, retryAt: null, error: null }), ...patch },
        }));
      const poll = async () => {
        if (stopped) return;
        window.clearTimeout(timer);
        if (document.visibilityState !== "visible") {
          timer = window.setTimeout(poll, EVERY_MS);
          return;
        }
        try {
          const res = await callAt<Delta>(daemon.url, daemon.token, seq !== undefined ? `/api/runs?since=${seq}` : "/api/runs");
          const next = applyRuns(rows, res);
          if (!next) {
            seq = undefined;
            rows = new Map();
            timer = window.setTimeout(poll, 0);
            return;
          }
          const changed = next.size !== rows.size || res.runs.length > 0 || failures > 0;
          rows = next;
          seq = typeof res.seq === "number" ? res.seq : undefined;
          failures = 0;
          if (changed || seq === undefined) set({ runs: [...rows.values()], online: true, retryAt: null, error: null });
          timer = window.setTimeout(poll, EVERY_MS);
        } catch (err) {
          const wait = BACKOFF_MS[Math.min(failures, BACKOFF_MS.length - 1)]!;
          failures += 1;
          set({ online: false, retryAt: Date.now() + wait, error: err instanceof Error ? err.message : String(err) });
          timer = window.setTimeout(poll, wait);
        }
      };
      pokes.current.set(daemon.url, () => {
        failures = 0;
        void poll();
      });
      void poll();
      stops.push(() => {
        stopped = true;
        window.clearTimeout(timer);
        pokes.current.delete(daemon.url);
      });
    }
    // Drop machines that were removed.
    setMachines((all) => Object.fromEntries(Object.entries(all).filter(([url]) => daemons.some((d) => d.url === url))));
    return () => stops.forEach((stop) => stop());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, signedIn]);

  return {
    machines: daemons.map((d) => machines[d.url] ?? { url: d.url, runs: null, online: true, retryAt: null, error: null }),
    retry: (url) => pokes.current.get(url)?.(),
  };
}
