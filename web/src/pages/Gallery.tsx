// Home: a chatbox to start a run, the most recently used run cards in a strip, and a
// Show all popup for the rest. A card still shows the latest screenshot or a typed frame.
// Nothing here scans the disk.

import { useEffect, useId, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type RefObject } from "react";
import { AnimatePresence, motion } from "motion/react";
import { Archive, ArchiveRestore, ArrowLeft, ArrowRight, ArrowUp, ChevronRight, GitBranch, Paperclip, Pin, PinOff, Shield, X } from "lucide-react";
import { api, callAt, type ApprovalMode, type RunSummary } from "../api";
import { RoleDeck } from "../components/RoleDeck";
import { PagesPicker, resolvePages, type Pages } from "../components/PagesPicker";
import { modelLabel } from "../components/ProviderPicker";
import { ProviderLogo, providerName } from "../providers/logos";
import { afterStart, problemWith, SEATS, seatChoice, toRequest, useChoice } from "../rolechoice";
import { prefetchRun, useProviders, useSettings } from "../query";
import { runStarted } from "../notify";
import { EmptyRuns } from "../lazy";
import { Loading } from "../ui";
import { Link, navigate } from "../router";
import { ExternalLink } from "../paths";
import { isUnpriced, phaseLabel, tokens, usd } from "../format";
import { bezier, dur, fadeUp, stagger } from "../motion";
import { motionOn, prefs, setPrefs, usePrefs } from "../prefs";
import { byRecent, ordered, toggleFlag, useRuns } from "../runs";
import { dropOnRun } from "../App";
import { Composer, messageText, type ComposerHandle, type Message } from "../composer";
import { Problem, useAction, useLoad } from "../ui";

/** A name the View Transitions API accepts for a run, shared by its card and its page. */
export function morphName(id: string): string {
  return `run-${id.replace(/[^a-zA-Z0-9_-]/g, "_")}`;
}

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

function RunCard({ run, index }: { run: RunSummary; group?: string; index: number }) {
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
      className="card gallery-card"
      onMouseEnter={() => prefetchRun(run.id)}
      onFocus={() => prefetchRun(run.id)}
      onDragOver={(e) => e.dataTransfer.types.includes("Files") && e.preventDefault()}
      onDrop={(e) => {
        if (e.dataTransfer.files.length) {
          e.preventDefault();
          void dropOnRun(run.id, e.dataTransfer.files);
        }
      }}
    >
      <Link morph to={`/runs/${encodeURIComponent(run.id)}`} className="frame-link" aria-label={`Open ${run.id}`} style={{ viewTransitionName: morphName(run.id) } as React.CSSProperties}>
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
          <Link morph to={`/runs/${encodeURIComponent(run.id)}`} className="card-title clip">
            {run.id}
          </Link>
          <div className="card-meta clip">
            {phaseLabel(run)} · {run.pages.length} page{run.pages.length === 1 ? "" : "s"} · {isUnpriced(run.spent) ? `${tokens(run.spent.tokens)} tokens` : usd(run.spent.cost_usd)}
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

function slugify(text: string): string {
  const slug = text
    .toLowerCase()
    .replace(/['"]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 48);
  return slug;
}

function accessLabel(mode?: ApprovalMode) {
  if (mode === "never") return "Approvals off";
  if (mode === "always") return "Ask on every call";
  return "Ask on risky tools";
}

export function Gallery() {
  const { runs } = useRuns();
  const p = usePrefs();
  const settings = useSettings();
  const providers = useProviders();
  const list = byRecent(ordered(runs, p.showArchived));
  const archived = ordered(runs, true).filter((x) => x.group === "Archived").length;
  const daemons = [...p.daemons].sort((a, b) => Number(b.pinned) - Number(a.pinned));
  const project = settings.data?.project;
  const name = project ? project.split(/[/\\]/).filter(Boolean).pop() ?? project : null;
  const projectLabel = name ?? null;
  const branch = settings.data?.branch ?? null;
  const access = accessLabel(settings.data?.approvals?.mode);
  const { choice, setSeat, missing, fresh } = useChoice(settings.data);
  const [remember, setRemember] = useState(false);
  const [rolesOpen, setRolesOpen] = useState(false);
  const max = settings.data?.machine?.max_parallel ?? null;
  const [pages, setPagesState] = useState<Pages | null>(() => prefs().parallel);
  const setPages = (v: Pages) => {
    setPagesState(v);
    setPrefs({ parallel: v });
  };
  const masterPick = seatChoice(choice, "master");
  const providerList = providers.data ?? [];
  const modelPill = masterPick ? `${providerList.find((x) => x.id === masterPick.harness)?.name ?? providerName(masterPick.harness)} · ${modelLabel(providerList, masterPick)}` : "Choose providers";

  const strip = useRef<HTMLDivElement>(null);
  const showAllBtn = useRef<HTMLButtonElement>(null);
  const composer = useRef<ComposerHandle>(null);
  const fileRef = useRef<HTMLInputElement>(null);
  const [allOpen, setAllOpen] = useState(false);
  const [mode, setMode] = useState<"this" | "new">("this");
  const [newName, setNewName] = useState("");
  const [nameTouched, setNameTouched] = useState(false);
  const action = useAction();

  useEffect(() => {
    composer.current?.focus();
  }, []);

  useEffect(() => {
    if (settings.data && !settings.data.project) setMode("new");
  }, [settings.data]);

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
      if (missing.length) {
        setRolesOpen(true);
        throw new Error(`Choose a provider for ${missing.map((x) => SEATS.find((y) => y.id === x)!.label.toLowerCase()).join(", ")}.`);
      }
      for (const seat of SEATS) {
        const problem = problemWith(providerList, seatChoice(choice, seat.id));
        if (problem) {
          setRolesOpen(true);
          throw new Error(`${seat.label}: ${problem}`);
        }
      }
      if (pages == null) {
        setRolesOpen(true);
        throw new Error("Choose how many pages to run at once.");
      }
      const { roles, models: chosen } = toRequest(choice);
      const parallel = resolvePages(pages, max ?? 32);
      if (mode === "new") {
        const name = newName.trim() || slugify(prompt);
        if (!name) throw new Error("Name the project first.");
        const created = await api.createProject({ name, prompt, roles, models: chosen, parallel });
        if (!created.id) throw new Error("The project was created, but the first run did not start.");
        void afterStart(choice, settings.data, true);
        runStarted(created.id);
        composer.current?.clear();
        navigate(`/runs/${encodeURIComponent(created.id)}`);
        return;
      }
      const { id } = await api.createRun({ prompt, roles, models: chosen, parallel });
      void afterStart(choice, settings.data, remember);
      runStarted(id);
      composer.current?.clear();
      navigate(`/runs/${encodeURIComponent(id)}`);
    });
  };

  return (
    <div className="page wide gallery-page">
      <div className="home-above">
        <h2 data-testid="home-heading">
          {mode === "new" ? (
            "Start something new"
          ) : (
            <>Describe the next run{projectLabel ? <> for <span className="ask-project">{projectLabel}</span></> : null}</>
          )}
        </h2>
        <div className="home-mode" role="tablist" aria-label="Choose a project">
          <button
            type="button"
            role="tab"
            data-testid="project-mode-this"
            className={mode === "this" ? "on" : ""}
            aria-selected={mode === "this"}
            disabled={!project}
            onClick={() => setMode("this")}
          >
            This project{name ? `: ${name}` : ""}
          </button>
          <span className="home-mode-sep" aria-hidden>
            ·
          </span>
          <button
            type="button"
            role="tab"
            data-testid="project-mode-new"
            className={mode === "new" ? "on" : ""}
            aria-selected={mode === "new"}
            onClick={() => setMode("new")}
          >
            New project
          </button>
        </div>
        {mode === "new" && (
          <label className="home-name">
            <span>Name</span>
            <input
              data-testid="new-project-name"
              value={newName}
              onChange={(e) => {
                setNameTouched(true);
                setNewName(e.target.value);
              }}
              placeholder="a folder name"
              spellCheck={false}
              autoComplete="off"
            />
          </label>
        )}
        <p className="ask-sub">
          {mode === "new" ? "Name the project, then tell the master what to build." : "A master will plan it; workers take the pages."}
        </p>
      </div>
      <div className="home-composer">
        <Composer
          ref={composer}
          bus
          placeholder="Tell the master what to plan. @ files, $ skills, / commands"
          onChange={(m) => {
            if (mode === "new" && !nameTouched) setNewName(slugify(m.text));
          }}
          onSend={(m) => submit(m)}
          footer={
            <div className="composer-foot">
              <div className="composer-actions">
                <input
                  ref={fileRef}
                  type="file"
                  multiple
                  className="sr-only"
                  aria-hidden
                  tabIndex={-1}
                  onChange={(e) => {
                    if (e.target.files?.length) void composer.current?.addFiles(e.target.files);
                    e.target.value = "";
                  }}
                />
                <button type="button" className="icon attach-btn" aria-label="Attach files" onClick={() => fileRef.current?.click()}>
                  <Paperclip size={16} />
                </button>
              </div>
              <div className="composer-end">
                <button type="button" className={`model-pill ${rolesOpen ? "on" : ""}`} aria-expanded={rolesOpen} title="Roles and pages" onClick={() => setRolesOpen((v) => !v)}>
                  <ProviderLogo id={masterPick?.harness} size={14} />
                  <span className="clip">{modelPill}</span>
                </button>
                <button type="button" className="send-btn" aria-label="Start planning" disabled={action.busy !== null} onClick={() => submit()}>
                  {action.busy ? <span className="spin" aria-hidden /> : <ArrowUp size={16} />}
                </button>
              </div>
            </div>
          }
        />
        <div className="home-meta">
          <span className="home-meta-left">
            <span>Project</span>
            {project && <span className="mono clip" title={project}>{name}</span>}
            {branch && (
              <span className="home-meta-branch clip">
                <GitBranch size={12} aria-hidden />
                {branch}
              </span>
            )}
          </span>
          <span className="home-meta-access">
            <Shield size={12} aria-hidden />
            {access}
          </span>
        </div>
        <Problem error={action.error} />
        {!rolesOpen && (missing.length > 0 || pages == null) && (
          <p className="home-choose">
            <button type="button" className="link" onClick={() => setRolesOpen(true)}>
              {missing.length ? "Choose providers" : "Choose pages at once"}
            </button>
            {missing.length > 0 && pages == null ? " and how many pages to run at once." : missing.length > 0 ? " for each role, then you only do this once." : "."}
          </p>
        )}
      </div>
      <div className="home-below">
        <AnimatePresence initial={false}>
          {rolesOpen && (
            <motion.div
              key="roles"
              className="home-roles"
              initial={motionOn() ? { opacity: 0, y: -6 } : false}
              animate={{ opacity: 1, y: 0 }}
              exit={motionOn() ? { opacity: 0, y: -4 } : undefined}
              transition={{ duration: dur.base, ease: bezier.out }}
            >
              <RoleDeck
                choice={choice}
                providers={providerList}
                loading={providers.isLoading}
                onSeat={setSeat}
                remember={remember}
                onRemember={setRemember}
                fresh={fresh}
                missing={missing}
              />
              <PagesPicker value={pages} onChange={setPages} max={max} />
            </motion.div>
          )}
        </AnimatePresence>
      {!settings.data && runs.length === 0 && <Loading rows={2} tall label="Loading the gallery" />}
      {settings.data && list.length === 0 && (
        <section className="home-empty" aria-label="No runs yet">
          <EmptyRuns />
          <p>No runs yet. Describe one above and the master plans it with you.</p>
        </section>
      )}
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

      {archived > 0 || p.showArchived ? (
        <div className="row gap wrap small gallery-foot">
          <label className="check">
            <input type="checkbox" checked={p.showArchived} onChange={(e) => setPrefs({ showArchived: e.target.checked })} />
            Show archived ({archived})
          </label>
        </div>
      ) : null}
      </div>

      <ShowAll open={allOpen} onClose={() => setAllOpen(false)} list={list} daemons={daemons} archived={archived} />
    </div>
  );
}
