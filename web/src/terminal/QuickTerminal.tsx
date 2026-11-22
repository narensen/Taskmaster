// The quick terminal: Command-J (Ctrl-J elsewhere) slides it up from the bottom, in the
// open project's folder, on the daemon's PTY. Several tabs; drag the top edge to resize;
// the height is remembered. Hiding it keeps every shell running.

import { Plus, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { api, type TerminalInfo } from "../api";
import { toast, useOn } from "../overlay";
import { setPrefs, usePrefs } from "../prefs";
import { useHealth, useSettings } from "../query";
import { TerminalSession } from "./Xterm";
import { MOD } from "../desktop";

const MIN = 140;

export function QuickTerminal({ open, onClose }: { open: boolean; onClose: () => void }) {
  const p = usePrefs();
  const settings = useSettings();
  // A daemon with no terminal (Windows, for now) says so here instead of failing to open one.
  const health = useHealth();
  const unavailable = health.data?.terminal === false;
  const [tabs, setTabs] = useState<TerminalInfo[]>([]);
  const [active, setActive] = useState<string | null>(null);
  const [height, setHeight] = useState(p.termHeight);
  const [alt, setAlt] = useState<Record<string, boolean>>({});
  const [loaded, setLoaded] = useState(false);
  const box = useRef<HTMLDivElement>(null);
  const project = settings.data?.project ?? "";

  const add = useCallback(async () => {
    try {
      const t = await api.openTerminal(100, 20);
      setTabs((all) => [...all, t]);
      setActive(t.id);
    } catch (err) {
      toast("The terminal did not open", err instanceof Error ? err.message : String(err));
    }
  }, []);

  // Shells the daemon still has (from before a reload) come back as tabs.
  useEffect(() => {
    if (!open || loaded || !health.isSuccess || unavailable) return;
    setLoaded(true);
    void api
      .terminals()
      .then((list) => {
        const alive = list.filter((t) => t.alive);
        if (alive.length) {
          setTabs(alive);
          setActive(alive[alive.length - 1]!.id);
        } else void add();
      })
      .catch(() => void add());
  }, [open, loaded, add, health.isSuccess, unavailable]);

  const close = (id: string) => {
    void api.closeTerminal(id).catch(() => undefined);
    setTabs((all) => {
      const next = all.filter((t) => t.id !== id);
      if (active === id) setActive(next[next.length - 1]?.id ?? null);
      if (!next.length) onClose();
      return next;
    });
  };

  useOn("terminal-new", () => {
    if (!unavailable) void add();
  });

  // Escape closes, unless a full-screen program in the terminal wants it.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || !box.current?.contains(document.activeElement)) return;
      if (active && alt[active]) return;
      e.preventDefault();
      onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [open, active, alt, onClose]);

  // Resize from the top edge. The height changes once a frame and is saved at the end.
  const drag = (e: React.PointerEvent) => {
    e.preventDefault();
    const startY = e.clientY;
    const startH = height;
    const max = window.innerHeight * 0.85;
    let frame = 0;
    let next = startH;
    const move = (ev: PointerEvent) => {
      next = Math.round(Math.min(max, Math.max(MIN, startH + (startY - ev.clientY))));
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => setHeight(next));
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      setPrefs({ termHeight: next });
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  const nudge = (e: React.KeyboardEvent) => {
    if (e.key !== "ArrowUp" && e.key !== "ArrowDown") return;
    e.preventDefault();
    const next = Math.min(window.innerHeight * 0.85, Math.max(MIN, height + (e.key === "ArrowUp" ? 24 : -24)));
    setHeight(next);
    setPrefs({ termHeight: next });
  };

  return (
    <section
      ref={box}
      className={`qterm ${open ? "open" : ""}`}
      aria-label="Terminal"
      aria-hidden={!open}
      inert={!open}
      style={{ height }}
    >
      <div className="qterm-grip" role="separator" aria-orientation="horizontal" aria-label="Resize the terminal" tabIndex={0} onPointerDown={drag} onKeyDown={nudge} />
      <div className="qterm-bar">
        <div className="qterm-tabs" role="tablist" aria-label="Shells">
          {tabs.map((t, i) => (
            <div key={t.id} className={`qterm-tab ${t.id === active ? "on" : ""}`}>
              <button type="button" role="tab" aria-selected={t.id === active} className="qterm-tab-name" onClick={() => setActive(t.id)}>
                {t.shell.split("/").pop()} {i + 1}
              </button>
              <button type="button" className="qterm-x" aria-label={`Close shell ${i + 1}`} onClick={() => close(t.id)}>
                <X size={12} />
              </button>
            </div>
          ))}
          {!unavailable && (
            <button type="button" className="tb-btn" aria-label="New shell" onClick={() => void add()}>
              <Plus size={13} />
            </button>
          )}
        </div>
        <span className="qterm-cwd mono clip" title={project}>
          {project}
        </span>
        <span className="qterm-hint">
          <kbd>esc</kbd> or <kbd>{MOD} J</kbd> hides it
        </span>
        <button type="button" className="tb-btn" aria-label="Hide the terminal" onClick={onClose}>
          <X size={14} />
        </button>
      </div>
      <div className="qterm-body">
        {unavailable && (
          <p className="qterm-none" role="status">
            The terminal is not available on Windows yet. Runs, previews, and everything else in Taskmaster work as usual.
          </p>
        )}
        {!unavailable && tabs.length === 0 && (
          <div className="qterm-skel" aria-hidden>
            <span className="skeleton" style={{ width: "38%" }} />
            <span className="skeleton" style={{ width: "22%" }} />
          </div>
        )}
        {tabs.map((t) => (
          <div key={t.id} className="qterm-pane" hidden={t.id !== active}>
            <TerminalSession
              id={t.id}
              active={open && t.id === active}
              onExit={() => setTabs((all) => all.map((x) => (x.id === t.id ? { ...x, alive: false } : x)))}
              onAltScreen={(v) => setAlt((a) => ({ ...a, [t.id]: v }))}
            />
          </div>
        ))}
      </div>
    </section>
  );
}
