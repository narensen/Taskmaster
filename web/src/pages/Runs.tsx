import { api, type RunSummary } from "../api";
import { phaseLabel, pageTone, phaseTone, tokens, usd } from "../format";
import { Link } from "../router";
import { Badge, Dot, Empty, Problem, Spinner, useLoad } from "../ui";

export function Runs() {
  const runs = useLoad(api.runs, [], 4000);
  const list = runs.data ?? [];
  const waiting = list.filter((run) => run.phase === "awaiting_approval" && !run.active);
  const others = list.filter((run) => !waiting.includes(run));

  return (
    <div className="page">
      <header className="page-head">
        <h1>Runs</h1>
        <Link to="/runs/new" className="button primary">
          New run
        </Link>
      </header>

      <Problem error={runs.error} onRetry={runs.reload} />
      {runs.loading && !runs.data && <Spinner label="Loading runs" />}
      {runs.data && list.length === 0 && (
        <Empty title="No runs yet">Describe what to build. The master plans it, and nothing is built until you approve the plan.</Empty>
      )}

      {waiting.length > 0 && (
        <section>
          <h2 className="section-title">Waiting for you</h2>
          <div className="cards">
            {waiting.map((run) => (
              <RunCard key={run.id} run={run} />
            ))}
          </div>
        </section>
      )}
      {others.length > 0 && (
        <section>
          {waiting.length > 0 && <h2 className="section-title">Everything else</h2>}
          <div className="cards">
            {others.map((run) => (
              <RunCard key={run.id} run={run} />
            ))}
          </div>
        </section>
      )}
    </div>
  );
}

function RunCard({ run }: { run: RunSummary }) {
  const passed = run.pages.filter((page) => page.state === "passed").length;
  return (
    <Link to={`/runs/${encodeURIComponent(run.id)}`} className="card run-card">
      <div className="row between">
        <strong className="mono">{run.id}</strong>
        <Badge tone={run.cancelled ? "quiet" : phaseTone(run.phase)}>
          {run.active && run.phase !== "done" && run.phase !== "failed" && <span className="pulse" aria-hidden />}
          {phaseLabel(run)}
        </Badge>
      </div>
      {run.pages.length > 0 && (
        <div className="page-dots" aria-label={`${passed} of ${run.pages.length} pages passed`}>
          {run.pages.map((page) => (
            <span key={page.id} title={`${page.id}: ${page.state}`}>
              <Dot tone={pageTone(page.state)} />
            </span>
          ))}
          <span className="muted small">
            {passed}/{run.pages.length} pages
          </span>
        </div>
      )}
      <div className="row muted small gap">
        <span>{usd(run.spent.cost_usd)}</span>
        <span>{tokens(run.spent.fresh)} tokens</span>
        <span>{run.spent.calls} calls</span>
        {run.branch && <span className="mono">{run.branch}</span>}
      </div>
      {run.error && <p className="error-line">{run.error}</p>}
    </Link>
  );
}
