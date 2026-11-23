// Who does the work, picked in the composer: the master and its workers (Code), one agent
// that owns the whole project (Agent), or Taskmaster's choice (Auto). The last choice is kept.

import { useCallback, useRef, type KeyboardEvent } from "react";
import { Bot, Network, ScanSearch, Wand2 } from "lucide-react";
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

/** On a finished run the picker also offers Review: one reviewer reads the run's changes. */
export type PickMode = RunMode | "review";

const REVIEW = {
  id: "review" as const,
  label: "Review",
  icon: ScanSearch,
  title: "Review: one reviewer reads this run's changes and a second checks what it reports. What you type is what it looks at in particular. Nothing is changed.",
};

/** The composer's mode, remembered across runs and reloads. */
export function useRunMode(): [RunMode, (mode: RunMode) => void] {
  const mode = usePrefs().runMode ?? "auto";
  const set = useCallback((next: RunMode) => setPrefs({ runMode: next }), []);
  return [mode, set];
}

export function ModePicker<M extends PickMode = RunMode>({
  value,
  onChange,
  label = "Mode",
  review = false,
}: {
  value: M;
  onChange: (mode: M) => void;
  label?: string;
  /** Offer Review too. */
  review?: boolean;
}) {
  const group = useRef<HTMLDivElement>(null);
  const modes = (review ? [...MODES, REVIEW] : MODES) as { id: M; label: string; icon: typeof Bot; title: string }[];
  const onKey = (e: KeyboardEvent) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    const at = modes.findIndex((m) => m.id === value);
    const next = modes[(at + (e.key === "ArrowRight" ? 1 : modes.length - 1)) % modes.length]!;
    onChange(next.id);
    group.current?.querySelector<HTMLButtonElement>(`[data-mode="${next.id}"]`)?.focus();
  };
  return (
    <div ref={group} className="mode-picker" role="radiogroup" aria-label={label} data-testid="mode-picker" onKeyDown={onKey}>
      {modes.map((m) => {
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
