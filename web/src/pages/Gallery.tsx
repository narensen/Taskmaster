// Home: a chatbox to start a run, the most recently used run cards in a strip, and a
// Show all popup for the rest. A card still shows the latest screenshot or a typed frame.
// Nothing here scans the disk.

import { useEffect, useId, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type RefObject } from "react";
import { motion } from "framer-motion";
import { Archive, ArchiveRestore, ArrowLeft, ArrowRight, ArrowUp, ChevronRight, Folder, Inbox, Pin, PinOff, X } from "lucide-react";
import { api, callAt, DEFAULT_ROLES, type RunSummary } from "../api";
import { Link, navigate } from "../router";
import { ExternalLink } from "../paths";
import { isUnpriced, phaseLabel, phaseTone, tokens, usd } from "../format";
import { bezier, dur, fadeUp, stagger } from "../motion";
import { motionOn, setPrefs, usePrefs } from "../prefs";
import { byRecent, ordered, toggleFlag, useRuns } from "../runs";
import { dropOnRun } from "../App";
import { Composer, messageText, type ComposerHandle, type Message } from "../composer";
import { Dot, Empty, Problem, useAction, useLoad } from "../ui";

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

function useFocusTrap(ref: RefObject<HTMLElement | null>, active: boolean, onClose: () => void) {
  useEffect(() => {
    if (!active || !ref.current) return;
    const root = ref.current;
    const sel = "a[href], button:not([disabled]), input, textarea, select, [tabindex]:not([tabindex='-1'])";
    const list = () => [...root.querySelectorAll<HTMLElement>(sel)].filter((el) => el.tabIndex !== -1 && !el.closest("[disabled]"));
    const previously = document.activeElement as HTMLElement | null;
    list()[0]?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
        return;
      }
      if (e.key !== "Tab") return;
      const items = list();
      const first = items[0];
      const last = items[items.length - 1];
      if (!first || !last) return;
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault();
        last.focus();
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault();
        first.focus();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("keydown", onKey);
      previously?.focus();
    };
  }, [active, onClose, ref]);
}

function scrollStrip(el: HTMLElement | null, dir: 1 | -1) {
  if (!el) return;
  const card = el.querySelector<HTMLElement>(".gallery-card");
  const gap = 16;
  const w = card ? card.offsetWidth + gap : Math.max(200, el.clientWidth * 0.7);
  el.scrollBy({ left: dir * w, behavior: motionOn() ? "smooth" : "auto" });
}

function ShowAll({
  open,
  onClose,
  list,
  daemons,
  archived,
}: {
  open: boolean;
  onClose: () => void;
  list: { run: RunSummary; group: string }[];
  daemons: { url: string; token: string | null; pinned: boolean }[];
  archived: number;
}) {
  const p = usePrefs();
  const box = useRef<HTMLDivElement>(null);
  const titleId = useId();
  const [q, setQ] = useState("");
  useFocusTrap(box, open, onClose);
  useEffect(() => {
    if (open) setQ("");
  }, [open]);
  const query = q.trim().toLowerCase();
  const runs = query
    ? list.filter(({ run, group }) => {
        const prompt = prompts.get(run.id) ?? "";
        return run.id.toLowerCase().includes(query) || group.toLowerCase().includes(query) || prompt.toLowerCase().includes(query);
      })
    : list;
  const others = query ? daemons.filter((d) => d.url.toLowerCase().includes(query)) : daemons;
  if (!open) return null;
  return (
    <div className="showall-back" onMouseDown={onClose}>
      <div
        ref={box}
        className="showall-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        data-testid="show-all-dialog"
        onMouseDown={(e) => e.stopPropagation()}
      >
        <div className="showall-head">
          <h2 id={titleId}>All runs</h2>
          <input
            data-testid="show-all-search"
            type="search"
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="Search runs"
            aria-label="Search runs"
          />
          <button type="button" className="icon" aria-label="Close" onClick={onClose}>
            <X size={16} />
          </button>
        </div>
        <div className="showall-body">
          {runs.length === 0 && others.length === 0 ? (
            <div className="muted" style={{ padding: "12px 4px" }}>
              Nothing matches “{q}”.
            </div>
          ) : (
            <div className="gallery">
              {runs.map(({ run, group }, i) => (
                <RunCard key={run.id} run={run} group={group} index={i} />
              ))}
              {others.map((d, i) => (
                <DaemonCard key={d.url} {...d} index={i} />
              ))}
            </div>
          )}
        </div>
        {archived > 0 || p.showArchived ? (
          <div className="showall-foot">
            <label className="check">
              <input type="checkbox" checked={p.showArchived} onChange={(e) => setPrefs({ showArchived: e.target.checked })} />
              Show archived ({archived})
            </label>
          </div>
        ) : null}
      </div>
    </div>
  );
}

export function Gallery() {
  const { runs } = useRuns();
  const p = usePrefs();
  const settings = useLoad(api.settings, []);
  const list = byRecent(ordered(runs, p.showArchived));
  const archived = ordered(runs, true).filter((x) => x.group === "Archived").length;
  const daemons = [...p.daemons].sort((a, b) => Number(b.pinned) - Number(a.pinned));
  const waiting = list.filter((x) => x.group === "Needs approval").length;
  const working = list.filter((x) => x.group === "Working").length;
  const project = settings.data?.project;
  const name = project ? project.split(/[/\\]/).filter(Boolean).pop() ?? project : null;
  const projectLabel = name ?? null;

  const strip = useRef<HTMLDivElement>(null);
  const showAllBtn = useRef<HTMLButtonElement>(null);
  const composer = useRef<ComposerHandle>(null);
  const [allOpen, setAllOpen] = useState(false);
  const action = useAction();

  const onStripKey = (e: ReactKeyboardEvent) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    scrollStrip(strip.current, e.key === "ArrowRight" ? 1 : -1);
  };

  const submit = (message?: Message) => {
    const m = message ?? composer.current?.read();
    if (!m) return;
    const prompt = messageText(m);
    void action.run("create", async () => {
      if (!prompt) throw new Error("Write what to build first.");
      const roles = { ...DEFAULT_ROLES, ...(settings.data?.roles ?? {}) };
      const models = settings.data?.models ?? {};
      const chosen = Object.fromEntries(Object.entries(models).filter(([, model]) => model));
      const { id } = await api.createRun({ prompt, roles, models: chosen });
      composer.current?.clear();
      navigate(`/runs/${encodeURIComponent(id)}`);
    });
  };

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
        </div>
      </header>

      {list.length > 0 && (
        <section className="home-recent" aria-label="Recent runs">
          <div className="home-recent-head">
            <h2>Recent</h2>
            <div className="home-recent-actions">
              <button type="button" className="icon" aria-label="Previous runs" onClick={() => scrollStrip(strip.current, -1)}>
                <ArrowLeft size={16} />
              </button>
              <button type="button" className="icon" aria-label="Next runs" onClick={() => scrollStrip(strip.current, 1)}>
                <ArrowRight size={16} />
              </button>
              <button ref={showAllBtn} type="button" className="ghost small" data-testid="show-all" onClick={() => setAllOpen(true)}>
                Show all
                <ChevronRight size={14} aria-hidden />
              </button>
            </div>
          </div>
          <div
            ref={strip}
            className="home-strip"
            data-testid="run-strip"
            tabIndex={0}
            aria-label="Recently used runs"
            onKeyDown={onStripKey}
          >
            {list.map(({ run, group }, i) => (
              <RunCard key={run.id} run={run} group={group} index={i} />
            ))}
            {daemons.map((d, i) => (
              <DaemonCard key={d.url} {...d} index={list.length + i} />
            ))}
          </div>
        </section>
      )}

      {runs.length === 0 && (
        <Empty title="No runs yet" icon={<Inbox size={18} strokeWidth={1.5} />}>
          Describe what to build in the box below. Taskmaster plans it, splits it across agents, and each run gets a card here.
        </Empty>
      )}

      <div className="home-ask">
        <h2>
          What should we build{projectLabel ? <> in <span className="ask-project">{projectLabel}</span></> : null}?
        </h2>
        <Composer
          ref={composer}
          bus
          placeholder="Ask anything. @ files, # pull requests, / commands"
          onSend={(m) => submit(m)}
          footer={
            <div className="composer-foot">
              <div className="composer-actions">
                <Link to="/runs/new" className="muted small">
                  More options
                </Link>
                <span className="hint ask-keys">
                  <kbd>Enter</kbd> sends · <kbd>Shift</kbd>+<kbd>Enter</kbd> new line
                </span>
              </div>
              <button type="button" className="send-btn" aria-label="Start planning" disabled={action.busy !== null} onClick={() => submit()}>
                {action.busy ? <span className="spin" aria-hidden /> : <ArrowUp size={16} />}
              </button>
            </div>
          }
        />
        <Problem error={action.error} />
      </div>

      {archived > 0 || p.showArchived ? (
        <div className="row gap wrap small gallery-foot">
          <label className="check">
            <input type="checkbox" checked={p.showArchived} onChange={(e) => setPrefs({ showArchived: e.target.checked })} />
            Show archived ({archived})
          </label>
        </div>
      ) : null}

      <ShowAll open={allOpen} onClose={() => setAllOpen(false)} list={list} daemons={daemons} archived={archived} />
    </div>
  );
}
