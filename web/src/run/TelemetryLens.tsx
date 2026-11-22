// Lens 4 — Telemetry, Waterfall & Checkpoint Time-Travel:
// 1. Prompt Cache & Cost Economics stacked bar (Cache Read, Cache Write, Fresh Input, Output)
//    plus USD budget meter and per-role cost breakdown.
// 2. Parallel Execution Gantt Waterfall computed from ledger timestamps and call durations.
// 3. Checkpoint Time-Travel scrubber (`GET /api/runs/{id}/checkpoints`) with 1-click Fork Run.
// 4. Secret-redacted `.tmrun.json` Flight Recorder Bundle download (`GET /api/runs/{id}/bundle`).

import { useEffect, useMemo, useState } from "react";
import {
  Activity,
  Coins,
  Download,
  GitCommitHorizontal,
  GitFork,
  Layers,
  ShieldCheck,
  Timer,
  Zap,
} from "lucide-react";
import { api, type Checkpoint, type LedgerEntry, type RunDetail, type Totals } from "../api";
import { usd } from "../format";
import { toast } from "../overlay";
import { navigate } from "../router";

function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

function fmtSecs(s: number): string {
  if (s >= 60) {
    const m = Math.floor(s / 60);
    const rem = Math.round(s % 60);
    return `${m}m ${rem}s`;
  }
  return `${s.toFixed(1)}s`;
}

function laneKey(entry: LedgerEntry): string {
  if (entry.agent) return entry.agent;
  if (entry.page) return `page:${entry.page}`;
  return entry.role;
}

function roleColorClass(role: string): string {
  if (role === "planner" || role === "master" || role === "arbiter") return "bar-violet";
  if (role === "worker" || role === "mid") return "bar-blue";
  if (role === "reviewer" || role === "security") return "bar-cyan";
  if (role === "verifier" || role === "checker") return "bar-emerald";
  return "bar-amber";
}

export function TelemetryLens({
  id,
  detail,
  ledger,
}: {
  id: string;
  detail: RunDetail;
  ledger: { totals: Totals; entries: LedgerEntry[] } | null;
}) {
  const [checkpoints, setCheckpoints] = useState<Checkpoint[]>([]);
  const [selectedCp, setSelectedCp] = useState<Checkpoint | null>(null);
  const [forkNote, setForkNote] = useState("");
  const [forkBudget, setForkBudget] = useState("");
  const [forking, setForking] = useState(false);
  const [exporting, setExporting] = useState(false);

  useEffect(() => {
    let alive = true;
    api
      .checkpoints(id)
      .then((list) => {
        if (!alive) return;
        setCheckpoints(list);
      })
      .catch(() => {
        if (alive) setCheckpoints([]);
      });
    return () => {
      alive = false;
    };
  }, [id, detail.phase, detail.pages.length]);

  const totals = ledger?.totals ?? detail.spent;
  const entries = ledger?.entries ?? [];

  // Cache & Token Economics
  const econ = useMemo(() => {
    const cacheRead = totals.cache_read || 0;
    const cacheWrite = totals.cache_write || 0;
    const freshIn = totals.input || 0;
    const out = totals.output || 0;
    const totalIn = cacheRead + cacheWrite + freshIn;
    const totalAll = totalIn + out;
    const hitRate = totalIn > 0 ? (cacheRead / totalIn) * 100 : 0;
    // Conservative estimate: cached reads cost ~10% of fresh input, saving ~90% of that share.
    const avgInputPrice = totalAll > 0 && totals.cost_usd > 0 ? totals.cost_usd / totalAll : 0;
    const estSaved = cacheRead * avgInputPrice * 0.85;
    return {
      cacheRead,
      cacheWrite,
      freshIn,
      out,
      totalIn,
      totalAll,
      hitRate,
      estSaved,
      pCacheRead: totalAll > 0 ? (cacheRead / totalAll) * 100 : 0,
      pCacheWrite: totalAll > 0 ? (cacheWrite / totalAll) * 100 : 0,
      pFreshIn: totalAll > 0 ? (freshIn / totalAll) * 100 : 0,
      pOut: totalAll > 0 ? (out / totalAll) * 100 : 0,
    };
  }, [totals]);

  // Parallel Gantt Waterfall spans
  const waterfall = useMemo(() => {
    if (!entries.length) return { lanes: [] as { lane: string; items: { entry: LedgerEntry; leftPct: number; widthPct: number }[] }[], spanSecs: 0 };
    const timed = entries.map((e, idx) => {
      const endMs = Date.parse(e.at);
      const durMs = Math.max(200, (e.seconds || 0.5) * 1000);
      const fallbackStart = idx * 1500;
      const end = Number.isFinite(endMs) ? endMs : fallbackStart + durMs;
      const start = end - durMs;
      return { entry: e, start, end };
    });
    const minT = Math.min(...timed.map((t) => t.start));
    const maxT = Math.max(...timed.map((t) => t.end));
    const span = Math.max(1000, maxT - minT);

    const byLane = new Map<string, { entry: LedgerEntry; leftPct: number; widthPct: number }[]>();
    for (const item of timed) {
      const lane = laneKey(item.entry);
      const arr = byLane.get(lane) ?? [];
      arr.push({
        entry: item.entry,
        leftPct: ((item.start - minT) / span) * 100,
        widthPct: Math.max(2.5, ((item.end - item.start) / span) * 100),
      });
      byLane.set(lane, arr);
    }
    const lanes = [...byLane.entries()].map(([lane, items]) => ({ lane, items }));
    return { lanes, spanSecs: span / 1000 };
  }, [entries]);

  const exportBundle = async () => {
    setExporting(true);
    try {
      const bundle = await api.bundle(id);
      const blob = new Blob([JSON.stringify(bundle, null, 2)], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `${id}.tmrun.json`;
      document.body.appendChild(a);
      a.click();
      a.remove();
      URL.revokeObjectURL(url);
      toast("Flight Recorder exported", `Saved secret-redacted ${id}.tmrun.json`, 3000);
    } catch (err) {
      toast("Export failed", err instanceof Error ? err.message : String(err));
    } finally {
      setExporting(false);
    }
  };

  const doFork = async () => {
    setForking(true);
    try {
      const cap = forkBudget.trim() ? Number(forkBudget.trim()) : undefined;
      const res = await api.forkRun(id, {
        yes: false,
        max_cost_usd: cap && Number.isFinite(cap) && cap > 0 ? cap : undefined,
        note: forkNote.trim() || undefined,
      });
      toast("Forked run created", `Switched to ${res.id}`, 2500);
      navigate(`/runs/${encodeURIComponent(res.id)}`);
    } catch (err) {
      toast("Fork failed", err instanceof Error ? err.message : String(err));
    } finally {
      setForking(false);
    }
  };

  const budgetPct =
    detail.budget_usd && detail.budget_usd > 0 ? Math.min(100, (totals.cost_usd / detail.budget_usd) * 100) : null;

  return (
    <div className="telemetry-lens" data-testid="telemetry-lens">
      {/* Top KPI Row + Flight Recorder Export */}
      <div className="telemetry-top">
        <div className="telemetry-kpi-grid">
          <div className="kpi-card tone-emerald">
            <div className="kpi-label">
              <Zap size={13} aria-hidden /> Cache Hit Rate
            </div>
            <div className="kpi-value mono">{econ.hitRate.toFixed(1)}%</div>
            <div className="kpi-sub muted">{fmtTokens(econ.cacheRead)} cached tokens</div>
          </div>

          <div className="kpi-card tone-indigo">
            <div className="kpi-label">
              <Coins size={13} aria-hidden /> Total Spend
            </div>
            <div className="kpi-value mono">{usd(totals.cost_usd)}</div>
            <div className="kpi-sub muted">
              {totals.calls} agent call{totals.calls === 1 ? "" : "s"}
              {econ.estSaved >= 0.005 ? ` · ~${usd(econ.estSaved)} saved` : ""}
            </div>
          </div>

          <div className="kpi-card tone-cyan">
            <div className="kpi-label">
              <Timer size={13} aria-hidden /> Agent Time
            </div>
            <div className="kpi-value mono">{fmtSecs(totals.seconds)}</div>
            <div className="kpi-sub muted">
              {waterfall.spanSecs > 0 ? `Wall span ${fmtSecs(waterfall.spanSecs)}` : "Cumulative compute"}
            </div>
          </div>

          <div className="kpi-card tone-violet">
            <div className="kpi-label">
              <Layers size={13} aria-hidden /> Total Tokens
            </div>
            <div className="kpi-value mono">{fmtTokens(econ.totalAll)}</div>
            <div className="kpi-sub muted">
              {fmtTokens(econ.freshIn)} fresh · {fmtTokens(econ.out)} out
            </div>
          </div>
        </div>

        <div className="flight-recorder-card">
          <div className="row gap">
            <ShieldCheck size={16} className="tone-emerald-ico" aria-hidden />
            <div>
              <strong>Flight Recorder Bundle</strong>
              <p className="muted small">
                Export plan, events, ledger, transcript & checkpoints as portable <code>.tmrun.json</code> with Policy secret redaction.
              </p>
            </div>
          </div>
          <button
            type="button"
            className="primary small"
            disabled={exporting}
            onClick={() => void exportBundle()}
            data-testid="export-bundle-btn"
          >
            <Download size={13} aria-hidden /> {exporting ? "Exporting…" : "Export .tmrun.json"}
          </button>
        </div>
      </div>

      {/* Prompt Cache Economics Stacked Bar + Budget Meter */}
      <section className="studio-card">
        <div className="row between">
          <h3>
            <Activity size={14} aria-hidden /> Token & Prompt Cache Economics
          </h3>
          {detail.budget_usd && detail.budget_usd > 0 && (
            <span className="mono small">
              Budget: <strong>{usd(totals.cost_usd)}</strong> / {usd(detail.budget_usd)} ({budgetPct?.toFixed(0)}%)
            </span>
          )}
        </div>

        <div className="stacked-bar" aria-label="Token breakdown">
          {econ.pCacheRead > 0 && (
            <div className="seg seg-cache-read" style={{ width: `${econ.pCacheRead}%` }} title={`Cache read: ${fmtTokens(econ.cacheRead)}`} />
          )}
          {econ.pCacheWrite > 0 && (
            <div className="seg seg-cache-write" style={{ width: `${econ.pCacheWrite}%` }} title={`Cache write: ${fmtTokens(econ.cacheWrite)}`} />
          )}
          {econ.pFreshIn > 0 && (
            <div className="seg seg-fresh-in" style={{ width: `${econ.pFreshIn}%` }} title={`Fresh input: ${fmtTokens(econ.freshIn)}`} />
          )}
          {econ.pOut > 0 && (
            <div className="seg seg-output" style={{ width: `${econ.pOut}%` }} title={`Output: ${fmtTokens(econ.out)}`} />
          )}
        </div>

        <div className="stacked-legend mono small">
          <span><i className="dot seg-cache-read" /> Cache Read {fmtTokens(econ.cacheRead)}</span>
          <span><i className="dot seg-cache-write" /> Cache Write {fmtTokens(econ.cacheWrite)}</span>
          <span><i className="dot seg-fresh-in" /> Fresh Input {fmtTokens(econ.freshIn)}</span>
          <span><i className="dot seg-output" /> Output {fmtTokens(econ.out)}</span>
        </div>

        {budgetPct !== null && (
          <div className="budget-meter">
            <div
              className={`budget-meter-fill ${budgetPct >= 90 ? "danger" : budgetPct >= 70 ? "warn" : "ok"}`}
              style={{ width: `${budgetPct}%` }}
            />
          </div>
        )}
      </section>

      {/* Parallel Execution Gantt Waterfall */}
      <section className="studio-card">
        <div className="row between">
          <h3>Parallel Execution Waterfall</h3>
          <span className="muted small mono">
            {waterfall.lanes.length} lane{waterfall.lanes.length === 1 ? "" : "s"} · {entries.length} call{entries.length === 1 ? "" : "s"}
          </span>
        </div>

        {waterfall.lanes.length === 0 ? (
          <p className="muted small">Agent calls will appear here as a parallel Gantt timeline once execution begins.</p>
        ) : (
          <div className="gantt-chart" role="region" aria-label="Execution waterfall">
            {waterfall.lanes.map(({ lane, items }) => (
              <div key={lane} className="gantt-row">
                <div className="gantt-lane mono clip" title={lane}>
                  {lane}
                </div>
                <div className="gantt-track">
                  {items.map((item, idx) => (
                    <div
                      key={idx}
                      className={`gantt-bar ${roleColorClass(item.entry.role)} ${item.entry.ok ? "" : "bar-failed"}`}
                      style={{ left: `${item.leftPct}%`, width: `${item.widthPct}%` }}
                      title={`${item.entry.role} · ${item.entry.model ?? item.entry.harness} · ${fmtSecs(item.entry.seconds)} · ${item.entry.cost_usd != null ? usd(item.entry.cost_usd) : "unpriced"}`}
                    >
                      <span className="gantt-bar-label mono clip">
                        {fmtSecs(item.entry.seconds)}
                        {item.entry.cost_usd != null && item.entry.cost_usd > 0 ? ` · ${usd(item.entry.cost_usd)}` : ""}
                      </span>
                    </div>
                  ))}
                </div>
              </div>
            ))}
          </div>
        )}
      </section>

      {/* Checkpoint Time-Travel & Run Forking */}
      <section className="studio-card">
        <div className="row between">
          <h3>
            <GitCommitHorizontal size={14} aria-hidden /> Checkpoint Time-Travel & Forking
          </h3>
          <span className="muted small mono">{checkpoints.length} milestone{checkpoints.length === 1 ? "" : "s"}</span>
        </div>

        {checkpoints.length === 0 ? (
          <p className="muted small">Committed unit milestones and git checkpoints will appear here as pages land.</p>
        ) : (
          <div className="checkpoint-strip">
            {checkpoints.map((cp) => {
              const active = selectedCp?.sha === cp.sha;
              return (
                <button
                  key={cp.sha}
                  type="button"
                  className={`checkpoint-node ${active ? "selected" : ""}`}
                  onClick={() => setSelectedCp(active ? null : cp)}
                >
                  <div className="row between gap">
                    <code className="mono">{cp.short_sha}</code>
                    <span className="studio-pill tone-cyan">{cp.page ?? cp.kind}</span>
                  </div>
                  <div className="checkpoint-label clip" title={cp.label}>
                    {cp.label}
                  </div>
                  {cp.model && <div className="muted small mono clip">{cp.model}</div>}
                </button>
              );
            })}
          </div>
        )}

        <div className="fork-panel">
          <div className="fork-panel-inputs">
            <input
              type="text"
              value={forkNote}
              onChange={(e) => setForkNote(e.target.value)}
              placeholder={
                selectedCp
                  ? `Fork guidance from ${selectedCp.short_sha} (optional)…`
                  : "Optional note or experiment direction for the forked run…"
              }
            />
            <input
              type="number"
              min="0.1"
              step="0.5"
              className="fork-budget-input mono"
              value={forkBudget}
              onChange={(e) => setForkBudget(e.target.value)}
              placeholder="USD cap"
              title="Optional USD budget cap for the forked run"
            />
            <button
              type="button"
              className="primary small"
              disabled={forking}
              onClick={() => void doFork()}
              data-testid="fork-run-btn"
            >
              <GitFork size={13} aria-hidden /> {forking ? "Forking…" : "Fork Run"}
            </button>
          </div>
        </div>
      </section>
    </div>
  );
}
