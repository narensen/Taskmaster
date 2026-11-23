// H5: the runs of paired daemons, under this one's in the sidebar. Each machine has its own
// line (reachable or not, and how many runs need you there), its own Retry, and its runs in
// the same order as here. A run opens in that machine's own app.

import { AlertCircle, Check, ChevronDown, Circle, CircleAlert, Loader2, Monitor, RefreshCw } from "lucide-react";
import { memo, useEffect, useState } from "react";
import { usePrefs } from "../prefs";
import { machineName, useMachines, type MachineState } from "../remote";
import { ordered, type Group } from "../runs";

const SHOWN = 8;

function Mark({ group, phase }: { group: Group; phase: string }) {
  if (group === "Needs you") return <CircleAlert size={13} aria-hidden />;
  if (group === "Working") return <Loader2 size={13} className="spin-ico" aria-hidden />;
  if (phase === "done") return <Check size={13} aria-hidden />;
  if (phase === "failed") return <AlertCircle size={13} aria-hidden />;
  return <Circle size={13} aria-hidden />;
}

function useNow(on: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!on) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [on]);
  return now;
}

const Machine = memo(function Machine({ machine, onRetry }: { machine: MachineState; onRetry: () => void }) {
  const { showArchived } = usePrefs();
  const [folded, setFolded] = useState(false);
  const list = machine.runs ? ordered(machine.runs, showArchived) : [];
  const needs = list.filter((x) => x.group === "Needs you").length;
  const working = list.filter((x) => x.group === "Working").length;
  const now = useNow(!machine.online);
  const wait = machine.retryAt ? Math.max(0, Math.ceil((machine.retryAt - now) / 1000)) : 0;
  const name = machineName(machine.url);
  return (
    <section className={`machine ${machine.online ? "" : "offline"}`} data-testid="machine" data-machine={name} aria-label={`Runs on ${name}`}>
      <div className="machine-head">
        <button type="button" className="group-head machine-fold" aria-expanded={!folded} onClick={() => setFolded((f) => !f)}>
          <span className="machine-name">
            <Monitor size={12} aria-hidden />
            <span className="clip">{name}</span>
          </span>
          <span className="group-count">
            {needs > 0 && (
              <span className="machine-needs" data-testid="machine-needs">
                {needs} need{needs === 1 ? "s" : ""} you
              </span>
            )}
            {working > 0 && <span className="muted">{working} working</span>}
            <ChevronDown size={12} className={folded ? "chev closed" : "chev"} />
          </span>
        </button>
      </div>
      {!folded &&
        (!machine.online ? (
          <div className="machine-off small" data-testid="machine-offline">
            <span className="muted">{machine.runs ? "Lost touch" : "Not reachable"}{wait > 0 ? `, trying again in ${wait}s` : ""}.</span>
            <button type="button" className="ghost small" onClick={onRetry}>
              <RefreshCw size={12} aria-hidden /> Retry
            </button>
          </div>
        ) : machine.runs === null ? (
          <div className="machine-off small muted">Checking…</div>
        ) : list.length === 0 ? (
          <div className="machine-off small muted">No runs there yet.</div>
        ) : (
          <ul>
            {list.slice(0, SHOWN).map(({ run, group }) => (
              <li key={run.id}>
                <div className={`runrow ${group === "Needs you" ? "ask" : ""} ${group === "Settled" ? "settled" : ""}`} title={run.state_reason}>
                  <a className="runrow-link" href={`${machine.url.replace(/\/+$/, "")}/runs/${encodeURIComponent(run.id)}`} target="_blank" rel="noreferrer">
                    <span className={`run-ico ${group === "Working" ? "working" : ""} ${group === "Needs you" ? "ask" : ""}`}>
                      <Mark group={group} phase={run.phase} />
                    </span>
                    <span className="run-copy">
                      <span className="clip run-title">{run.prompt_head || run.id}</span>
                    </span>
                  </a>
                </div>
              </li>
            ))}
            {list.length > SHOWN && (
              <li className="machine-more small muted">
                <a href={machine.url} target="_blank" rel="noreferrer">
                  {list.length - SHOWN} more on {name}
                </a>
              </li>
            )}
          </ul>
        ))}
    </section>
  );
});

export function MachineRuns({ signedIn }: { signedIn: boolean }) {
  const { daemons } = usePrefs();
  const { machines, retry } = useMachines(daemons, signedIn);
  if (!signedIn || machines.length === 0) return null;
  return (
    <div className="machines" data-testid="machines">
      {machines.map((m) => (
        <Machine key={m.url} machine={m} onRetry={() => retry(m.url)} />
      ))}
    </div>
  );
}
