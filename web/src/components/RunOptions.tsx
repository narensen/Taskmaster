// Two run options beside the mode picker in a composer: the run's permission mode (A3) and
// a goal the daemon checks after each turn (A4). Neither is remembered across runs: a run
// that skips every question, or keeps going on its own, is asked for each time.

import * as Popover from "@radix-ui/react-popover";
import { Check, ChevronDown, ShieldCheck, Target, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { GoalProgress, PermissionMode } from "../api";

export const PERMISSIONS: { id: PermissionMode; label: string; desc: string }[] = [
  {
    id: "supervised",
    label: "Supervised",
    desc: "Asks before every tool call.",
  },
  {
    id: "auto_edits",
    label: "Auto-accept edits",
    desc: "Edits in the run's files go through. Shell commands, network fetches and writes elsewhere still ask.",
  },
  {
    id: "auto",
    label: "Auto",
    desc: "Never asks. The worktree, the sandbox and the owned files still hold.",
  },
  {
    id: "full_access",
    label: "Full access",
    desc: "Never asks and lifts the sandbox: agents can run any command and edit any file on this machine.",
  },
];

/** Where a provider cannot stop to ask, said once under the list. */
const ASK_NOTE =
  "Claude Code and Codex can only stop to ask inside Code Mode (run_code). With their own tools they accept edits in the worktree and stay sandboxed.";

export function permissionLabel(mode: PermissionMode | null | undefined): string {
  return PERMISSIONS.find((p) => p.id === mode)?.label ?? "As in Settings";
}

export function PermissionPicker({ value, onChange }: { value: PermissionMode | null; onChange: (mode: PermissionMode | null) => void }) {
  const [open, setOpen] = useState(false);
  const pick = (mode: PermissionMode | null) => {
    onChange(mode);
    setOpen(false);
  };
  const options: { id: PermissionMode | null; label: string; desc: string }[] = [
    { id: null, label: "As in Settings", desc: "The approval policy in Settings → Approvals." },
    ...PERMISSIONS,
  ];
  return (
    <Popover.Root open={open} onOpenChange={setOpen} modal={false}>
      <Popover.Trigger asChild>
        <button
          type="button"
          className={`model-pill run-opt ${value ? "on" : ""} ${value === "full_access" ? "risky" : ""}`}
          aria-label={`Permissions: ${permissionLabel(value)}`}
          title="Permissions for this run"
          data-testid="permission-picker"
        >
          <ShieldCheck size={12} strokeWidth={1.9} aria-hidden />
          <span>{value ? permissionLabel(value) : "Permissions"}</span>
          <ChevronDown size={11} strokeWidth={1.8} aria-hidden />
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content side="top" align="start" sideOffset={6} collisionPadding={12}>
          <div className="pop run-opt-pop" role="dialog" aria-label="Permissions for this run" data-testid="permission-popover">
            <div className="run-opt-list" role="radiogroup" aria-label="Permission mode">
              {options.map((o) => {
                const on = o.id === value;
                return (
                  <button
                    key={o.id ?? "settings"}
                    type="button"
                    role="radio"
                    aria-checked={on}
                    className={`run-opt-row ${on ? "on" : ""}`}
                    data-testid={`permission-${o.id ?? "settings"}`}
                    onClick={() => pick(o.id)}
                  >
                    <span className="run-opt-check" aria-hidden>
                      {on && <Check size={12} strokeWidth={2.2} />}
                    </span>
                    <span className="run-opt-body">
                      <span className="run-opt-title">{o.label}</span>
                      <span className="run-opt-desc">{o.desc}</span>
                    </span>
                  </button>
                );
              })}
            </div>
            <p className="run-opt-note">{ASK_NOTE}</p>
          </div>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

export interface GoalChoice {
  condition: string;
  maxTurns: number;
}

const GOAL_IDEAS = ["All tests pass", "The preview opens", "`npm test` is green", "`cargo test` is green"];

export function GoalButton({ value, onChange }: { value: GoalChoice | null; onChange: (goal: GoalChoice | null) => void }) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState(value?.condition ?? "");
  const [turns, setTurns] = useState(value?.maxTurns ?? 5);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (!open) return;
    setText(value?.condition ?? "");
    setTurns(value?.maxTurns ?? 5);
  }, [open, value]);
  const save = () => {
    const condition = text.trim();
    onChange(condition ? { condition, maxTurns: Math.min(20, Math.max(1, Math.round(turns) || 5)) } : null);
    setOpen(false);
  };
  return (
    <Popover.Root open={open} onOpenChange={setOpen} modal={false}>
      <Popover.Trigger asChild>
        <button
          type="button"
          className={`model-pill run-opt ${value ? "on" : ""}`}
          aria-label={value ? `Goal: ${value.condition}` : "Set a goal"}
          title={value ? `Keeps going until: ${value.condition} (up to ${value.maxTurns} turns)` : "Keep going until a condition holds"}
          data-testid="goal-button"
        >
          <Target size={12} strokeWidth={1.9} aria-hidden />
          <span className="run-opt-clip">{value ? value.condition : "Goal"}</span>
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          side="top"
          align="start"
          sideOffset={6}
          collisionPadding={12}
          onOpenAutoFocus={(e) => {
            e.preventDefault();
            input.current?.focus();
          }}
        >
          <form
            className="pop run-opt-pop goal-pop"
            aria-label="Goal for this run"
            data-testid="goal-popover"
            onSubmit={(e) => {
              e.preventDefault();
              save();
            }}
          >
            <label className="goal-field">
              <span className="run-opt-title">Keep going until</span>
              <input
                ref={input}
                value={text}
                onChange={(e) => setText(e.target.value)}
                placeholder="All tests pass"
                aria-label="Goal condition"
                data-testid="goal-input"
              />
            </label>
            <div className="goal-ideas">
              {GOAL_IDEAS.map((idea) => (
                <button key={idea} type="button" className="chip-btn" onClick={() => setText(idea)}>
                  {idea}
                </button>
              ))}
            </div>
            <p className="run-opt-desc">
              After each turn the daemon checks it: the project's tests, a command in backticks, or the preview. If it does not hold, one agent
              gets the failure as its next task.
            </p>
            <div className="goal-foot">
              <label className="goal-turns">
                <span>Up to</span>
                <input
                  type="number"
                  min={1}
                  max={20}
                  value={turns}
                  onChange={(e) => setTurns(Number(e.target.value))}
                  aria-label="Most turns"
                  data-testid="goal-turns"
                />
                <span>turns</span>
              </label>
              {value && (
                <button
                  type="button"
                  className="ghost small"
                  onClick={() => {
                    onChange(null);
                    setOpen(false);
                  }}
                >
                  <X size={12} aria-hidden /> Clear
                </button>
              )}
              <button type="submit" className="primary small" data-testid="goal-save">
                Set goal
              </button>
            </div>
          </form>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

/** The goal line in a run's header: the condition, the turns used, and where it stands. */
export function GoalStatus({ goal }: { goal: GoalProgress }) {
  const where =
    goal.state === "met"
      ? "met"
      : goal.state === "stopped"
        ? "not met, stopped"
        : goal.state === "checking"
          ? "checking…"
          : goal.attempts === 0
            ? "checked after this turn"
            : "not met yet";
  return (
    <span className={`goal-status ${goal.state}`} data-testid="goal-status" title={goal.last_check ?? undefined}>
      <Target size={12} strokeWidth={1.9} aria-hidden />
      <span className="goal-cond clip">{goal.condition}</span>
      <span className="muted mono">
        {goal.attempts}/{goal.max_turns}
      </span>
      <span className="goal-where">{where}</span>
    </span>
  );
}
