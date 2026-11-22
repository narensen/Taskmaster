// The daemon stopped answering: say so in one line, and keep trying.
//
// Retries back off with jitter (1 s doubling to 30 s), so every open tab does not knock in
// the same second after a restart. Coming back to the tab, or the network coming back,
// probes at once. When the daemon answers, cached data is refetched and the line goes.

import { useCallback, useEffect, useRef, useState } from "react";
import { RefreshCw, WifiOff } from "lucide-react";
import { api, daemonReachable, server } from "../api";
import { queryClient } from "../query";
import { toast } from "../overlay";

const FIRST = 1000;
const MOST = 30_000;

export function ConnectionBanner() {
  const [down, setDown] = useState(() => !daemonReachable());
  const [next, setNext] = useState<number | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const delay = useRef(FIRST);
  const timer = useRef(0);

  const probe = useCallback(async () => {
    window.clearTimeout(timer.current);
    setNext(null);
    try {
      await api.health();
    } catch {
      const wait = Math.min(MOST, delay.current) * (0.75 + Math.random() * 0.5);
      delay.current = Math.min(MOST, delay.current * 2);
      setNext(Date.now() + wait);
      timer.current = window.setTimeout(() => void probe(), wait);
    }
  }, []);

  useEffect(() => {
    const onChange = (event: Event) => {
      const up = (event as CustomEvent<boolean>).detail;
      setDown(!up);
      if (up) {
        window.clearTimeout(timer.current);
        delay.current = FIRST;
        setNext(null);
        void queryClient.invalidateQueries();
        window.dispatchEvent(new Event("taskmaster-reconnected"));
        toast("Reconnected", undefined, 2500);
      } else {
        delay.current = FIRST;
        void probe();
      }
    };
    const soon = () => {
      if (!daemonReachable() && document.visibilityState === "visible") {
        delay.current = FIRST;
        void probe();
      }
    };
    window.addEventListener("taskmaster-connection", onChange);
    window.addEventListener("online", soon);
    document.addEventListener("visibilitychange", soon);
    return () => {
      window.removeEventListener("taskmaster-connection", onChange);
      window.removeEventListener("online", soon);
      document.removeEventListener("visibilitychange", soon);
      window.clearTimeout(timer.current);
    };
  }, [probe]);

  // Only the countdown ticks, and only while the line shows.
  useEffect(() => {
    if (!down) return;
    const tick = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(tick);
  }, [down]);

  if (!down) return null;
  const where = server() || window.location.origin;
  const left = next ? Math.max(0, Math.ceil((next - now) / 1000)) : 0;
  return (
    <div className="connection-banner" role="status" aria-live="polite" data-testid="connection-banner">
      <WifiOff size={14} aria-hidden />
      <span>
        Cannot reach Taskmaster at <span className="mono">{where.replace(/^https?:\/\//, "")}</span>.{" "}
        {next ? `Trying again in ${left}s.` : "Trying now…"} Runs keep going on the daemon; this page catches up when it answers.
      </span>
      <button
        type="button"
        className="ghost small"
        onClick={() => {
          delay.current = FIRST;
          void probe();
        }}
      >
        <RefreshCw size={12} aria-hidden /> Retry now
      </button>
    </div>
  );
}
