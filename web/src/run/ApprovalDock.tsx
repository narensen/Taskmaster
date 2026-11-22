// Ambient Floating Approval Dock: surfaces pending risky tool approvals across every
// Run Studio lens (Mission, Stream, Review, Telemetry) with keyboard shortcuts A / D.

import { useEffect, useState } from "react";
import { Check, ShieldAlert, X } from "lucide-react";
import type { ToolApproval } from "../api";

function typingTarget(target: EventTarget | null): boolean {
  const el = target instanceof Element ? target : null;
  return !!el?.closest("input, textarea, select, [contenteditable='true'], .xterm, [role='dialog']");
}

function agentLabel(a: unknown): string {
  if (typeof a === "string") return a;
  const obj = a as { id?: string } | null;
  return obj?.id ?? "agent";
}

export function ApprovalDock({
  list,
  onAnswer,
}: {
  list: ToolApproval[];
  onAnswer: (id: string, allow: boolean) => Promise<void>;
}) {
  const [busy, setBusy] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const first = list[0] ?? null;

  useEffect(() => {
    if (!first) return;
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, [first]);

  useEffect(() => {
    if (!first || busy) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey || typingTarget(e.target)) return;
      const k = e.key.toLowerCase();
      if (k === "a") {
        e.preventDefault();
        setBusy(first.id);
        void onAnswer(first.id, true).finally(() => setBusy(null));
      } else if (k === "d") {
        e.preventDefault();
        setBusy(first.id);
        void onAnswer(first.id, false).finally(() => setBusy(null));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [first, busy, onAnswer]);

  if (!first) return null;

  const left = Math.max(0, Math.round((Date.parse(first.at) + first.timeout_secs * 1000 - now) / 1000));
  const pct = first.timeout_secs > 0 ? Math.min(100, Math.max(0, (100 * left) / first.timeout_secs)) : 100;

  return (
    <aside className="approval-dock" role="region" aria-label="Pending tool approval" data-testid="approval-dock">
      <div className="approval-dock-head">
        <span className="approval-dock-ico" aria-hidden>
          <ShieldAlert size={15} />
        </span>
        <strong className="approval-dock-title">{first.tool}</strong>
        <span className={`studio-pill kind-${first.kind}`}>{first.kind}</span>
        <span className="muted small mono">{agentLabel(first.agent)}</span>
        {list.length > 1 && <span className="approval-dock-queue">+{list.length - 1} queued</span>}
        <span className="approval-dock-timer mono">
          {Number.isFinite(left) ? `${left}s` : ""}
        </span>
      </div>
      <div className="approval-dock-body">
        {first.command && <code className="approval-dock-cmd mono">{first.command}</code>}
        {first.paths.length > 0 && (
          <span className="approval-dock-paths mono">{first.paths.slice(0, 3).join(", ")}</span>
        )}
        <span className="approval-dock-reason">{first.reason}</span>
      </div>
      <div className="approval-dock-actions">
        <button
          type="button"
          className="ghost small approval-dock-deny"
          disabled={busy === first.id}
          onClick={() => {
            setBusy(first.id);
            void onAnswer(first.id, false).finally(() => setBusy(null));
          }}
        >
          <X size={13} aria-hidden /> Deny <kbd>D</kbd>
        </button>
        <button
          type="button"
          className="primary small approval-dock-allow"
          disabled={busy === first.id}
          onClick={() => {
            setBusy(first.id);
            void onAnswer(first.id, true).finally(() => setBusy(null));
          }}
        >
          <Check size={13} aria-hidden /> Approve <kbd>A</kbd>
        </button>
      </div>
      <div className="approval-dock-bar" aria-hidden>
        <div className="approval-dock-bar-fill" style={{ width: `${pct}%` }} />
      </div>
    </aside>
  );
}
