// An earlier turn of a run, folded to one line: what was asked, who took it, what it
// changed. Opened, it is that turn's stream as it ended. The newest turn sits below the
// older ones, so a run reads top to bottom like a conversation.

import { memo, useCallback, useMemo, useState } from "react";
import { AlertCircle, Check, ChevronRight, Square } from "lucide-react";
import type { AgentEntry, LedgerEntry, RunDetail } from "../api";
import { Flow } from "./Flow";
import { turnSurfaces, type Turn } from "./model";

function firstLine(text: string | null | undefined): string {
  const line = (text ?? "").split("\n").find((l) => l.trim()) ?? "";
  return line.length > 140 ? `${line.slice(0, 139).trimEnd()}…` : line;
}

export const PastTurn = memo(function PastTurn({
  turn,
  next,
  entries,
  detail,
  ledger,
}: {
  turn: Turn;
  /** The turn after this one, which bounds this one's spend. */
  next: Turn | undefined;
  entries: AgentEntry[];
  detail: RunDetail;
  ledger: LedgerEntry[];
}) {
  const [open, setOpen] = useState(false);
  const [blown, setBlown] = useState<Set<string>>(() => new Set());
  const until = next?.at ?? null;
  const list = useMemo(() => turnSurfaces(entries, turn, detail, ledger, until), [entries, turn, detail, ledger, until]);
  const past = useMemo<RunDetail>(
    () => ({ ...detail, plan: turn.plan, pages: [], active: false, cancelled: false, cancelling: null, phase: "done" }),
    [detail, turn.plan],
  );
  const ran = list.filter((s) => s.calls.length > 0);
  const edited = new Set(ran.flatMap((s) => s.summary.editedFiles));
  const failed = ran.some((s) => s.state === "failed");
  const stopped = !failed && ran.some((s) => s.state === "stopped");
  const agents = ran.filter((s) => s.id !== "master").length;
  const asked = turn.n === 1 ? detail.prompt : turn.text;
  const toggle = useCallback((id: string) => {
    setBlown((cur) => {
      const nextSet = new Set(cur);
      if (nextSet.has(id)) nextSet.delete(id);
      else nextSet.add(id);
      return nextSet;
    });
  }, []);

  const bits: string[] = [];
  if (ran.some((s) => s.id === "master")) bits.push("master");
  if (agents) bits.push(`${agents} ${agents === 1 ? "agent" : "agents"}`);
  if (edited.size) bits.push(`${edited.size} file${edited.size === 1 ? "" : "s"} edited`);

  return (
    <section className={`past-turn ${open ? "open" : ""}`} data-testid="past-turn" aria-label={`Turn ${turn.n}`}>
      <button type="button" className="past-turn-head" aria-expanded={open} onClick={() => setOpen((v) => !v)}>
        <ChevronRight size={14} className={`agent-chev ${open ? "open" : ""}`} aria-hidden />
        <span className="past-turn-n mono">{turn.n === 1 ? "Request" : `Follow-up ${turn.n - 1}`}</span>
        <span className="past-turn-ask clip">{firstLine(asked) || "No message"}</span>
        <span className="past-turn-meta mono">{bits.join(" · ")}</span>
        {failed ? (
          <AlertCircle size={13} className="agent-ico failed" aria-label="failed" />
        ) : stopped ? (
          <Square size={10} strokeWidth={2.2} className="agent-ico stopped" aria-label="stopped" />
        ) : (
          <Check size={13} className="agent-ico done" aria-label="done" />
        )}
      </button>
      {open && (
        <div className="past-turn-body">
          <Flow detail={past} list={list} focus={null} blown={blown} onToggleAgent={toggle} turn={{ n: turn.n, text: turn.text, mode: turn.mode, past: true }} />
        </div>
      )}
    </section>
  );
});
