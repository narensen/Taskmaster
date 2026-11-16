import { useEffect, useMemo, useState } from "react";
import { AnimatePresence, LayoutGroup, MotionConfig, motion } from "framer-motion";
import {
  Archive,
  ArchiveRestore,
  BookOpen,
  Bug,
  ChevronDown,
  Circle,
  CircleAlert,
  CircleCheck,
  Cpu,
  GitBranch,
  GitPullRequest,
  LayoutGrid,
  Loader2,
  Menu,
  Pin,
  PinOff,
  Plus,
  Search,
  Settings,
  SquarePen,
  type LucideIcon,
} from "lucide-react";
import { api, setToken, token, tokenFromRedirect, type DaemonSettings, type RunSummary } from "./api";
import { Link, match, navigate, usePath } from "./router";
import { Dictionary } from "./pages/Dictionary";
import { Errorography } from "./pages/Errorography";
import { NewRun } from "./pages/NewRun";
import { ReviewView, Reviews } from "./pages/Reviews";
import { RunView } from "./pages/RunView";
import { Gallery } from "./pages/Gallery";
import { Providers } from "./pages/Providers";
import { Settings as SettingsPage } from "./pages/Settings";
import { SignIn } from "./pages/SignIn";
import { ContextMenu, Toasts, openMenu, undoLatest, type Chip } from "./overlay";
import { Palette, type PaletteState } from "./palette";
import { prefs, setPrefs, useMotion, usePrefs } from "./prefs";
import { RunsContext, ordered, toggleFlag, type Group } from "./runs";
import { phaseLabel, shortSpan } from "./format";
import { readFile } from "./composer";
import { bezier, dur } from "./motion";
import { useLoad } from "./ui";

const NAV: { to: string; label: string; icon: LucideIcon; active?: (p: string) => boolean }[] = [
  { to: "/", label: "Gallery", icon: LayoutGrid, active: (p) => p === "/" || p === "/runs" },
  { to: "/runs/new", label: "New run", icon: Plus },
  { to: "/dictionary", label: "Dictionary", icon: BookOpen },
  { to: "/errorography", label: "Errorography", icon: Bug },
  { to: "/reviews", label: "Reviews", icon: GitPullRequest },
  { to: "/providers", label: "Providers", icon: Cpu },
  { to: "/settings", label: "Settings", icon: Settings },
];

/** Files dropped on a run card wait here for that run's composer. */
const waiting = new Map<string, Chip[]>();
export function takeAttachments(run: string): Chip[] {
  const list = waiting.get(run) ?? [];
  waiting.delete(run);
  return list;
}
export async function dropOnRun(run: string, files: FileList) {
  const chips = await Promise.all([...files].map(readFile));
  waiting.set(run, [...(waiting.get(run) ?? []), ...chips]);
  navigate(`/runs/${encodeURIComponent(run)}`);
}

function useSignedIn(): boolean {
  const [signedIn, setSignedIn] = useState(() => {
    // Back from an account sign-in: keep the token and drop it from the address bar.
    const handed = tokenFromRedirect();
    if (handed) {
      setToken(handed);
      const back = window.location.pathname === "/signed-in" ? "/" : window.location.pathname + window.location.search;
      window.history.replaceState(null, "", back);
    }
    return token() !== null;
  });
  useEffect(() => {
    const update = () => setSignedIn(token() !== null);
    window.addEventListener("taskmaster-auth", update);
    window.addEventListener("storage", update);
    return () => {
      window.removeEventListener("taskmaster-auth", update);
      window.removeEventListener("storage", update);
    };
  }, []);
  return signedIn;
}

function Page({ path }: { path: string }) {
  let params: Record<string, string> | null;
  if (path === "/" || path === "/runs") return <Gallery />;
  if (path === "/runs/new") return <NewRun />;
  if ((params = match(path, "/runs/:id"))) return <RunView key={params.id} id={params.id!} />;
  if (path === "/dictionary") return <Dictionary />;
  if (path === "/errorography") return <Errorography />;
  if (path === "/reviews") return <Reviews />;
  if ((params = match(path, "/reviews/:id"))) return <ReviewView id={params.id!} />;
  if (path === "/providers") return <Providers />;
  if (path === "/settings") return <SettingsPage />;
  return (
    <div className="page">
      <h1>Nothing here</h1>
      <p>
        <Link to="/">Back to the gallery</Link>
      </p>
    </div>
  );
}

export function typing(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName);
}

function projectName(project: string | undefined): string {
  if (!project) return "Project";
  const parts = project.split(/[/\\]/).filter(Boolean);
  return parts[parts.length - 1] || project;
}

function hereLabel(path: string): string {
  if (path === "/" || path === "/runs") return "Gallery";
  if (path === "/runs/new") return "New run";
  if (path.startsWith("/runs/")) return decodeURIComponent(path.slice("/runs/".length));
  if (path === "/dictionary") return "Dictionary";
  if (path === "/errorography") return "Errorography";
  if (path === "/reviews") return "Reviews";
  if (path.startsWith("/reviews/")) return "Review";
  if (path === "/providers") return "Providers";
  if (path === "/settings") return "Settings";
  return "Taskmaster";
}

function Crumbs({ project, here }: { project: string; here: string }) {
  return (
    <div className="crumbs">
      <span className="clip">{project}</span>
      <span className="crumb-sep">/</span>
      <span className="clip crumb-here">{here}</span>
    </div>
  );
}

/** First and last ledger timestamps, so Working can show elapsed time and Settled a relative time. */
function useClocks(ids: string[]) {
  const [clocks, setClocks] = useState<Record<string, { start: number; end: number }>>({});
  const key = ids.join("|");
  useEffect(() => {
    let cancel = false;
    for (const id of key ? key.split("|") : []) {
      api
        .ledger(id)
        .then((ledger) => {
          const stamps = ledger.entries.map((e) => Date.parse(e.at)).filter((n) => !Number.isNaN(n));
          if (cancel || !stamps.length) return;
          setClocks((c) => (c[id] ? c : { ...c, [id]: { start: Math.min(...stamps), end: Math.max(...stamps) } }));
        })
        .catch(() => {});
    }
    return () => {
      cancel = true;
    };
  }, [key]);
  return clocks;
}

function RunMark({ group }: { group: Group }) {
  if (group === "Working") return <Loader2 size={13} className="spin-ico" />;
  if (group === "Needs approval") return <CircleAlert size={13} />;
  if (group === "Pinned") return <Pin size={13} />;
  if (group === "Archived") return <Archive size={13} />;
  return <Circle size={13} />;
}

function RunList({ runs, path, onPick }: { runs: RunSummary[]; path: string; onPick: () => void }) {
  const p = usePrefs();
  const list = ordered(runs, p.showArchived);
  const clocks = useClocks(list.map((x) => x.run.id));
  const working = list.some((x) => x.group === "Working");
  const [now, setNow] = useState(() => Date.now());
  const [folded, setFolded] = useState<Partial<Record<Group, boolean>>>({});
  useEffect(() => {
    if (!working) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [working]);
  const counts = new Map<Group, number>();
  for (const item of list) counts.set(item.group, (counts.get(item.group) ?? 0) + 1);
  let last: Group | null = null;
  return (
    <div className="runlist">
      <LayoutGroup>
        <ul>
          <AnimatePresence initial={false}>
            {list.map(({ run, group }, i) => {
              const head = group !== last ? group : null;
              last = group;
              const shut = !!folded[group];
              if (shut && !head) return null;
              const on = path === `/runs/${encodeURIComponent(run.id)}` || path === `/runs/${run.id}`;
              const flags = p.runFlags[run.id] ?? {};
              const clock = clocks[run.id];
              const menu = (x: number, y: number) =>
                openMenu(x, y, [
                  { label: flags.pinned ? "Unpin" : "Pin", run: () => toggleFlag(run.id, "pinned") },
                  { label: flags.settled ? "Unsettle" : "Settle", run: () => toggleFlag(run.id, "settled") },
                  { label: flags.archived ? "Unarchive" : "Archive", run: () => toggleFlag(run.id, "archived") },
                ]);
              const when =
                group === "Working" && clock
                  ? shortSpan(now - clock.start)
                  : group !== "Working" && clock
                    ? shortSpan(now - clock.end)
                    : null;
              const stamp =
                group === "Working" ? (when ? `Working ${when}` : phaseLabel(run).toLowerCase()) : (when ?? phaseLabel(run).toLowerCase());
              return (
                <motion.li
                  key={run.id}
                  layout="position"
                  initial={{ opacity: 0, y: 4 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0, y: -4 }}
                  transition={{ duration: 0.16, ease: bezier.out, layout: { duration: dur.base, ease: bezier.out } }}
                >
                  {head && (
                    <button
                      type="button"
                      className="group-head"
                      aria-expanded={!shut}
                      onClick={() => setFolded((f) => ({ ...f, [group]: !f[group] }))}
                    >
                      <span>{head}</span>
                      <span className="group-count">
                        {counts.get(head)}
                        <ChevronDown size={12} className={shut ? "chev closed" : "chev"} />
                      </span>
                    </button>
                  )}
                  {!shut && (
                    <div
                      className={`runrow ${on ? "on" : ""} ${group === "Needs approval" ? "ask" : ""} ${group === "Settled" ? "settled" : ""}`}
                      onDragOver={(e) => e.dataTransfer.types.includes("Files") && e.preventDefault()}
                      onDrop={(e) => {
                        if (e.dataTransfer.files.length) {
                          e.preventDefault();
                          void dropOnRun(run.id, e.dataTransfer.files);
                        }
                      }}
                      onContextMenu={(e) => {
                        e.preventDefault();
                        menu(e.clientX, e.clientY);
                      }}
                    >
                      <Link to={`/runs/${encodeURIComponent(run.id)}`} className="runrow-link" onClick={onPick}>
                        <span className={`run-ico ${group === "Working" ? "working" : ""} ${group === "Needs approval" ? "ask" : ""}`} aria-hidden>
                          <RunMark group={group} />
                        </span>
                        <span className="run-copy">
                          <span className="clip run-title">{run.id}</span>
                          {run.branch && (
                            <span className="run-branch clip">
                              <GitBranch size={11} />
                              {run.branch}
                            </span>
                          )}
                        </span>
                        <span className={`runrow-phase ${group === "Working" ? "elapsed" : ""}`}>
                          {i < 9 && <kbd>{i + 1}</kbd>}
                          <span>{stamp}</span>
                        </span>
                      </Link>
                      <span className="runrow-actions">
                        <button className="icon" title={flags.pinned ? "Unpin" : "Pin"} aria-label={flags.pinned ? "Unpin" : "Pin"} onClick={() => toggleFlag(run.id, "pinned")}>
                          {flags.pinned ? <PinOff size={13} /> : <Pin size={13} />}
                        </button>
                        <button className="icon" title={flags.settled ? "Unsettle" : "Settle"} aria-label={flags.settled ? "Unsettle" : "Settle"} onClick={() => toggleFlag(run.id, "settled")}>
                          {flags.settled ? <CircleCheck size={13} /> : <Circle size={13} />}
                        </button>
                        <button className="icon" title={flags.archived ? "Unarchive" : "Archive"} aria-label={flags.archived ? "Unarchive" : "Archive"} onClick={() => toggleFlag(run.id, "archived")}>
                          {flags.archived ? <ArchiveRestore size={13} /> : <Archive size={13} />}
                        </button>
                      </span>
                    </div>
                  )}
                </motion.li>
              );
            })}
          </AnimatePresence>
        </ul>
      </LayoutGroup>
      <label className="check small">
        <input type="checkbox" checked={p.showArchived} onChange={(e) => setPrefs({ showArchived: e.target.checked })} />
        Show archived
      </label>
    </div>
  );
}

export function App() {
  const path = usePath();
  const signedIn = useSignedIn();
  const p = usePrefs();
  const motionOn = useMotion();
  const [palette, setPalette] = useState<PaletteState | null>(null);
  const [drawer, setDrawer] = useState(false);
  const runs = useLoad(() => (signedIn ? api.runs() : Promise.resolve([] as RunSummary[])), [signedIn], 3000);
  const settings = useLoad(() => (signedIn ? api.settings() : Promise.resolve(null as DaemonSettings | null)), [signedIn]);

  useEffect(() => {
    if (signedIn && path === "/signed-in") navigate("/", true);
  }, [signedIn, path]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && !e.shiftKey && e.key.toLowerCase() === "z") {
        // An Undo on screen wins, even inside the composer.
        if (undoLatest()) {
          e.preventDefault();
          e.stopPropagation();
        }
        return;
      }
      if (mod && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPalette((s) => (s ? null : { open: true, mode: "all" }));
        return;
      }
      if (typing(e.target)) return;
      if (e.key === "/" && !mod) {
        e.preventDefault();
        setPalette({ open: true, mode: "slash" });
        return;
      }
      if (mod && /^[1-9]$/.test(e.key)) {
        const list = ordered(runs.data ?? [], prefs().showArchived);
        const run = list[Number(e.key) - 1]?.run;
        if (run) {
          e.preventDefault();
          navigate(`/runs/${encodeURIComponent(run.id)}`);
        }
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [runs.data]);

  const runsState = useMemo(() => ({ runs: runs.data ?? [], reload: () => void runs.reload() }), [runs.data, runs.reload]);
  const project = projectName(settings.data?.project);
  const here = hereLabel(path);

  if (!signedIn) return <SignIn />;

  return (
    <MotionConfig reducedMotion={motionOn ? "never" : "always"}>
      <RunsContext.Provider value={runsState}>
        <div className={`shell theme-${p.theme} density-${p.density} ${motionOn ? "" : "no-motion"}`}>
          <header className="topbar">
            <button className="ghost small" onClick={() => setDrawer(!drawer)} aria-label="Menu">
              <Menu size={16} />
            </button>
            <Crumbs project={project} here={here} />
            <button className="ghost small" aria-label="Search" onClick={() => setPalette({ open: true, mode: "all" })}>
              <Search size={16} />
            </button>
          </header>
          <aside className={`side ${drawer ? "open" : ""}`} aria-label="Main">
            <Link to="/" className="brand side-brand" onClick={() => setDrawer(false)}>
              <img src="/icon.png" alt="" width={16} height={16} />
              <span>Taskmaster</span>
            </Link>
            <div className="side-search">
              <button className="palette-hint" onClick={() => setPalette({ open: true, mode: "all" })}>
                <Search size={14} />
                <span>Search</span>
                <kbd>⌘K</kbd>
              </button>
              <Link to="/runs/new" className="side-new" aria-label="New run" title="New run" onClick={() => setDrawer(false)}>
                <SquarePen size={14} />
              </Link>
            </div>
            <nav className="nav">
              {NAV.map((item) => {
                const on = item.active ? item.active(path) : path === item.to || path.startsWith(item.to + "/");
                const Icon = item.icon;
                return (
                  <Link key={item.to} to={item.to} className={on ? "on" : ""} aria-current={on ? "page" : undefined} onClick={() => setDrawer(false)}>
                    <Icon size={15} strokeWidth={1.75} />
                    <span>{item.label}</span>
                  </Link>
                );
              })}
            </nav>
            <RunList runs={runs.data ?? []} path={path} onPick={() => setDrawer(false)} />
          </aside>
          {drawer && <div className="drawer-back" onClick={() => setDrawer(false)} />}
          <main className="main">
            <div className="crumbbar">
              <Crumbs project={project} here={here} />
            </div>
            <AnimatePresence mode="wait" initial={false}>
              <motion.div
                key={path}
                className="route"
                initial={motionOn ? { opacity: 0, y: 4 } : false}
                animate={{ opacity: 1, y: 0 }}
                exit={motionOn ? { opacity: 0 } : undefined}
                transition={{ duration: 0.16, ease: bezier.out }}
              >
                <Page path={path} />
              </motion.div>
            </AnimatePresence>
          </main>
          {palette && <Palette state={palette} onClose={() => setPalette(null)} />}
          <ContextMenu />
          <Toasts />
        </div>
      </RunsContext.Provider>
    </MotionConfig>
  );
}
