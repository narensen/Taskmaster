// Home: a gallery of this daemon's runs, then the other daemons this browser knows. A card
// shows the run's latest page screenshot, or a typed frame with the first line of the
// prompt and the phase. Nothing here scans the disk.

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { motion, AnimatePresence } from "framer-motion";
import gsap from "gsap";
import { api, callAt, type RunSummary } from "../api";
import { Link } from "../router";
import { ExternalLink } from "../paths";
import { phaseLabel, usd } from "../format";
import { bezier, dur, ease } from "../motion";
import { motionOn, setPrefs, usePrefs } from "../prefs";
import { ordered, toggleFlag, useRuns } from "../runs";
import { dropOnRun } from "../App";
import { Empty, useLoad } from "../ui";

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

function RunCard({ run, group }: { run: RunSummary; group: string }) {
  const { shot, prompt } = useShot(run);
  const p = usePrefs();
  const flags = p.runFlags[run.id] ?? {};
  return (
    <motion.article
      layout
      transition={{ layout: { duration: dur.base, ease: bezier.out } }}
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
            <div className="typed-phase">{phaseLabel(run)}</div>
          </div>
        )}
      </Link>
      <div className="card-foot">
        <div className="grow">
          <div className="strong clip">{run.id}</div>
          <div className="muted small">
            {group.toLowerCase()} · {run.pages.length} page{run.pages.length === 1 ? "" : "s"} · {usd(run.spent.cost_usd)}
          </div>
        </div>
        <button className="icon" onClick={() => toggleFlag(run.id, "pinned")}>
          {flags.pinned ? "unpin" : "pin"}
        </button>
        <button className="icon" onClick={() => toggleFlag(run.id, "archived")}>
          {flags.archived ? "unarchive" : "archive"}
        </button>
      </div>
    </motion.article>
  );
}

function DaemonCard({ url, token, pinned }: { url: string; token: string | null; pinned: boolean }) {
  const state = useLoad(() => callAt<{ runs: RunSummary[] }>(url, token, "/api/runs"), [url, token], 15000);
  return (
    <motion.article layout transition={{ layout: { duration: dur.base, ease: bezier.out } }} className="card gallery-card">
      <div className="frame typed">
        <div className="typed-line">{url}</div>
        <div className="typed-phase">
          {state.error ? "not reachable with the saved token" : state.data ? `${state.data.runs.length} runs` : "…"}
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
          {pinned ? "unpin" : "pin"}
        </button>
      </div>
    </motion.article>
  );
}

export function Gallery() {
  const { runs } = useRuns();
  const p = usePrefs();
  const settings = useLoad(api.settings, []);
  const grid = useRef<HTMLDivElement>(null);
  const list = ordered(runs, p.showArchived);
  const archived = ordered(runs, true).filter((x) => x.group === "Archived").length;
  const daemons = [...p.daemons].sort((a, b) => Number(b.pinned) - Number(a.pinned));
  const animated = useRef(false);

  useLayoutEffect(() => {
    if (animated.current || !grid.current || !list.length) return;
    animated.current = true;
    if (!motionOn()) return;
    gsap.from(grid.current.querySelectorAll(".gallery-card"), { opacity: 0, y: 8, duration: dur.base, stagger: 0.03, ease: ease.out, clearProps: "opacity,transform" });
  }, [list.length]);

  return (
    <div className="page wide">
      <header className="page-head">
        <div className="grow">
          <div className="eyebrow">Project</div>
          <h1 className="break">{settings.data?.project ?? "…"}</h1>
        </div>
        <Link to="/runs/new" className="button primary">
          New run
        </Link>
      </header>
      {runs.length === 0 && <Empty title="No runs yet">Start one with New run. Each run gets a card here.</Empty>}
      <div className="gallery" ref={grid}>
        <AnimatePresence>
          {list.map(({ run, group }) => (
            <RunCard key={run.id} run={run} group={group} />
          ))}
        </AnimatePresence>
      </div>
      <div className="row gap wrap small">
        <label className="check">
          <input type="checkbox" checked={p.showArchived} onChange={(e) => setPrefs({ showArchived: e.target.checked })} />
          Show archived ({archived})
        </label>
      </div>
      {daemons.length > 0 && (
        <>
          <h2 className="section-title">Other daemons</h2>
          <div className="gallery">
            {daemons.map((d) => (
              <DaemonCard key={d.url} {...d} />
            ))}
          </div>
        </>
      )}
    </div>
  );
}
