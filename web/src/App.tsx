import { Suspense, lazy, memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  Archive,
  ArchiveRestore,
  BookOpen,
  Bug,
  ChevronDown,
  Circle,
  CircleAlert,
  CircleCheck,
  CircleDot,
  Compass,
  Cpu,
  FolderTree,
  GitBranch,
  GitPullRequest,
  LayoutGrid,
  Pin,
  PinOff,
  Plug,
  Plus,
  Puzzle,
  Search,
  Settings,
  SquarePen,
  type LucideIcon,
} from "lucide-react";
import { api, setToken, token, tokenFromRedirect, type RunSummary } from "./api";
import { Link, match, navigate, usePath } from "./router";
import { Gallery } from "./pages/Gallery";
import { SignIn } from "./pages/SignIn";
import { ContextMenu, Toasts, emit, openMenu, toast, undoLatest, useOn, type Chip } from "./overlay";
import type { PaletteState } from "./palette";
import { openProject } from "./openProject";
import { prefs, setPrefs, useMotion, usePrefs } from "./prefs";
import { RunsContext, ordered, toggleFlag, type Group } from "./runs";
import { phaseLabel, shortSpan, usd } from "./format";
import { readFile } from "./composer";
import { typing } from "./keys";
import { RunBar } from "./run/Meter";
import { TitleBar, type Crumb } from "./shell/TitleBar";
import { ConnectionBanner } from "./shell/Connection";
import { QuickTerminal } from "./lazy";
import { TipProvider } from "./components/Tip";
import { prefetchRun, useSettings } from "./query";
import { watchRuns } from "./notify";
import { MOD, isDesktop, isMac } from "./desktop";

const PaletteLazy = lazy(() => import("./palette").then((m) => ({ default: m.Palette })));
const ShortcutsLazy = lazy(() => import("./shell/Shortcuts").then((m) => ({ default: m.Shortcuts })));

export { typing };

const NAV: { to: string; label: string; icon: LucideIcon; active?: (p: string) => boolean }[] = [
  { to: "/", label: "Gallery", icon: LayoutGrid, active: (p) => p === "/" || p === "/runs" },
  { to: "/runs/new", label: "New run", icon: Plus },
  { to: "/files", label: "Files", icon: FolderTree },
  { to: "/knowledge", label: "Knowledge", icon: Compass },
  { to: "/dictionary", label: "Dictionary", icon: BookOpen },
  { to: "/errorography", label: "Errorography", icon: Bug },
  { to: "/reviews", label: "Reviews", icon: GitPullRequest },
  { to: "/skills", label: "Skills", icon: Puzzle },
  { to: "/providers", label: "Providers", icon: Cpu },
  { to: "/connect", label: "Connect", icon: Plug },
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

// Every screen but the gallery loads when first opened, so the first load carries only what
// the home screen draws. Once the app is idle the rest is fetched, so later switches do not wait.
const loaders = {
  dictionary: () => import("./pages/Dictionary"),
  errorography: () => import("./pages/Errorography"),
  knowledge: () => import("./pages/Knowledge"),
  newRun: () => import("./pages/NewRun"),
  reviews: () => import("./pages/Reviews"),
  run: () => import("./pages/RunView"),
  providers: () => import("./pages/Providers"),
  skills: () => import("./pages/Skills"),
  settings: () => import("./pages/Settings"),
  connect: () => import("./pages/Connect"),
  files: () => import("./pages/Files"),
};
const Dictionary = lazy(() => loaders.dictionary().then((m) => ({ default: m.Dictionary })));
const Errorography = lazy(() => loaders.errorography().then((m) => ({ default: m.Errorography })));
const Knowledge = lazy(() => loaders.knowledge().then((m) => ({ default: m.Knowledge })));
const NewRun = lazy(() => loaders.newRun().then((m) => ({ default: m.NewRun })));
const Reviews = lazy(() => loaders.reviews().then((m) => ({ default: m.Reviews })));
const ReviewView = lazy(() => loaders.reviews().then((m) => ({ default: m.ReviewView })));
const RunView = lazy(() => loaders.run().then((m) => ({ default: m.RunView })));
const Providers = lazy(() => loaders.providers().then((m) => ({ default: m.Providers })));
const Skills = lazy(() => loaders.skills().then((m) => ({ default: m.Skills })));
const SettingsPage = lazy(() => loaders.settings().then((m) => ({ default: m.Settings })));
const ConnectPage = lazy(() => loaders.connect().then((m) => ({ default: m.Connect })));
const Files = lazy(() => loaders.files().then((m) => ({ default: m.Files })));

function usePrefetchScreens() {
  useEffect(() => {
    const go = () => {
      void loaders.run().catch(() => undefined);
      void loaders.newRun().catch(() => undefined);
    };
    if ("requestIdleCallback" in window) {
      const handle = window.requestIdleCallback(go, { timeout: 3000 });
      return () => window.cancelIdleCallback(handle);
    }
    const handle = globalThis.setTimeout(go, 1500);
    return () => globalThis.clearTimeout(handle);
  }, []);
}

function Page({ path }: { path: string }) {
  return (
    <Suspense fallback={<div className="page" aria-busy="true" />}>
      <Screen path={path} />
    </Suspense>
  );
}

function Screen({ path }: { path: string }) {
  let params: Record<string, string> | null;
  if (path === "/" || path === "/runs" || path === "/pair") return <Gallery />;
  if (path === "/runs/new") return <NewRun />;
  if (path === "/files") return <Files />;
  if (path === "/knowledge") return <Knowledge />;
  if ((params = match(path, "/runs/:id"))) return <RunView key={params.id} id={params.id!} />;
  if (path === "/dictionary") return <Dictionary />;
  if (path === "/errorography") return <Errorography />;
  if (path === "/reviews") return <Reviews />;
  if ((params = match(path, "/reviews/:id"))) return <ReviewView id={params.id!} />;
  if (path === "/providers") return <Providers />;
  if (path === "/skills") return <Skills />;
  if (path === "/settings") return <SettingsPage />;
  if (path === "/connect") return <ConnectPage />;
  return (
    <div className="page">
      <h1>Nothing here</h1>
      <p>
        <Link to="/">Back to the gallery</Link>
      </p>
    </div>
  );
}

function projectName(project: string | undefined): string {
  if (!project) return "Project";
  const parts = project.split(/[/\\]/).filter(Boolean);
  return parts[parts.length - 1] || project;
}

const PAGE_NAMES: Record<string, string> = {
  "/runs/new": "New run",
  "/files": "Files",
  "/knowledge": "Knowledge Hub",
  "/dictionary": "Dictionary",
  "/errorography": "Errorography",
  "/reviews": "Reviews",
  "/providers": "Providers",
  "/skills": "Skills",
  "/settings": "Settings",
  "/connect": "Connect",
};

/** Project, then the run or page: parents dimmed, the current one in full ink. */
function crumbsFor(path: string, project: string): Crumb[] {
  const home: Crumb = { label: project, to: "/" };
  if (path === "/" || path === "/runs") return [home, { label: "Gallery" }];
  if (PAGE_NAMES[path]) return [home, { label: PAGE_NAMES[path]! }];
  if (path.startsWith("/runs/")) return [home, { label: "Runs", to: "/" }, { label: decodeURIComponent(path.slice("/runs/".length)) }];
  if (path.startsWith("/reviews/")) return [home, { label: "Reviews", to: "/reviews" }, { label: decodeURIComponent(path.slice("/reviews/".length)) }];
  return [home, { label: "Not found" }];
}

/** First and last ledger timestamps, so Working can show elapsed time and Settled a relative time. */
function useClocks(ids: string[], runs: RunSummary[] = []) {
  const [clocks, setClocks] = useState<Record<string, { start: number; end: number }>>({});
  const fetched = useRef(new Set<string>());
  const stable = useRef<Record<string, { start: number; end: number }>>({});
  // A daemon that sends the timestamps with the list needs no ledger fetch per run.
  const listed = useMemo(() => {
    const out: Record<string, { start: number; end: number }> = {};
    for (const run of runs) {
      if (!("started_at" in run)) continue;
      const start = run.started_at ? Date.parse(run.started_at) : NaN;
      const end = run.last_at ? Date.parse(run.last_at) : NaN;
      if (!Number.isNaN(start) && !Number.isNaN(end)) {
        const prev = stable.current[run.id];
        if (prev && prev.start === start && prev.end === end) {
          out[run.id] = prev;
        } else {
          const next = { start, end };
          stable.current[run.id] = next;
          out[run.id] = next;
        }
      }
      fetched.current.add(run.id);
    }
    return out;
  }, [runs]);
  const key = ids.join("|");
  useEffect(() => {
    let cancel = false;
    for (const id of key ? key.split("|") : []) {
      if (fetched.current.has(id)) continue;
      fetched.current.add(id);
      api
        .ledger(id)
        .then((ledger) => {
          const stamps = ledger.entries.map((e) => Date.parse(e.at)).filter((n) => !Number.isNaN(n));
          if (cancel || !stamps.length) return;
          setClocks((c) => (c[id] ? c : { ...c, [id]: { start: Math.min(...stamps), end: Math.max(...stamps) } }));
        })
        .catch(() => {
          fetched.current.delete(id);
        });
    }
    return () => {
      cancel = true;
    };
  }, [key]);
  return useMemo(() => ({ ...clocks, ...listed }), [clocks, listed]);
}

function RunMark({ group }: { group: Group }) {
  if (group === "Working") return <CircleDot size={13} />;
  if (group === "Needs approval") return <CircleAlert size={13} />;
  if (group === "Pinned") return <Pin size={13} />;
  if (group === "Archived") return <Archive size={13} />;
  return <Circle size={13} />;
}

/** A long run list is drawn a screenful at a time. */
const VIRTUAL_AFTER = 60;

/** A run's clock in the sidebar. Only this text ticks, so the list around it stays still. */
function Stamp({ run, group, clock }: { run: RunSummary; group: Group; clock?: { start: number; end: number } }) {
  const working = group === "Working";
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!working) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [working]);
  const when = working && clock ? shortSpan(now - clock.start) : !working && clock ? shortSpan(now - clock.end) : null;
  return <span>{working ? (when ? `Working ${when}` : phaseLabel(run).toLowerCase()) : (when ?? phaseLabel(run).toLowerCase())}</span>;
}

const RunRow = memo(function RunRow({
  run,
  group,
  i,
  on,
  clock,
  onPick,
}: {
  run: RunSummary;
  group: Group;
  i: number;
  on: boolean;
  clock?: { start: number; end: number };
  onPick: () => void;
}) {
  const p = usePrefs();
  const flags = p.runFlags[run.id] ?? {};
  const menu = (x: number, y: number) =>
    openMenu(x, y, [
      { label: flags.pinned ? "Unpin" : "Pin", run: () => toggleFlag(run.id, "pinned") },
      { label: flags.settled ? "Unsettle" : "Settle", run: () => toggleFlag(run.id, "settled") },
      { label: flags.archived ? "Unarchive" : "Archive", run: () => toggleFlag(run.id, "archived") },
    ]);
  return (
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
      onMouseEnter={() => prefetchRun(run.id)}
      onFocus={() => prefetchRun(run.id)}
    >
      <Link to={`/runs/${encodeURIComponent(run.id)}`} className="runrow-link" onClick={onPick} aria-current={on ? "page" : undefined}>
        <span
          className={`run-ico ${group === "Working" ? "working" : ""} ${group === "Needs approval" ? "ask" : ""} ${
            run.phase === "done" ? "done" : run.phase === "failed" ? "failed" : ""
          }`}
          aria-hidden
        >
          <RunMark group={group} />
        </span>
        <span className="run-copy">
          <span className="clip run-title">{run.id}</span>
          {run.branch && !(run.merged && run.merge_commit) && (
            <span className="run-branch clip">
              <GitBranch size={11} />
              {run.branch}
            </span>
          )}
        </span>
        <span className={`runrow-phase ${group === "Working" ? "elapsed" : ""}`}>
          {run.spent.cost_usd >= 0.01 && <span className="runrow-cost mono">{usd(run.spent.cost_usd)}</span>}
          {i < 9 && <kbd>{i + 1}</kbd>}
          <Stamp run={run} group={group} clock={clock} />
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
  );
});

const RunList = memo(function RunList({ runs, path, onPick }: { runs: RunSummary[]; path: string; onPick: () => void }) {
  const p = usePrefs();
  const list = useMemo(() => ordered(runs, p.showArchived), [runs, p.showArchived]);
  const clockIds = useMemo(() => {
    if (list.length > VIRTUAL_AFTER) return [];
    const active = list.filter((x) => x.group === "Working").map((x) => x.run.id);
    const recent = list.slice(0, 12).map((x) => x.run.id);
    return [...new Set([...active, ...recent])];
  }, [list]);
  const clocks = useClocks(clockIds, runs);
  const [folded, setFolded] = useState<Partial<Record<Group, boolean>>>({});
  const scroller = useRef<HTMLDivElement>(null);
  const counts = new Map<Group, number>();
  for (const item of list) counts.set(item.group, (counts.get(item.group) ?? 0) + 1);

  // Rows and group heads in one flat list, folded groups left out.
  type Item = { kind: "head"; group: Group } | { kind: "run"; run: RunSummary; group: Group; i: number };
  const items: Item[] = [];
  let last: Group | null = null;
  list.forEach(({ run, group }, i) => {
    if (group !== last) items.push({ kind: "head", group });
    last = group;
    if (!folded[group]) items.push({ kind: "run", run, group, i });
  });
  const isOn = (run: RunSummary) => path === `/runs/${encodeURIComponent(run.id)}` || path === `/runs/${run.id}`;
  const head = (group: Group) => (
    <button type="button" className="group-head" aria-expanded={!folded[group]} onClick={() => setFolded((f) => ({ ...f, [group]: !f[group] }))}>
      <span>{group}</span>
      <span className="group-count">
        {counts.get(group)}
        <ChevronDown size={12} className={folded[group] ? "chev closed" : "chev"} />
      </span>
    </button>
  );

  const virtual = useVirtualizer({
    count: items.length,
    getScrollElement: () => scroller.current,
    estimateSize: (i) => (items[i]?.kind === "head" ? 30 : 40),
    overscan: 10,
    enabled: list.length > VIRTUAL_AFTER,
  });

  return (
    <div className="runlist" ref={scroller}>
      {list.length === 0 ? (
        <div className="runlist-empty">Runs you start show up here.</div>
      ) : list.length > VIRTUAL_AFTER ? (
        <div style={{ height: virtual.getTotalSize(), position: "relative" }}>
          {virtual.getVirtualItems().map((v) => {
            const item = items[v.index]!;
            return (
              <div key={item.kind === "head" ? `h:${item.group}` : item.run.id} ref={virtual.measureElement} data-index={v.index} className="vrow" style={{ transform: `translateY(${v.start}px)` }}>
                {item.kind === "head" ? head(item.group) : <RunRow run={item.run} group={item.group} i={item.i} on={isOn(item.run)} clock={clocks[item.run.id]} onPick={onPick} />}
              </div>
            );
          })}
        </div>
      ) : (
        <ul>
          {items.map((item) => (
            <li key={item.kind === "head" ? `h:${item.group}` : item.run.id}>
              {item.kind === "head" ? head(item.group) : <RunRow run={item.run} group={item.group} i={item.i} on={isOn(item.run)} clock={clocks[item.run.id]} onPick={onPick} />}
            </li>
          ))}
        </ul>
      )}
      <label className="check small">
        <input type="checkbox" checked={p.showArchived} onChange={(e) => setPrefs({ showArchived: e.target.checked })} />
        Show archived
      </label>
    </div>
  );
});

/** Open the project this browser had open last, once, if the daemon started on another. */
function useLastProject(project: string | undefined) {
  const done = useRef(false);
  useEffect(() => {
    if (!project || done.current) return;
    done.current = true;
    const last = prefs().lastProject;
    if (last && last !== project) {
      void api
        .projects()
        .then((r) => {
          const hit = r.projects.find((x) => x.path === last);
          if (hit) openProject(hit.path, hit.name);
          else setPrefs({ lastProject: project });
        })
        .catch(() => undefined);
    } else setPrefs({ lastProject: project });
  }, [project]);
}

/**
 * Poll `/api/runs?since=<seq>` with structural sharing by run id: unchanged rows keep their exact
 * object reference so memoized sidebar rows never re-render while another run is active (P9).
 */
function useRuns(signedIn: boolean) {
  const [data, setData] = useState<RunSummary[]>([]);
  const seqRef = useRef<number | undefined>(undefined);
  const byId = useRef<Map<string, { raw: string; run: RunSummary }>>(new Map());
  const orderRef = useRef<string[]>([]);

  const fetchRuns = useCallback(
    async (full = false) => {
      if (!signedIn) {
        seqRef.current = undefined;
        byId.current.clear();
        orderRef.current = [];
        setData((prev) => (prev.length === 0 ? prev : []));
        return;
      }
      try {
        const res = await api.runsDelta(full ? undefined : seqRef.current);
        if (typeof res.seq === "number") seqRef.current = res.seq;
        let changed = false;

        if (res.delta && res.all_ids) {
          for (const run of res.runs) {
            const raw = JSON.stringify(run);
            const prev = byId.current.get(run.id);
            if (!prev || prev.raw !== raw) {
              byId.current.set(run.id, { raw, run });
              changed = true;
            }
          }
          const alive = new Set(res.all_ids);
          for (const id of [...byId.current.keys()]) {
            if (!alive.has(id)) {
              byId.current.delete(id);
              changed = true;
            }
          }
          // If an id in all_ids isn't cached yet (e.g. switched project), fall back to a full sync.
          const missing = res.all_ids.some((id) => !byId.current.has(id));
          if (missing) {
            seqRef.current = undefined;
            await fetchRuns(true);
            return;
          }
          const sameOrder =
            res.all_ids.length === orderRef.current.length &&
            res.all_ids.every((id, i) => orderRef.current[i] === id);
          if (!changed && sameOrder) return;
          orderRef.current = res.all_ids;
          setData(res.all_ids.map((id) => byId.current.get(id)!.run));
        } else {
          const nextIds: string[] = [];
          const nextMap = new Map<string, { raw: string; run: RunSummary }>();
          const nextList: RunSummary[] = [];
          for (const run of res.runs) {
            nextIds.push(run.id);
            const raw = JSON.stringify(run);
            const prev = byId.current.get(run.id);
            if (prev && prev.raw === raw) {
              nextMap.set(run.id, prev);
              nextList.push(prev.run);
            } else {
              const entry = { raw, run };
              nextMap.set(run.id, entry);
              nextList.push(run);
              changed = true;
            }
          }
          const sameOrder =
            nextIds.length === orderRef.current.length &&
            nextIds.every((id, i) => orderRef.current[i] === id);
          byId.current = nextMap;
          if (!changed && sameOrder) return;
          orderRef.current = nextIds;
          setData(nextList);
        }
      } catch {
        // Connection banner handles offline state; next tick retries.
      }
    },
    [signedIn],
  );

  useEffect(() => {
    seqRef.current = undefined;
    void fetchRuns(true);
    if (!signedIn) return;
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") void fetchRuns(false);
    }, 3000);
    return () => window.clearInterval(timer);
  }, [signedIn, fetchRuns]);

  const reload = useCallback(() => fetchRuns(true), [fetchRuns]);
  return { data, reload };
}

export function App() {
  const path = usePath();
  const signedIn = useSignedIn();
  const p = usePrefs();
  const motionOn = useMotion();
  const [palette, setPalette] = useState<PaletteState | null>(null);
  const [drawer, setDrawer] = useState(false);
  const closeDrawer = useCallback(() => setDrawer(false), []);
  const [term, setTerm] = useState(false);
  const [termMounted, setTermMounted] = useState(false);
  const [keys, setKeys] = useState(false);
  const runs = useRuns(signedIn);
  const settings = useSettings();
  usePrefetchScreens();

  useEffect(() => {
    if (signedIn && path === "/signed-in") navigate("/", true);
    // A pairing link opened in a browser that is already signed in: leave the link unused,
    // so another device can still take it.
    if (signedIn && path === "/pair") {
      const had = new URLSearchParams(window.location.hash.slice(1)).has("code");
      // After this render's effects, so the router is already listening on a first load.
      window.setTimeout(() => navigate("/", true), 0);
      if (had) toast("Already paired", "This browser is signed in. The link was not used, so another device can still open it.", 6000);
    }
  }, [signedIn, path]);

  useEffect(() => {
    if (runs.data) watchRuns(runs.data);
  }, [runs.data]);

  useLastProject(signedIn ? settings.data?.project : undefined);

  const toggleTerm = useCallback(() => {
    setTermMounted(true);
    setTerm((t) => !t);
  }, []);
  useOn("terminal-toggle", toggleTerm);
  useOn("shortcuts", () => setKeys(true));

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      const k = e.key.toLowerCase();
      if (mod && !e.shiftKey && k === "z") {
        // An Undo on screen wins, even inside the composer.
        if (undoLatest()) {
          e.preventDefault();
          e.stopPropagation();
        }
        return;
      }
      if (mod && k === "k") {
        e.preventDefault();
        setPalette((s) => (s ? null : { open: true, mode: "all" }));
        return;
      }
      // Command-J on a Mac, Ctrl-J elsewhere; it works from inside the terminal too.
      if (mod && !e.shiftKey && k === "j") {
        e.preventDefault();
        toggleTerm();
        return;
      }
      if (mod && k === ",") {
        e.preventDefault();
        navigate("/settings");
        return;
      }
      if (mod && e.shiftKey && k === "e") {
        e.preventDefault();
        if (path.startsWith("/runs/") && path !== "/runs/new") emit("files-toggle", null);
        else navigate("/files");
        return;
      }
      if (typing(e.target)) return;
      if (e.key === "/" && !mod) {
        e.preventDefault();
        setPalette({ open: true, mode: "slash" });
        return;
      }
      if (e.key === "?" && !mod) {
        e.preventDefault();
        setKeys(true);
        return;
      }
      if (k === "n" && !mod && !e.altKey && !e.shiftKey) {
        e.preventDefault();
        navigate("/runs/new");
        return;
      }
      const list = ordered(runs.data ?? [], prefs().showArchived);
      if (e.altKey && !mod && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
        const here = list.findIndex((x) => path === `/runs/${encodeURIComponent(x.run.id)}`);
        const next = list[here < 0 ? 0 : Math.max(0, Math.min(list.length - 1, here + (e.key === "ArrowDown" ? 1 : -1)))]?.run;
        if (next) {
          e.preventDefault();
          navigate(`/runs/${encodeURIComponent(next.id)}`);
        }
        return;
      }
      if (mod && /^[1-9]$/.test(e.key)) {
        const run = list[Number(e.key) - 1]?.run;
        if (run) {
          e.preventDefault();
          navigate(`/runs/${encodeURIComponent(run.id)}`);
        }
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [runs.data, path, toggleTerm]);

  const runsState = useMemo(() => ({ runs: runs.data ?? [], reload: () => void runs.reload() }), [runs.data, runs.reload]);
  const project = projectName(settings.data?.project);
  const crumbs = crumbsFor(path, project);

  if (!signedIn) return <SignIn />;

  return (
    <TipProvider>
      <RunsContext.Provider value={runsState}>
        <div className={`shell theme-${p.theme} density-${p.density} ${motionOn ? "" : "no-motion"} ${term ? "term-open" : ""} ${isDesktop() && isMac ? "desktop-mac" : ""}`}>
          <TitleBar
            crumbs={crumbs}
            onPalette={() => setPalette({ open: true, mode: "all" })}
            onTerminal={toggleTerm}
            onMenu={() => setDrawer(!drawer)}
            terminalOpen={term}
            extra={/^\/runs\/(?!new$)[^/]+/.test(path) ? <RunBar /> : null}
          />
          <aside className={`side ${drawer ? "open" : ""}`} aria-label="Main">
            <Link to="/" className="brand side-brand" onClick={() => setDrawer(false)}>
              <img src="/icon.png" alt="" width={16} height={16} />
              <span>Taskmaster</span>
            </Link>
            <div className="side-search">
              <button className="palette-hint" onClick={() => setPalette({ open: true, mode: "all" })}>
                <Search size={14} />
                <span>Search</span>
                <kbd>{MOD} K</kbd>
              </button>
              <Link to="/runs/new" className="side-new" aria-label="New run" title="New run (N)" onClick={() => setDrawer(false)}>
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
            <RunList runs={runs.data} path={path} onPick={closeDrawer} />
          </aside>
          {drawer && <div className="drawer-back" onClick={() => setDrawer(false)} />}
          <main className="main">
            <ConnectionBanner />
            <div key={path} className="route">
              <Page path={path} />
            </div>
          </main>
          {termMounted && <QuickTerminal open={term} onClose={() => setTerm(false)} />}
          {palette && (
            <Suspense fallback={null}>
              <PaletteLazy state={palette} onClose={() => setPalette(null)} />
            </Suspense>
          )}
          {keys && (
            <Suspense fallback={null}>
              <ShortcutsLazy open={keys} onOpenChange={setKeys} />
            </Suspense>
          )}
          <ContextMenu />
          <Toasts theme={p.theme} />
        </div>
      </RunsContext.Provider>
    </TipProvider>
  );
}
