// Who does the work, picked in the composer: the master and its workers (Code), one agent
// that owns the whole project (Agent), or Taskmaster's choice (Auto). The last choice is kept.

import { useCallback, useRef, type KeyboardEvent } from "react";
import { Bot, Network, Wand2 } from "lucide-react";
import type { RunMode } from "../api";
import { setPrefs, usePrefs } from "../prefs";

export const MODES: { id: RunMode; label: string; icon: typeof Bot; title: string }[] = [
  {
    id: "auto",
    label: "Auto",
    icon: Wand2,
    title: "Auto: a fix or a small change goes to one agent, anything bigger to the master and its workers.",
  },
  {
    id: "code",
    label: "Code",
    icon: Network,
    title: "Code: the master plans the work and parallel workers build it.",
  },
  {
    id: "agent",
    label: "Agent",
    icon: Bot,
    title: "Agent: one agent owns the whole project and fixes what is wrong across as many files as it takes. No planning step.",
  },
];

/** The composer's mode, remembered across runs and reloads. */
export function useRunMode(): [RunMode, (mode: RunMode) => void] {
  const mode = usePrefs().runMode ?? "auto";
  const set = useCallback((next: RunMode) => setPrefs({ runMode: next }), []);
  return [mode, set];
}

export function ModePicker({ value, onChange, label = "Mode" }: { value: RunMode; onChange: (mode: RunMode) => void; label?: string }) {
  const group = useRef<HTMLDivElement>(null);
  const onKey = (e: KeyboardEvent) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    const at = MODES.findIndex((m) => m.id === value);
    const next = MODES[(at + (e.key === "ArrowRight" ? 1 : MODES.length - 1)) % MODES.length]!;
    onChange(next.id);
    group.current?.querySelector<HTMLButtonElement>(`[data-mode="${next.id}"]`)?.focus();
  };
  return (
    <div ref={group} className="mode-picker" role="radiogroup" aria-label={label} data-testid="mode-picker" onKeyDown={onKey}>
      {MODES.map((m) => {
        const on = m.id === value;
        const Icon = m.icon;
        return (
          <button
            key={m.id}
            type="button"
            role="radio"
            aria-checked={on}
            tabIndex={on ? 0 : -1}
            data-mode={m.id}
            data-testid={`mode-${m.id}`}
            className={`mode-opt ${on ? "on" : ""}`}
            title={m.title}
            onClick={() => onChange(m.id)}
          >
            <Icon size={12} strokeWidth={1.9} aria-hidden />
            <span>{m.label}</span>
          </button>
        );
      })}
    </div>
  );
}
