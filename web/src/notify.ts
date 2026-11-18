// What the app says when a run moves on: a calm toast in the window, a desktop
// notification when the window is not in front (the Tauri notification plugin in the
// desktop app, the Notification API in a browser), and a soft tick when a run finishes,
// if the person turned it on. Clicking a notification opens the run.

import type { RunSummary } from "./api";
import { isDesktop } from "./desktop";
import { toast } from "./overlay";
import { prefs } from "./prefs";
import { navigate } from "./router";

const open = (id: string) => navigate(`/runs/${encodeURIComponent(id)}`);

export function runStarted(id: string) {
  toast("Run started", `${id} is planning`, 3500, { action: { label: "Open", run: () => open(id) } });
}

export function pageDone(run: string, page: string) {
  toast(`Page ${page} done`, `${run} · its checks passed`, 3500);
}

export function checksFailed(run: string, page: string, detail?: string) {
  toast(`Checks failed on ${page}`, detail ? `${run} · ${detail.split("\n")[0]!.slice(0, 120)}` : run, 6000, {
    action: { label: "Open", run: () => open(run) },
  });
}

// The tick: a short, quiet sine with a fast fall, made here so no sound file ships.
let audio: AudioContext | null = null;
export function tick() {
  try {
    audio ??= new AudioContext();
    const now = audio.currentTime;
    const osc = audio.createOscillator();
    const gain = audio.createGain();
    osc.type = "sine";
    osc.frequency.setValueAtTime(1320, now);
    osc.frequency.exponentialRampToValueAtTime(880, now + 0.08);
    gain.gain.setValueAtTime(0.0001, now);
    gain.gain.exponentialRampToValueAtTime(0.05, now + 0.008);
    gain.gain.exponentialRampToValueAtTime(0.0001, now + 0.16);
    osc.connect(gain).connect(audio.destination);
    osc.start(now);
    osc.stop(now + 0.18);
  } catch {
    // No audio device: nothing to play.
  }
}

interface TauriNotification {
  isPermissionGranted: () => Promise<boolean>;
  requestPermission: () => Promise<string>;
  sendNotification: (options: { title: string; body?: string }) => void;
}

function tauriNotification(): TauriNotification | null {
  return (window as unknown as { __TAURI__?: { notification?: TauriNotification } }).__TAURI__?.notification ?? null;
}

/** Ask once, from a click in Settings. True when notifications may be shown. */
export async function allowNotifications(): Promise<boolean> {
  const t = tauriNotification();
  if (t) return (await t.isPermissionGranted()) || (await t.requestPermission()) === "granted";
  if (!("Notification" in window)) return false;
  if (Notification.permission === "granted") return true;
  return (await Notification.requestPermission()) === "granted";
}

// The desktop plugin does not report clicks on desktop systems, so a notification marks
// its run, and the window coming to the front soon after opens it.
let clicked: { id: string; until: number } | null = null;
window.addEventListener("focus", () => {
  if (clicked && clicked.until > Date.now()) open(clicked.id);
  clicked = null;
});

async function desktop(title: string, body: string, run: string) {
  if (!prefs().notify || document.hasFocus()) return;
  const t = tauriNotification();
  if (t) {
    if (!(await t.isPermissionGranted())) return;
    t.sendNotification({ title, body });
    clicked = { id: run, until: Date.now() + 60_000 };
    return;
  }
  if (!isDesktop() && "Notification" in window && Notification.permission === "granted") {
    const n = new Notification(title, { body, tag: `taskmaster-${run}` });
    n.onclick = () => {
      window.focus();
      open(run);
      n.close();
    };
  }
}

const WORKING = new Set(["planning", "building", "reviewing", "verifying"]);
let seen: Map<string, { phase: string; asks: number }> | null = null;

/** Compare the run list with the last one and tell the person what changed. */
export function watchRuns(runs: RunSummary[]) {
  const before = seen;
  seen = new Map(runs.map((r) => [r.id, { phase: r.phase, asks: r.pending_approvals ?? 0 }]));
  if (!before) return;
  for (const run of runs) {
    const was = before.get(run.id);
    if (!was) continue;
    const asks = run.pending_approvals ?? 0;
    if (was.phase !== run.phase) {
      if (run.phase === "done" && WORKING.has(was.phase)) {
        if (prefs().sound) tick();
        void desktop("Run finished", `${run.id}: ${run.pages.filter((p) => p.state === "passed").length} of ${run.pages.length} pages passed`, run.id);
      } else if (run.phase === "failed" && WORKING.has(was.phase) && !run.cancelled) {
        void desktop("Run failed", `${run.id}${run.error ? `: ${run.error.slice(0, 120)}` : ""}`, run.id);
      } else if (run.phase === "awaiting_approval") {
        void desktop("A plan is waiting for you", `${run.id} needs your approval to build`, run.id);
      }
    } else if (asks > was.asks) {
      void desktop("A tool call is waiting for you", `${run.id} asks before running a risky tool`, run.id);
    }
  }
}
