// The run's token total, in the title bar, with the per-role split on hover.
// The approval mode sits beside it: one word, changed in place.

import { useState } from "react";
import { api, type ApprovalMode, type Spend } from "../api";
import { tokens } from "../format";
import { keys, queryClient, useSettings } from "../query";
import { useRunChrome } from "../shell/runChrome";
import { TokenLedger } from "./Tokens";

const MODES: ApprovalMode[] = ["risky", "always", "never"];

function roleName(key: string): string {
  if (key === "master") return "Master";
  if (key === "worker") return "Workers";
  if (key === "mid") return "Mid";
  if (key === "reviewer") return "Reviewer";
  if (key === "verifier") return "Verifier";
  return key;
}

function RoleLine({ name, spend }: { name: string; spend: Spend }) {
  const n = spend.input + spend.output + spend.cache_read + spend.cache_write;
  return (
    <div className="tb-meter-row" data-testid="agent-split">
      <span>{name}</span>
      <span className="mono">{tokens(n || spend.tokens)}</span>
      <span className="muted">
        {tokens(spend.output)} out · {tokens(spend.input)} in
        {spend.cache_read ? ` · ${tokens(spend.cache_read)} cached` : ""}
      </span>
    </div>
  );
}

export function RunBar() {
  const chrome = useRunChrome();
  const settings = useSettings();
  const mode = settings.data?.approvals?.mode ?? "risky";
  const [pending, setPending] = useState<ApprovalMode | null>(null);
  const shown = pending ?? mode;
  const totals = chrome?.totals;
  const total = totals ? totals.input + totals.output + totals.cache_read + totals.cache_write : 0;
  const roles = Object.entries(totals?.by_role ?? {}).filter(([, s]) => s.calls > 0 || s.tokens > 0);

  const setMode = (next: ApprovalMode) => {
    setPending(next);
    api.setApprovals({ approvals: next }).then(
      () => {
        setPending(null);
        void queryClient.invalidateQueries({ queryKey: keys.settings });
      },
      () => setPending(null),
    );
  };

  return (
    <div className="tb-run">
      <div className="tb-meter" data-testid="token-counter">
        <button type="button" className="tb-meter-btn" aria-describedby="token-breakdown">
          {totals ? tokens(total) : "–"}
        </button>
        <div className="tb-meter-pop" id="token-breakdown" role="tooltip">
          {roles.length === 0 && <div className="muted small">No tokens yet.</div>}
          {roles.map(([role, spend]) => (
            <RoleLine key={role} name={roleName(role)} spend={spend} />
          ))}
          {totals && totals.calls > 0 && <TokenLedger totals={totals} open />}
        </div>
      </div>
      <select className="tb-mode" aria-label="Approval mode" value={shown} onChange={(e) => setMode(e.target.value as ApprovalMode)}>
        {MODES.map((m) => (
          <option key={m} value={m}>
            {m}
          </option>
        ))}
      </select>
    </div>
  );
}
