import { useEffect, useMemo, useState } from "react";
import { AnimatePresence, LayoutGroup, MotionConfig, motion } from "framer-motion";
import { api, setToken, token, tokenFromRedirect, type RunSummary } from "./api";
import { Link, match, navigate, usePath } from "./router";
import { Dictionary } from "./pages/Dictionary";
import { Errorography } from "./pages/Errorography";
import { NewRun } from "./pages/NewRun";
import { ReviewView, Reviews } from "./pages/Reviews";
import { RunView } from "./pages/RunView";
import { Gallery } from "./pages/Gallery";
import { Providers } from "./pages/Providers";
import { Settings } from "./pages/Settings";
import { SignIn } from "./pages/SignIn";
import { ContextMenu, Toasts, openMenu, undoLatest, type Chip } from "./overlay";
import { Palette, type PaletteState } from "./palette";
import { prefs, setPrefs, useMotion, usePrefs } from "./prefs";
import { RunsContext, ordered, toggleFlag, type Group } from "./runs";
import { phaseLabel } from "./format";
import { readFile } from "./composer";
import { useLoad } from "./ui";

const NAV = [
  { to: "/", label: "Gallery", active: (p: string) => p === "/" || p === "/runs" },
  { to: "/runs/new", label: "New run" },
  { to: "/dictionary", label: "Dictionary" },
  { to: "/errorography", label: "Errorography" },
  { to: "/reviews", label: "Reviews" },
  { to: "/providers", label: "Providers" },
  { to: "/settings", label: "Settings" },
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
  if (path === "/settings") return <Settings />;
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

function RunList({ runs, path, onPick }: { runs: RunSummary[]; path: string; onPick: () => void }) {
  const p = usePrefs();
  const list = ordered(runs, p.showArchived);
  let last: Group | null = null;
  return (
    <div className="runlist">
      <LayoutGroup>
        <ul>
          <AnimatePresence initial={false}>
            {list.map(({ run, group }, i) => {
              const head = group !== last ? group : null;
              last = group;
              const on = path === `/runs/${encodeURIComponent(run.id)}` || path === `/runs/${run.id}`;
              const flags = p.runFlags[run.id] ?? {};
              const menu = (x: number, y: number) =>
                openMenu(x, y, [
                  { label: flags.pinned ? "Unpin" : "Pin", run: () => toggleFlag(run.id, "pinned") },
                  { label: flags.settled ? "Unsettle" : "Settle", run: () => toggleFlag(run.id, "settled") },
                  { label: flags.archived ? "Unarchive" : "Archive", run: () => toggleFlag(run.id, "archived") },
                ]);
              return (
                <motion.li key={run.id} layout="position" initial={{ opacity: 0, x: -8 }} animate={{ opacity: 1, x: 0 }} exit={{ opacity: 0, x: -8 }} transition={{ duration: 0.22, ease: [0.22, 1, 0.36, 1] }}>
                  {head && <div className="group-head">{head}</div>}
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
                      <span className="clip">{run.id}</span>
                      <span className="runrow-phase">
                        {i < 9 && <kbd>{i + 1}</kbd>} {phaseLabel(run).toLowerCase()}
                      </span>
                    </Link>
                    <span className="runrow-actions">
                      <button className="icon" title={flags.pinned ? "Unpin" : "Pin"} onClick={() => toggleFlag(run.id, "pinned")}>
                        {flags.pinned ? "unpin" : "pin"}
                      </button>
                      <button className="icon" title={flags.settled ? "Unsettle" : "Settle"} onClick={() => toggleFlag(run.id, "settled")}>
                        {flags.settled ? "unsettle" : "settle"}
                      </button>
                      <button className="icon" title={flags.archived ? "Unarchive" : "Archive"} onClick={() => toggleFlag(run.id, "archived")}>
                        {flags.archived ? "unarchive" : "archive"}
                      </button>
                    </span>
                  </div>
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

  if (!signedIn) return <SignIn />;

  return (
    <MotionConfig reducedMotion={motionOn ? "never" : "always"}>
      <RunsContext.Provider value={runsState}>
        <div className={`shell density-${p.density} ${motionOn ? "" : "no-motion"}`}>
          <header className="topbar">
            <button className="ghost small" onClick={() => setDrawer(!drawer)} aria-label="Menu">
              menu
            </button>
            <Link to="/" className="brand">
              TASKMASTER
            </Link>
            <button className="ghost small" onClick={() => setPalette({ open: true, mode: "all" })}>
              ⌘K
            </button>
          </header>
          <aside className={`side ${drawer ? "open" : ""}`} aria-label="Main">
            <Link to="/" className="brand side-brand">
              TASKMASTER
            </Link>
            <nav className="nav">
              {NAV.map((item) => {
                const on = item.active ? item.active(path) : path === item.to || path.startsWith(item.to + "/");
                return (
                  <Link key={item.to} to={item.to} className={on ? "on" : ""} aria-current={on ? "page" : undefined} onClick={() => setDrawer(false)}>
                    {item.label}
                  </Link>
                );
              })}
            </nav>
            <button className="palette-hint" onClick={() => setPalette({ open: true, mode: "all" })}>
              <span>Search runs and commands</span>
              <kbd>⌘K</kbd>
            </button>
            <RunList runs={runs.data ?? []} path={path} onPick={() => setDrawer(false)} />
          </aside>
          {drawer && <div className="drawer-back" onClick={() => setDrawer(false)} />}
          <main className="main">
            <Page path={path} />
          </main>
          {palette && <Palette state={palette} onClose={() => setPalette(null)} />}
          <ContextMenu />
          <Toasts />
        </div>
      </RunsContext.Provider>
    </MotionConfig>
  );
}
