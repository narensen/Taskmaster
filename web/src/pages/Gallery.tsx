// Home: a gallery of this daemon's runs, then the other daemons this browser knows. A card
// shows the run's latest page screenshot, or a typed frame with the first line of the
// prompt and the phase. Nothing here scans the disk.

import { useEffect, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import { Archive, ArchiveRestore, Folder, Inbox, Pin, PinOff, Plus } from "lucide-react";
import { api, callAt, type RunSummary } from "../api";
import { Link } from "../router";
import { ExternalLink } from "../paths";
import { isUnpriced, phaseLabel, phaseTone, tokens, usd } from "../format";
import { bezier, dur, fadeUp, stagger } from "../motion";
import { motionOn, setPrefs, usePrefs } from "../prefs";
import { ordered, toggleFlag, useRuns } from "../runs";
import { dropOnRun } from "../App";
import { Dot, Empty, useLoad } from "../ui";

const shots = new Map<string, string | null>();
const prompts = new Map<string, string | null>();

function useShot(run: RunSummary): { shot: string | null | undefined; prompt: string | null | undefined } {
  const key = `${run.id}:${run.pages.length}:${run.phase}`;
  const [shot, setShot] = useState<string | null | undefined>(shots.get(key));
  const [prompt, setPrompt] = useState<string | null | undefined>(prompts.get(run.id));
  useEffect(() => {
    let stale = false;
    if (!shots.has(key)) {
      api
        .screens(run.id)
        .then(async (pages) => {
          const last = pages[pages.length - 1];
          const url = last ? await api.screen(run.id, last) : null;
          shots.set(key, url);
          if (!stale) setShot(url);
        })
        .catch(() => !stale && setShot(null));
    }
    if (!prompts.has(run.id)) {
      api
        .run(run.id)
        .then((detail) => {
          const first = detail.prompt?.split("\n").find((l) => l.trim()) ?? null;
          prompts.set(run.id, first);
          if (!stale) setPrompt(first);
        })
        .catch(() => !stale && setPrompt(null));
    }
    return () => {
      stale = true;
    };
  }, [key, run.id]);
  return { shot, prompt };
}

function RunCard({ run, group, index }: { run: RunSummary; group: string; index: number }) {
  const { shot, prompt } = useShot(run);
  const p = usePrefs();
  const flags = p.runFlags[run.id] ?? {};
  const on = motionOn();
  return (
    <motion.article
      layout
      initial={on ? fadeUp.initial : false}
      animate={fadeUp.animate}
      transition={{
        ...fadeUp.transition,
        delay: on ? stagger(index) : 0,
        layout: { duration: on ? dur.base : 0, ease: bezier.out },
      }}
      className={`card gallery-card ${group === "Needs approval" ? "ask" : ""}`}
      onDragOver={(e) => e.dataTransfer.types.includes("Files") && e.preventDefault()}
      onDrop={(e) => {
        if (e.dataTransfer.files.length) {
          e.preventDefault();
          void dropOnRun(run.id, e.dataTransfer.files);
        }
      }}
    >
      <Link to={`/runs/${encodeURIComponent(run.id)}`} className="frame-link" aria-label={`Open ${run.id}`}>
        {shot ? (
          <img className="frame shot" src={shot} alt={`Latest page of ${run.id}`} />
        ) : (
          <div className="frame typed" data-testid="typed-frame">
            <div className="typed-line">{prompt ?? (prompt === null ? "A run from a plan file" : " ")}</div>
            <div className="typed-phase">
              <Dot tone={run.cancelled ? "quiet" : group === "Working" ? "busy" : phaseTone(run.phase)} />
              {phaseLabel(run)}
            </div>
          </div>
        )}
        {shot && (
          <span className={`frame-badge tone-${run.cancelled ? "quiet" : phaseTone(run.phase)}`}>
            <Dot tone={run.cancelled ? "quiet" : group === "Working" ? "busy" : phaseTone(run.phase)} />
            {phaseLabel(run)}
          </span>
        )}
      </Link>
      <div className="card-foot">
        <div className="grow">
          <Link to={`/runs/${encodeURIComponent(run.id)}`} className="card-title clip">
            {run.id}
          </Link>
          <div className="card-meta clip">
            {group.toLowerCase()} · {run.pages.length} page{run.pages.length === 1 ? "" : "s"} · {isUnpriced(run.spent) ? `${tokens(run.spent.tokens)} tokens` : usd(run.spent.cost_usd)}
          </div>
        </div>
        <div className="card-actions">
          <button className="icon" title={flags.pinned ? "Unpin" : "Pin"} onClick={() => toggleFlag(run.id, "pinned")}>
            {flags.pinned ? <PinOff size={14} aria-hidden /> : <Pin size={14} aria-hidden />}
            <span className="sr-only">{flags.pinned ? "unpin" : "pin"}</span>
          </button>
          <button className="icon" title={flags.archived ? "Unarchive" : "Archive"} onClick={() => toggleFlag(run.id, "archived")}>
            {flags.archived ? <ArchiveRestore size={14} aria-hidden /> : <Archive size={14} aria-hidden />}
            <span className="sr-only">{flags.archived ? "unarchive" : "archive"}</span>
          </button>
        </div>
      </div>
    </motion.article>
  );
}

function DaemonCard({ url, token, pinned, index }: { url: string; token: string | null; pinned: boolean; index: number }) {
  const state = useLoad(() => callAt<{ runs: RunSummary[] }>(url, token, "/api/runs"), [url, token], 15000);
  const on = motionOn();
  return (
    <motion.article
      layout
      className="card gallery-card"
      initial={on ? fadeUp.initial : false}
      animate={fadeUp.animate}
      transition={{
        ...fadeUp.transition,
        delay: on ? stagger(index) : 0,
        layout: { duration: on ? dur.base : 0, ease: bezier.out },
      }}
    >
      <div className="frame typed">
        <div className="typed-line">{url}</div>
        <div className="typed-phase">
          {state.error ? "not reachable with the saved token" : state.data ? `${state.data.runs.length} runs` : <span className="skeleton">…</span>}
        </div>
      </div>
      <div className="card-foot">
        <div className="grow">
          <ExternalLink href={url}>{url.replace(/^https?:\/\//, "")}</ExternalLink>
          <div className="muted small">another daemon</div>
        </div>
        <button
          className="icon"
          onClick={() => setPrefs((p) => ({ daemons: p.daemons.map((d) => (d.url === url ? { ...d, pinned: !d.pinned } : d)) }))}
        >
          {pinned ? <PinOff size={14} aria-hidden /> : <Pin size={14} aria-hidden />}
          <span className="sr-only">{pinned ? "unpin" : "pin"}</span>
        </button>
      </div>
    </motion.article>
  );
}

export function Gallery() {
  const { runs } = useRuns();
  const p = usePrefs();
  const settings = useLoad(api.settings, []);
  const list = ordered(runs, p.showArchived);
  const archived = ordered(runs, true).filter((x) => x.group === "Archived").length;
  const daemons = [...p.daemons].sort((a, b) => Number(b.pinned) - Number(a.pinned));

  const waiting = list.filter((x) => x.group === "Needs approval").length;
  const working = list.filter((x) => x.group === "Working").length;
  const project = settings.data?.project;
  const name = project ? project.split(/[/\\]/).filter(Boolean).pop() ?? project : null;

  return (
    <div className="page wide gallery-page">
      <header className="page-head">
        <div className="grow">
          <div className="eyebrow">Project</div>
          <h1 className="break">{name ?? <span className="skeleton">…</span>}</h1>
          <div className="page-sub">
            <Folder size={13} aria-hidden />
            <span className="mono clip" data-testid="project-path">{project ?? "…"}</span>
          </div>
        </div>
        <div className="page-head-actions">
          {runs.length > 0 && (
            <div className="head-stats" aria-label="Run summary">
              <span><strong>{runs.length}</strong> run{runs.length === 1 ? "" : "s"}</span>
              {working > 0 && <span className="stat-busy"><Dot tone="busy" /> {working} working</span>}
              {waiting > 0 && <span className="stat-ask"><Dot tone="ask" /> {waiting} need{waiting === 1 ? "s" : ""} approval</span>}
            </div>
          )}
          <Link to="/runs/new" className="button primary">
            <Plus size={15} aria-hidden />
            New run
          </Link>
        </div>
      </header>
      {runs.length === 0 && (
        <Empty
          title="No runs yet"
          icon={<Inbox size={18} strokeWidth={1.5} />}
          action={
            <Link to="/runs/new" className="button primary">
              <Plus size={15} aria-hidden />
              Start a run
            </Link>
          }
        >
          Describe what to build. Taskmaster plans it, splits it across agents, and each run gets a card here with a live screenshot.
        </Empty>
      )}
      <div className="gallery">
        <AnimatePresence>
          {list.map(({ run, group }, i) => (
            <RunCard key={run.id} run={run} group={group} index={i} />
          ))}
        </AnimatePresence>
      </div>
      {archived > 0 || p.showArchived ? (
        <div className="row gap wrap small gallery-foot">
          <label className="check">
            <input type="checkbox" checked={p.showArchived} onChange={(e) => setPrefs({ showArchived: e.target.checked })} />
            Show archived ({archived})
          </label>
        </div>
      ) : null}
      {daemons.length > 0 && (
        <>
          <h2 className="section-title">Other daemons</h2>
          <div className="gallery">
            {daemons.map((d, i) => (
              <DaemonCard key={d.url} {...d} index={i} />
            ))}
          </div>
        </>
      )}
    </div>
  );
}
