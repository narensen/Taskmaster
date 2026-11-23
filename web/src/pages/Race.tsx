// O5: a race side by side. Each provider ran the same request on its own branch; here are
// their checks, their diff, their cost and their preview. Merging one removes the others.

import { Check, ExternalLink, Loader2, Play, Trophy, X } from "lucide-react";
import { useEffect, useState } from "react";
import { api, type RaceRow, type RaceView } from "../api";
import { usd } from "../format";
import { toast } from "../overlay";
import { ProviderLogo } from "../providers/logos";
import { Link } from "../router";
import { Problem } from "../ui";

const WORKING = new Set(["planning", "building", "reviewing", "verifying"]);

function phaseWord(row: RaceRow): string {
  if (row.active && WORKING.has(row.phase)) return row.phase === "planning" ? "Planning" : row.phase === "building" ? "Building" : "Checking";
  if (row.phase === "awaiting_approval") return "Plan waits for you";
  if (row.phase === "done") return "Passed";
  if (row.phase === "failed") return "Failed";
  return row.phase;
}

function Preview({ run }: { run: string }) {
  const [url, setUrl] = useState<string | null>(null);
  const [state, setState] = useState<"idle" | "starting" | "failed">("idle");
  const start = async () => {
    setState("starting");
    try {
      let view = await api.startPreview(run);
      for (let i = 0; i < 120 && view.state === "starting"; i++) {
        await new Promise((r) => setTimeout(r, 500));
        view = await api.preview(run);
      }
      if (view.state === "running") {
        setUrl(api.previewProxyUrl(run));
        setState("idle");
      } else {
        setState("failed");
        toast("The preview did not start", view.reason ?? undefined);
      }
    } catch (err) {
      setState("failed");
      toast("The preview did not start", err instanceof Error ? err.message : String(err));
    }
  };
  if (url) return <iframe className="race-frame" title={`Preview of ${run}`} src={url} />;
  return (
    <button type="button" className="ghost small race-preview-btn" onClick={() => void start()} disabled={state === "starting"}>
      {state === "starting" ? <Loader2 size={13} className="spin-ico" aria-hidden /> : <Play size={13} aria-hidden />}
      {state === "starting" ? "Starting the preview…" : state === "failed" ? "Try the preview again" : "Preview"}
    </button>
  );
}

export function Race({ id }: { id: string }) {
  const [view, setView] = useState<RaceView | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const working = view?.runs.some((r) => r.active) ?? true;

  useEffect(() => {
    let stale = false;
    const load = () =>
      api.race(id).then(
        (v) => !stale && (setView(v), setError(null)),
        (err) => !stale && setError(err),
      );
    void load();
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible" && working) void load();
    }, 3000);
    return () => {
      stale = true;
      window.clearInterval(timer);
    };
  }, [id, working]);

  const pick = async (run: string) => {
    if (!window.confirm(`Merge ${run} into the project? The other runs' branches are removed.`)) return;
    setBusy(run);
    try {
      setView(await api.pickRace(id, run));
      toast("Merged", `${run} won the race; the others are archived.`, 5000);
    } catch (err) {
      toast("Not merged", err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(null);
    }
  };

  if (error && !view) return <div className="page"><Problem error={error} /></div>;
  if (!view) return <div className="page"><p className="muted">Loading the race…</p></div>;
  const winner = view.race.winner;
  // In cents, so two runs that cost the same are both the least.
  const cents = (usd: number) => Math.round(usd * 100);
  const done = view.runs.filter((r) => r.phase === "done");
  const cheapest = Math.min(...done.map((r) => cents(r.cost_usd)));
  const allSame = done.every((r) => cents(r.cost_usd) === cheapest);
  return (
    <div className="page race-page" data-testid="race">
      <header className="race-head">
        <h1>Race</h1>
        <p className="race-prompt">{view.race.prompt}</p>
        <p className="muted small">
          {winner
            ? `Decided: ${winner} was merged. The others were removed from git and archived.`
            : working
              ? "Each provider is building the same request on its own branch. None merges until you pick one."
              : "All have stopped. Compare them, open their previews, and merge the one you want."}
        </p>
      </header>
      <div className="race-grid" style={{ gridTemplateColumns: `repeat(${view.runs.length}, minmax(0, 1fr))` }}>
        {view.runs.map((row) => {
          const won = winner === row.run;
          const lost = !!winner && !won;
          return (
            <section key={row.run} className={`race-col ${won ? "won" : ""} ${lost ? "lost" : ""}`} data-testid="race-col" aria-label={`${row.provider} run`}>
              <div className="race-col-head">
                <ProviderLogo id={row.provider} size={16} />
                <strong>{row.provider}</strong>
                {row.model && <span className="muted small mono">{row.model}</span>}
                {won && <Trophy size={14} aria-label="merged" />}
              </div>
              <dl className="race-facts">
                <dt>State</dt>
                <dd data-testid="race-state">
                  {row.active ? <Loader2 size={12} className="spin-ico" aria-hidden /> : row.phase === "done" ? <Check size={12} aria-hidden /> : row.phase === "failed" ? <X size={12} aria-hidden /> : null}{" "}
                  {won ? "Merged" : lost ? "Archived" : phaseWord(row)}
                </dd>
                <dt>Pages</dt>
                <dd>
                  {row.passed} of {row.pages} passed{row.review_rounds ? `, ${row.review_rounds} review round${row.review_rounds === 1 ? "" : "s"}` : ""}
                </dd>
                <dt>Diff</dt>
                <dd className="mono">
                  {row.diff ? (
                    <>
                      {row.diff.files} file{row.diff.files === 1 ? "" : "s"} <span className="diff-add">+{row.diff.added}</span> <span className="diff-del">-{row.diff.removed}</span>
                    </>
                  ) : won ? (
                    "merged"
                  ) : (
                    "—"
                  )}
                </dd>
                <dt>Cost</dt>
                <dd className="mono">
                  {usd(row.cost_usd)}
                  {row.phase === "done" && cents(row.cost_usd) === cheapest && done.length > 1 && !allSame ? " · least" : ""}
                </dd>
                {row.error && (
                  <>
                    <dt>Error</dt>
                    <dd className="race-error">{row.error}</dd>
                  </>
                )}
              </dl>
              <div className="race-actions">
                <Link to={`/runs/${encodeURIComponent(row.run)}`} className="button ghost small">
                  <ExternalLink size={13} aria-hidden /> Open the run
                </Link>
                {!winner && (
                  <button
                    type="button"
                    className="primary small"
                    data-testid="race-pick"
                    disabled={working || row.phase !== "done" || busy !== null}
                    title={row.phase !== "done" ? "Only a run that passed can be merged" : working ? "Wait until every run has stopped" : undefined}
                    onClick={() => void pick(row.run)}
                  >
                    {busy === row.run ? "Merging…" : "Merge this one"}
                  </button>
                )}
              </div>
              {!lost && row.phase === "done" && <Preview run={row.run} />}
            </section>
          );
        })}
      </div>
    </div>
  );
}
