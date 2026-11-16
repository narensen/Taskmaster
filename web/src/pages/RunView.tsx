import { useEffect, useRef, useState } from "react";
import { api, follow, type Attempt, type ChangeEntry, type PagePlan, type RunDetail, type RunEvent, type Spend } from "../api";
import { CHANGE, phaseLabel, ago, describe, pageTone, phaseTone, seconds, tokens, usd } from "../format";
import { Link } from "../router";
import { Badge, Code, Dot, Empty, Problem, Spinner, Stat, Tabs, useAction, useLoad } from "../ui";

type Tab = "pages" | "activity" | "cost" | "changes";

export function RunView({ id }: { id: string }) {
  const run = useLoad(() => api.run(id), [id], 3000);
  const [tab, setTab] = useState<Tab>(() => {
    const asked = new URLSearchParams(window.location.search).get("tab");
    return asked === "activity" || asked === "cost" || asked === "changes" ? asked : "pages";
  });
  const { events, live } = useEvents(id, run.data?.active ?? false, run.reload);
  const action = useAction();

  if (run.loading && !run.data) return <div className="page"><Spinner label="Loading the run" /></div>;
  if (!run.data) return <div className="page"><Problem error={run.error} onRetry={run.reload} /></div>;
  const r = run.data;

  const act = (name: string, call: () => Promise<unknown>) =>
    void action.run(name, async () => {
      await call();
      await run.reload();
    });

  return (
    <div className="page">
      <nav className="crumbs">
        <Link to="/">Runs</Link> / <span className="mono">{r.id}</span>
      </nav>
      <header className="run-head">
        <div className="row between wrap gap">
          <h1 className="mono">{r.id}</h1>
          <Badge tone={r.cancelled ? "quiet" : phaseTone(r.phase)}>
            {r.active && <span className="pulse" aria-hidden />}
            {phaseLabel(r)}
          </Badge>
        </div>
        {r.prompt && <p className="prompt">{r.prompt}</p>}
        <div className="row wrap gap muted small">
          {r.branch && (
            <span>
              branch <span className="mono">{r.branch}</span>
              {r.merged === true && " · merged into the project"}
              {r.merged === false && r.phase === "done" && " · not merged, it is on its branch"}
            </span>
          )}
          {r.review_rounds > 0 && <span>{r.review_rounds} review round{r.review_rounds === 1 ? "" : "s"}</span>}
          {r.amendments > 0 && <span>{r.amendments} Dictionary amendment{r.amendments === 1 ? "" : "s"}</span>}
          {live && <span className="live">live</span>}
        </div>
        {r.error && <p className="error-line">{r.error}</p>}
      </header>

      <Actions run={r} busy={action.busy} act={act} />
      <Problem error={action.error} />

      {r.spent.calls > 0 && <div className="stats">
        <Stat label="Spent" value={usd(r.spent.cost_usd)} note={r.spent.unpriced_calls ? `${r.spent.unpriced_calls} calls had no price` : undefined} />
        <Stat label="Fresh tokens" value={tokens(r.spent.fresh)} note={r.estimate ? `plan said ${tokens(r.estimate.tokens)}` : undefined} />
        <Stat label="Read from cache" value={tokens(r.spent.cache_read)} />
        <Stat label="Agent calls" value={r.spent.calls} note={seconds(r.spent.seconds)} />
      </div>}

      <Tabs<Tab>
        value={tab}
        onChange={setTab}
        tabs={[
          { id: "pages", label: "Pages", count: r.pages.length },
          { id: "activity", label: "Activity", count: events.length },
          { id: "cost", label: "Cost" },
          { id: "changes", label: "Changes" },
        ]}
      />
      {tab === "pages" && <Pages run={r} />}
      {tab === "activity" && <Activity events={events} />}
      {tab === "cost" && <Cost id={id} calls={r.spent.calls} />}
      {tab === "changes" && <Changes id={id} calls={r.spent.calls} />}
    </div>
  );
}

/** The run's log so far, then the rest as it happens. */
function useEvents(id: string, active: boolean, changed: () => void) {
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [live, setLive] = useState(false);
  const seen = useRef(0);
  const [loaded, setLoaded] = useState(false);
  const refresh = useRef(changed);
  refresh.current = changed;

  useEffect(() => {
    seen.current = 0;
    setEvents([]);
    setLoaded(false);
    let gone = false;
    api.events(id).then(
      (log) => {
        if (gone) return;
        seen.current = log.next;
        setEvents(log.events);
        setLoaded(true);
      },
      () => setLoaded(true),
    );
    return () => {
      gone = true;
    };
  }, [id]);

  useEffect(() => {
    if (!loaded || !active) return;
    let timer: number | undefined;
    const soon = () => {
      window.clearTimeout(timer);
      timer = window.setTimeout(() => refresh.current(), 400);
    };
    setLive(true);
    const stop = follow(id, seen.current, {
      event: (event, index) => {
        if (index && index <= seen.current) return;
        seen.current = index || seen.current + 1;
        setEvents((list) => [...list, event]);
        soon();
      },
      idle: () => {
        setLive(false);
        soon();
      },
      lost: () => setLive(false),
    });
    return () => {
      stop();
      window.clearTimeout(timer);
      setLive(false);
    };
  }, [id, active, loaded]);

  return { events, live };
}

function Actions({
  run,
  busy,
  act,
}: {
  run: RunDetail;
  busy: string | null;
  act: (name: string, call: () => Promise<unknown>) => void;
}) {
  const [rejecting, setRejecting] = useState(false);
  const [prompt, setPrompt] = useState(run.prompt ?? "");
  const over = run.phase === "done" || run.phase === "failed";
  const waiting = run.phase === "awaiting_approval" && !run.active;
  const stopped = !run.active && !over && !waiting;

  if (over && !run.active) return null;

  return (
    <div className="actions card">
      {waiting && (
        <>
          <p>
            <strong>The plan is ready.</strong> Nothing is built until you approve it.
            {run.estimate && ` It should take about ${tokens(run.estimate.tokens)} fresh tokens${run.estimate.usd ? ` and ${usd(run.estimate.usd)}` : ""}.`}
          </p>
          {rejecting ? (
            <div className="stack">
              <textarea rows={4} value={prompt} onChange={(e) => setPrompt(e.target.value)} placeholder="What to plan instead" />
              <div className="row gap wrap">
                <button className="primary" disabled={!!busy || !prompt.trim()} onClick={() => act("reject", () => api.reject(run.id, prompt))}>
                  Plan again with this
                </button>
                <button className="ghost" disabled={!!busy} onClick={() => act("reject", () => api.reject(run.id))}>
                  Drop the plan and wait
                </button>
                <button className="link" onClick={() => setRejecting(false)}>
                  Keep the plan
                </button>
              </div>
            </div>
          ) : (
            <div className="row gap wrap">
              <button className="primary big" disabled={!!busy} onClick={() => act("approve", () => api.approve(run.id))}>
                {busy === "approve" ? "Approving…" : "Approve and build"}
              </button>
              <button className="ghost" disabled={!!busy} onClick={() => setRejecting(true)}>
                Change the plan
              </button>
              <button className="ghost danger" disabled={!!busy} onClick={() => act("cancel", () => api.cancel(run.id))}>
                Cancel
              </button>
            </div>
          )}
        </>
      )}
      {run.active && (
        <div className="row between wrap gap">
          <span className="muted">{run.phase === "planning" ? "The master is planning." : "Agents are working."}</span>
          <button className="ghost danger" disabled={!!busy} onClick={() => act("cancel", () => api.cancel(run.id))}>
            {busy === "cancel" ? "Cancelling…" : "Cancel"}
          </button>
        </div>
      )}
      {stopped && (
        <div className="row between wrap gap">
          <span className="muted">This run stopped part way, perhaps because the daemon restarted.</span>
          <div className="row gap">
            <button className="primary" disabled={!!busy} onClick={() => act("resume", () => api.resume(run.id))}>
              Resume
            </button>
            <button className="ghost danger" disabled={!!busy} onClick={() => act("cancel", () => api.cancel(run.id))}>
              Cancel
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

function Pages({ run }: { run: RunDetail }) {
  const plans = new Map((run.plan?.pages ?? []).map((page) => [page.id, page]));
  if (!run.plan) return <Empty title="No plan yet">{run.active ? "The master is planning." : "The run has no plan."}</Empty>;
  const states = new Map(run.pages.map((page) => [page.id, page]));
  const shots = useLoad(() => api.screens(run.id), [run.id, run.spent.calls]);
  return (
    <div className="stack">
      <div className="list">
        {run.plan.pages.map((plan) => (
          <PageRow
            key={plan.id}
            runId={run.id}
            plan={plan}
            state={states.get(plan.id)}
            shot={(shots.data ?? []).includes(plan.id)}
            version={run.spent.calls}
          />
        ))}
        {run.pages
          .filter((page) => !plans.has(page.id))
          .map((page) => (
            <div key={page.id} className="item">
              <span className="mono">{page.id}</span> <Badge tone={pageTone(page.state)}>{page.state}</Badge>
            </div>
          ))}
      </div>
      {run.plan.contracts.length > 0 && (
        <section>
          <h2 className="section-title">Contracts</h2>
          <p className="muted small">Shared files the master writes before any page is built. Every page imports them; the compiler holds pages to them.</p>
          <div className="list">
            {run.plan.contracts.map((file) => (
              <details key={file.path} className="item">
                <summary className="mono">{file.path}</summary>
                <Code>{file.content}</Code>
              </details>
            ))}
          </div>
        </section>
      )}
    </div>
  );
}

function PageRow({
  runId,
  plan,
  state,
  shot,
  version,
}: {
  runId: string;
  plan: PagePlan;
  state?: RunDetail["pages"][number];
  shot: boolean;
  version: number;
}) {
  const [open, setOpen] = useState(false);
  return (
    <details className="item page-row" onToggle={(e) => setOpen((e.target as HTMLDetailsElement).open)}>
      <summary>
        <div className="row between gap">
          <div className="row gap">
            {state && <Dot tone={pageTone(state.state)} />}
            <strong>{plan.id}</strong>
            <span className="mono muted">{plan.route}</span>
          </div>
          <div className="row gap small keep">
            {state && state.tier !== "worker" && <Badge tone="ask">{state.tier}</Badge>}
            {state && state.repairs > 0 && <span className="muted">{state.repairs} repair{state.repairs === 1 ? "" : "s"}</span>}
            {state && <Badge tone={pageTone(state.state)}>{state.state}</Badge>}
          </div>
        </div>
      </summary>
      <div className="stack small">
        <p>{plan.brief}</p>
        <p className="muted">
          May change <span className="mono">{plan.owned_glob}</span>
        </p>
        {plan.cites.length > 0 && (
          <p className="muted">
            Cites{" "}
            {plan.cites.map((key, i) => (
              <span key={key}>
                {i > 0 && ", "}
                <Link to={`/dictionary?q=${encodeURIComponent(key)}`} className="mono">
                  {key}
                </Link>
              </span>
            ))}
          </p>
        )}
        {open && shot && <Screenshot runId={runId} page={plan.id} version={version} />}
        {open && <Attempts runId={runId} page={plan.id} version={version} />}
      </div>
    </details>
  );
}

function Screenshot({ runId, page, version }: { runId: string; page: string; version: number }) {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    let made: string | null = null;
    api.screen(runId, page).then(
      (value) => {
        made = value;
        setUrl(value);
      },
      () => setUrl(null),
    );
    return () => {
      if (made) URL.revokeObjectURL(made);
    };
  }, [runId, page, version]);
  if (!url) return null;
  return (
    <figure className="shot">
      <img src={url} alt={`${page} in the browser`} loading="lazy" />
      <figcaption className="muted small">As the browser check saw it</figcaption>
    </figure>
  );
}

const OUTCOME: Record<Attempt["outcome"], string> = { merged: "kept", discarded: "thrown away", flagged: "objected" };

function Attempts({ runId, page, version }: { runId: string; page: string; version: number }) {
  const attempts = useLoad(() => api.attempts(runId, page), [runId, page, version]);
  const list = attempts.data ?? [];
  if (list.length === 0) return null;
  return (
    <div className="stack">
      <span className="section-title">Attempts</span>
      <ol className="attempts">
        {list.map((attempt, i) => (
          <li key={i}>
            <div className="row between gap wrap">
              <span>
                <span className="mono">{attempt.model}</span> <span className="muted">({attempt.tier})</span>
              </span>
              <Badge tone={attempt.outcome === "merged" ? "good" : attempt.outcome === "flagged" ? "ask" : "bad"}>
                {OUTCOME[attempt.outcome] ?? attempt.outcome}
              </Badge>
            </div>
            {attempt.fixing && <p className="muted small">Fixing: {attempt.fixing.split("\n")[0]}</p>}
            {attempt.failure && <p className="error-line">{attempt.failure.split("\n")[0]}</p>}
            {attempt.said && <p className="small">“{attempt.said.length > 280 ? `${attempt.said.slice(0, 280)}…` : attempt.said}”</p>}
            {attempt.diff && (
              <details>
                <summary className="muted small">The change that was thrown away</summary>
                <Code>{attempt.diff}</Code>
              </details>
            )}
          </li>
        ))}
      </ol>
    </div>
  );
}

function Activity({ events }: { events: RunEvent[] }) {
  if (events.length === 0) return <Empty title="Nothing has happened yet" />;
  return (
    <ol className="timeline">
      {events.map((event, i) => {
        const line = describe(event);
        return (
          <li key={i}>
            <Dot tone={line.tone} />
            <div>
              <span>{line.title}</span>
              {line.detail && !line.detail.includes("\n") && line.detail.length <= 140 && (
                <div className="muted small">{line.detail}</div>
              )}
              {line.detail && (line.detail.includes("\n") || line.detail.length > 140) && (
                <details>
                  <summary className="muted small">details</summary>
                  <Code>{line.detail}</Code>
                </details>
              )}
            </div>
          </li>
        );
      })}
    </ol>
  );
}

function SpendTable({ title, rows }: { title: string; rows: Record<string, Spend> }) {
  const entries = Object.entries(rows).sort((a, b) => b[1].cost_usd - a[1].cost_usd || b[1].tokens - a[1].tokens);
  if (entries.length === 0) return null;
  return (
    <section>
      <h2 className="section-title">{title}</h2>
      <div className="table-wrap">
        <table>
          <thead>
            <tr>
              <th />
              <th>Calls</th>
              <th>Fresh</th>
              <th>Cache</th>
              <th>Cost</th>
            </tr>
          </thead>
          <tbody>
            {entries.map(([name, spend]) => (
              <tr key={name}>
                <td className="mono">{name}</td>
                <td>{spend.calls}</td>
                <td>{tokens(spend.tokens - spend.cache_read)}</td>
                <td>{tokens(spend.cache_read)}</td>
                <td>
                  {usd(spend.cost_usd)}
                  {spend.unpriced_calls > 0 && <span className="muted" title={`${spend.unpriced_calls} calls had no price`}> +?</span>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function Cost({ id, calls }: { id: string; calls: number }) {
  const ledger = useLoad(() => api.ledger(id), [id, calls]);
  if (!ledger.data) return ledger.error ? <Problem error={ledger.error} onRetry={ledger.reload} /> : <Spinner />;
  const { totals, entries } = ledger.data;
  if (entries.length === 0) return <Empty title="No agent has run yet" />;
  return (
    <div className="stack">
      <p className="muted small">
        Fresh tokens are what the models read or wrote for the first time. A harness sends its own prompt again at every step and
        the provider serves that from its cache at a lower rate, so cache reads are shown apart.
      </p>
      <SpendTable title="By role" rows={totals.by_role} />
      <SpendTable title="By model" rows={totals.by_model} />
      <section>
        <h2 className="section-title">Every call</h2>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Role</th>
                <th>Page</th>
                <th>Model</th>
                <th>Fresh</th>
                <th>Cost</th>
                <th>Time</th>
              </tr>
            </thead>
            <tbody>
              {entries.map((entry, i) => (
                <tr key={i} className={entry.ok ? "" : "bad-row"}>
                  <td>{entry.role}</td>
                  <td className="mono">{entry.page ?? ""}</td>
                  <td className="mono">{entry.model ? `${entry.harness} ${entry.model}` : entry.harness}</td>
                  <td>{tokens(entry.input + entry.output + entry.cache_write)}</td>
                  <td>{entry.cost_usd === null ? "?" : usd(entry.cost_usd)}</td>
                  <td>{seconds(entry.seconds)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>
    </div>
  );
}

function Changes({ id, calls }: { id: string; calls: number }) {
  const changes = useLoad(() => api.changes(id), [id, calls]);
  if (!changes.data) return changes.error ? <Problem error={changes.error} onRetry={changes.reload} /> : <Spinner />;
  if (changes.data.length === 0) return <Empty title="Nothing committed yet" />;
  return (
    <div className="list">
      {changes.data.map((change: ChangeEntry, i) => (
        <details key={i} className="item">
          <summary>
            <div className="row between gap">
              <span>
                <strong>{CHANGE[change.kind] ?? change.kind}</strong>
                {change.page && <span className="mono muted"> {change.page}</span>}
              </span>
              <span className="muted small">{ago(change.at)}</span>
            </div>
            <div className="muted small">{change.summary}</div>
          </summary>
          <div className="stack small">
            {change.model && <span>by {change.model}</span>}
            {change.commit && (
              <span>
                commit <span className="mono">{change.commit.slice(0, 10)}</span>
              </span>
            )}
            {change.files.length > 0 && <Code>{change.files.join("\n")}</Code>}
            {change.keys.length > 0 && <span className="mono">{change.keys.join(", ")}</span>}
          </div>
        </details>
      ))}
    </div>
  );
}
