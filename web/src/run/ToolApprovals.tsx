// Per-tool approval cards: a risky tool call an agent is waiting on. Not the plan approval
// card (that one gates the build); these gate one call and time out into a denial.

import { useCallback, useEffect, useRef, useState } from "react";
import { ShieldAlert } from "lucide-react";
import { api, type AgentEntry, type ToolApproval } from "../api";
import { toast } from "../overlay";

/** Pending approvals for a run, kept in step with the run payload count and the live stream. */
export function useToolApprovals(run: string, pending: number | undefined, entries: AgentEntry[], onAnswered: () => void) {
  const [list, setList] = useState<ToolApproval[]>([]);
  const seq = useRef(0);
  const signal = entries.reduce((n, e) => n + (e.kind === "approval_requested" || e.kind === "approval_resolved" ? 1 : 0), 0);

  const load = useCallback(() => {
    const mine = ++seq.current;
    api
      .approvals(run)
      .then((r) => {
        if (mine === seq.current) setList(r.approvals);
      })
      .catch(() => {});
  }, [run]);

  useEffect(() => {
    load();
  }, [load, pending, signal]);

  // A request that times out resolves on the daemon; poll while any card is showing.
  useEffect(() => {
    if (!list.length) return;
    const timer = window.setInterval(load, 3000);
    return () => window.clearInterval(timer);
  }, [list.length, load]);

  const answer = useCallback(
    async (id: string, allow: boolean) => {
      try {
        await (allow ? api.approveTool(id) : api.denyTool(id));
      } catch (err) {
        toast(err instanceof Error ? err.message : String(err));
      }
      setList((l) => l.filter((a) => a.id !== id));
      load();
      onAnswered();
    },
    [load, onAnswered],
  );
  return { list, answer };
}

function useSeconds(): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, []);
  return now;
}

function agentLabel(a: unknown): string {
  if (typeof a === "string") return a;
  const t = a as { id?: string } | null;
  return t?.id ?? "agent";
}

export function ToolApprovals({ list, answer }: { list: ToolApproval[]; answer: (id: string, allow: boolean) => Promise<void> }) {
  const now = useSeconds();
  const [busy, setBusy] = useState<string | null>(null);
  return (
    <section className="tool-approvals" aria-label="Tool approvals">
      {list.map((a) => {
        const left = Math.max(0, Math.round((Date.parse(a.at) + a.timeout_secs * 1000 - now) / 1000));
        return (
          <div key={a.id} className={`tool-approval kind-${a.kind}`} data-testid="tool-approval">
            <div className="row between gap wrap">
              <div className="row gap tool-approval-head">
                <span className="approval-ico small-ico" aria-hidden>
                  <ShieldAlert size={14} />
                </span>
                <span className="strong">{a.tool}</span>
                <span className="badge ask">{a.kind}</span>
                <span className="muted small">{agentLabel(a.agent)}</span>
              </div>
              <span className="muted small nowrap">{Number.isFinite(left) ? `denies in ${left}s` : ""}</span>
            </div>
            {a.command && <pre className="tool-approval-cmd">{a.command}</pre>}
            {a.paths.length > 0 && (
              <ul className="tool-approval-paths small">
                {a.paths.map((p) => (
                  <li key={p} className="break">
                    {p}
                  </li>
                ))}
              </ul>
            )}
            <div className="tool-approval-reason">{a.reason}</div>
            <div className="row gap tool-approval-actions">
              <button
                className="small"
                data-testid="tool-deny"
                disabled={busy === a.id}
                onClick={() => {
                  setBusy(a.id);
                  void answer(a.id, false).finally(() => setBusy(null));
                }}
              >
                Deny
              </button>
              <button
                className="primary small"
                data-testid="tool-approve"
                disabled={busy === a.id}
                onClick={() => {
                  setBusy(a.id);
                  void answer(a.id, true).finally(() => setBusy(null));
                }}
              >
                Approve
              </button>
            </div>
            {Number.isFinite(left) && a.timeout_secs > 0 && (
              <div className="countdown" aria-hidden>
                <div className="countdown-fill" style={{ width: `${Math.min(100, (100 * left) / a.timeout_secs)}%` }} />
              </div>
            )}
          </div>
        );
      })}
    </section>
  );
}
