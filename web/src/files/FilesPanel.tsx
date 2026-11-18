// The files panel: the project tree beside a tabbed viewer. Find in the file (⌘F), go to
// a line (⌘G), search every file (⌘⇧F), and for a run, show the run's change to the open
// file as a diff (D). Open tabs, the active one and the split are remembered.

import { AnimatePresence, motion } from "motion/react";
import { ChevronDown, ChevronUp, FileDiff as DiffIcon, ExternalLink, Search, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Group, Panel, Separator, useDefaultLayout } from "react-resizable-panels";
import { PatchDiff } from "@pierre/diffs/react";
import { api, type GrepHit } from "../api";
import { typing } from "../keys";
import { MOD } from "../desktop";
import { openPathMenu } from "../paths";
import { setPrefs, prefs, useMotion, usePrefs } from "../prefs";
import { useFile, useProjectFiles, useRunDiff, useSettings, prefetchFile } from "../query";
import { bezier, dur } from "../motion";
import { Tip } from "../components/Tip";
import { CodeView, findMatches } from "./CodeView";
import { FileTree } from "./FileTree";

interface Target {
  n: number;
  at: number;
}

/** Split a whole-run patch into one patch per file. */
export function patchesByFile(diff: string): Map<string, { patch: string; mark: string }> {
  const out = new Map<string, { patch: string; mark: string }>();
  const parts = diff.split(/^(?=diff --git )/m);
  for (const part of parts) {
    const head = /^diff --git a\/(.+?) b\/(.+)$/m.exec(part);
    if (!head) continue;
    const mark = /^new file/m.test(part) ? "A" : /^deleted file/m.test(part) ? "D" : "M";
    out.set(head[2]!, { patch: part, mark });
  }
  return out;
}

function relative(path: string, project: string): string {
  if (project && path.startsWith(project + "/")) return path.slice(project.length + 1);
  return path.replace(/^\.\//, "");
}

export function FilesPanel({ run, open: openRequest }: { run?: string; open?: { path: string; line?: number; n: number } | null }) {
  const p = usePrefs();
  const on = useMotion();
  const settings = useSettings();
  const project = settings.data?.project ?? "";
  const files = useProjectFiles();
  const runDiff = useRunDiff(run ?? null);
  const saved = p.openFiles[project] ?? { tabs: [], active: null };
  const [tabs, setTabsState] = useState<string[]>(saved.tabs);
  const [active, setActiveState] = useState<string | null>(saved.active);
  const [target, setTarget] = useState<Record<string, Target>>({});
  const [filter, setFilter] = useState("");
  const [mode, setMode] = useState<"tree" | "search">("tree");
  const [find, setFind] = useState<{ open: boolean; q: string; i: number }>({ open: false, q: "", i: 0 });
  const [goto, setGoto] = useState<string | null>(null);
  const [reveal, setReveal] = useState<{ path: string; n: number } | null>(null);
  const root = useRef<HTMLDivElement>(null);
  const layout = useDefaultLayout({ id: "taskmaster-files", storage: localStorage });
  const findInput = useRef<HTMLInputElement>(null);
  const gotoInput = useRef<HTMLInputElement>(null);

  // Tabs saved for this project come back when the project is known.
  useEffect(() => {
    const s = prefs().openFiles[project];
    if (s) {
      setTabsState(s.tabs);
      setActiveState(s.active);
    }
  }, [project]);

  const persist = useCallback(
    (nextTabs: string[], nextActive: string | null) => {
      setPrefs((x) => ({ openFiles: { ...x.openFiles, [project]: { tabs: nextTabs.slice(-20), active: nextActive } } }));
    },
    [project],
  );

  const openFile = useCallback(
    (raw: string, line?: number) => {
      const path = relative(raw, project);
      setTabsState((all) => {
        const next = all.includes(path) ? all : [...all, path];
        persist(next, path);
        return next;
      });
      setActiveState(path);
      if (line) setTarget((t) => ({ ...t, [path]: { n: line, at: Date.now() } }));
    },
    [project, persist],
  );

  useEffect(() => {
    if (openRequest) openFile(openRequest.path, openRequest.line);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [openRequest?.n]);

  const closeTab = (path: string) => {
    setTabsState((all) => {
      const i = all.indexOf(path);
      const next = all.filter((t) => t !== path);
      const nextActive = active === path ? (next[Math.min(i, next.length - 1)] ?? null) : active;
      setActiveState(nextActive);
      persist(next, nextActive);
      return next;
    });
  };

  const file = useFile(active);
  const changed = useMemo(() => {
    const map = new Map<string, string>();
    if (runDiff.data) for (const [path, v] of patchesByFile(runDiff.data.diff)) map.set(path, v.mark);
    return map;
  }, [runDiff.data]);
  const patch = active && runDiff.data ? patchesByFile(runDiff.data.diff).get(active)?.patch : undefined;
  const showDiff = !!run && p.fileDiff;
  const lines = useMemo(() => (file.data ? file.data.content.split("\n") : []), [file.data]);
  const matchCount = useMemo(() => (find.q ? findMatches(lines, find.q).length : 0), [lines, find.q]);

  // Keys work while focus is anywhere in the panel.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!root.current?.contains(document.activeElement) && document.activeElement !== document.body) return;
      if (!root.current?.offsetParent) return;
      const mod = e.metaKey || e.ctrlKey;
      const k = e.key.toLowerCase();
      if (mod && e.shiftKey && k === "f") {
        e.preventDefault();
        setMode("search");
      } else if (mod && k === "f" && active) {
        e.preventDefault();
        setFind((f) => ({ ...f, open: true }));
        requestAnimationFrame(() => findInput.current?.select());
      } else if (mod && k === "g" && active) {
        e.preventDefault();
        setGoto("");
        requestAnimationFrame(() => gotoInput.current?.focus());
      } else if (!mod && k === "d" && run && !typing(e.target)) {
        e.preventDefault();
        setPrefs({ fileDiff: !prefs().fileDiff });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [active, run]);

  const crumbs = active ? active.split("/") : [];

  return (
    <div className="files" ref={root}>
      <Group orientation="horizontal" id="taskmaster-files" className="files-split" defaultLayout={layout.defaultLayout} onLayoutChanged={layout.onLayoutChanged}>
        <Panel id="tree" defaultSize="32%" minSize="16%" maxSize="60%" className="files-side">
          <div className="files-side-head">
            <div className="seg mini" role="tablist" aria-label="Browse">
              <button type="button" role="tab" aria-selected={mode === "tree"} className={`seg-item ${mode === "tree" ? "on" : ""}`} onClick={() => setMode("tree")}>
                <span className="seg-text">Files</span>
              </button>
              <button type="button" role="tab" aria-selected={mode === "search"} className={`seg-item ${mode === "search" ? "on" : ""}`} onClick={() => setMode("search")}>
                <span className="seg-text">Search</span>
              </button>
            </div>
          </div>
          {mode === "tree" ? (
            <>
              <div className="files-filter">
                <Search size={12} aria-hidden />
                <input value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="Filter files" aria-label="Filter files" spellCheck={false} />
              </div>
              {files.isLoading ? (
                <div className="tree-skel" aria-label="Loading files" role="status">
                  {Array.from({ length: 9 }, (_, i) => (
                    <span key={i} className="skeleton" style={{ width: `${70 - ((i * 13) % 40)}%`, marginLeft: (i % 3) * 12 }} />
                  ))}
                </div>
              ) : (
                <FileTree files={files.data ?? []} active={active} changed={changed} filter={filter} reveal={reveal} onOpen={(f) => openFile(f)} />
              )}
            </>
          ) : (
            <ProjectSearch onOpen={openFile} />
          )}
        </Panel>
        <Separator className="split-handle" />
        <Panel id="viewer" minSize="30%" className="files-main">
          {tabs.length > 0 && (
            <div className="ftabs" role="tablist" aria-label="Open files">
              <AnimatePresence initial={false}>
                {tabs.map((t) => (
                  <motion.div
                    key={t}
                    layout="position"
                    className={`ftab ${t === active ? "on" : ""}`}
                    initial={on ? { opacity: 0, y: 3 } : false}
                    animate={{ opacity: 1, y: 0 }}
                    exit={on ? { opacity: 0, transition: { duration: 0.12 } } : undefined}
                    transition={{ duration: dur.fast, ease: bezier.out, layout: { duration: dur.base, ease: bezier.out } }}
                    onMouseEnter={() => prefetchFile(t)}
                  >
                    <button
                      type="button"
                      role="tab"
                      aria-selected={t === active}
                      className="ftab-name"
                      title={t}
                      onClick={() => {
                        setActiveState(t);
                        persist(tabs, t);
                      }}
                      onAuxClick={(e) => e.button === 1 && closeTab(t)}
                    >
                      {t.split("/").pop()}
                      {changed.has(t) && <span className="tree-mark">{changed.get(t)}</span>}
                    </button>
                    <button type="button" className="ftab-x" aria-label={`Close ${t}`} onClick={() => closeTab(t)}>
                      <X size={11} />
                    </button>
                  </motion.div>
                ))}
              </AnimatePresence>
            </div>
          )}
          {active ? (
            <>
              <div className="fbar">
                <nav className="fcrumbs" aria-label="File path">
                  {crumbs.map((part, i) => {
                    const sub = crumbs.slice(0, i + 1).join("/");
                    const last = i === crumbs.length - 1;
                    return (
                      <span key={sub} className={last ? "here" : ""}>
                        {i > 0 && <span className="crumb-sep">/</span>}
                        {last ? (
                          <span>{part}</span>
                        ) : (
                          <button type="button" className="fcrumb" onClick={() => (setMode("tree"), setFilter(""), setReveal({ path: sub, n: Date.now() }))}>
                            {part}
                          </button>
                        )}
                      </span>
                    );
                  })}
                </nav>
                <div className="fbar-actions">
                  {run && (
                    <Tip label={showDiff ? "Show the file" : "Show this run's change"} keys="D">
                      <button
                        type="button"
                        className={`tb-btn ${showDiff ? "on" : ""}`}
                        aria-pressed={showDiff}
                        aria-label="Show the run's change as a diff"
                        onClick={() => setPrefs({ fileDiff: !p.fileDiff })}
                      >
                        <DiffIcon size={14} />
                      </button>
                    </Tip>
                  )}
                  <Tip label="Find in file" keys={`${MOD} F`}>
                    <button type="button" className="tb-btn" aria-label="Find in file" onClick={() => (setFind((f) => ({ ...f, open: true })), requestAnimationFrame(() => findInput.current?.select()))}>
                      <Search size={14} />
                    </button>
                  </Tip>
                  <Tip label="Open in your editor">
                    <button
                      type="button"
                      className="tb-btn"
                      aria-label="Open in your editor"
                      onClick={(e) => {
                        const r = e.currentTarget.getBoundingClientRect();
                        openPathMenu(r.left, r.bottom + 4, file.data?.full ?? active, { raw: active, line: target[active]?.n });
                      }}
                    >
                      <ExternalLink size={14} />
                    </button>
                  </Tip>
                </div>
              </div>
              <AnimatePresence>
                {(find.open || goto !== null) && (
                  <motion.div
                    className="findbar"
                    initial={on ? { opacity: 0, y: -4 } : false}
                    animate={{ opacity: 1, y: 0 }}
                    exit={on ? { opacity: 0, y: -4 } : undefined}
                    transition={{ duration: dur.fast, ease: bezier.out }}
                  >
                    {goto !== null ? (
                      <form
                        className="findbar-row"
                        onSubmit={(e) => {
                          e.preventDefault();
                          const n = Number(goto);
                          if (n >= 1) setTarget((t) => ({ ...t, [active]: { n: Math.min(n, lines.length), at: Date.now() } }));
                          setGoto(null);
                        }}
                      >
                        <span className="findbar-label">Go to line</span>
                        <input
                          ref={gotoInput}
                          autoFocus
                          inputMode="numeric"
                          value={goto}
                          onChange={(e) => setGoto(e.target.value.replace(/\D/g, ""))}
                          onKeyDown={(e) => e.key === "Escape" && (e.preventDefault(), setGoto(null))}
                          placeholder={`1 to ${lines.length}`}
                          aria-label="Line number"
                        />
                      </form>
                    ) : (
                      <div className="findbar-row">
                        <input
                          ref={findInput}
                          autoFocus
                          value={find.q}
                          onChange={(e) => setFind({ open: true, q: e.target.value, i: 0 })}
                          onKeyDown={(e) => {
                            if (e.key === "Escape") {
                              e.preventDefault();
                              setFind({ open: false, q: "", i: 0 });
                            } else if (e.key === "Enter" && matchCount) {
                              e.preventDefault();
                              setFind((f) => ({ ...f, i: (f.i + (e.shiftKey ? matchCount - 1 : 1)) % matchCount }));
                            }
                          }}
                          placeholder="Find in this file"
                          aria-label="Find in this file"
                          spellCheck={false}
                        />
                        <span className="findbar-count">{find.q ? (matchCount ? `${find.i + 1} of ${matchCount}` : "none") : ""}</span>
                        <button type="button" className="icon" aria-label="Previous match" disabled={!matchCount} onClick={() => setFind((f) => ({ ...f, i: (f.i + matchCount - 1) % matchCount }))}>
                          <ChevronUp size={14} />
                        </button>
                        <button type="button" className="icon" aria-label="Next match" disabled={!matchCount} onClick={() => setFind((f) => ({ ...f, i: (f.i + 1) % matchCount }))}>
                          <ChevronDown size={14} />
                        </button>
                        <button type="button" className="icon" aria-label="Close find" onClick={() => setFind({ open: false, q: "", i: 0 })}>
                          <X size={14} />
                        </button>
                      </div>
                    )}
                  </motion.div>
                )}
              </AnimatePresence>
              <div className="fview">
                {showDiff ? (
                  runDiff.isLoading ? (
                    <CodeSkeleton />
                  ) : patch ? (
                    <div className="fdiff">
                      <PatchDiff
                        patch={patch}
                        disableWorkerPool
                        options={{
                          theme: { dark: "min-dark", light: "min-light" },
                          themeType: p.theme,
                          diffStyle: "unified",
                          disableFileHeader: true,
                          overflow: "scroll",
                          unsafeCSS: DIFF_CSS,
                        }}
                      />
                    </div>
                  ) : (
                    <div className="tab-empty">This run did not change {active.split("/").pop()}.</div>
                  )
                ) : file.error ? (
                  <div className="pad error-line">{file.error instanceof Error ? file.error.message : String(file.error)}</div>
                ) : !file.data ? (
                  <CodeSkeleton />
                ) : (
                  <CodeView path={active} content={file.data.content} line={target[active] ?? null} query={find.open ? find.q : ""} current={find.i} />
                )}
              </div>
              {file.data?.truncated && !showDiff && <div className="fnote">Showing the first 512 KiB, read-only.</div>}
            </>
          ) : (
            <div className="files-empty">
              <p>Pick a file from the tree, or press {MOD} K and type its name.</p>
            </div>
          )}
        </Panel>
      </Group>
    </div>
  );
}

const DIFF_CSS = `
  :host, pre, [data-diffs] { background: var(--bg) !important; font-family: var(--mono) !important; font-size: 12px !important; }
  [data-line-type='change-addition'] { background: color-mix(in srgb, var(--good) 10%, transparent) !important; }
  [data-line-type='change-deletion'] { background: color-mix(in srgb, var(--bad) 10%, transparent) !important; }
`;

function CodeSkeleton() {
  return (
    <div className="code-skel" role="status" aria-label="Loading the file">
      {Array.from({ length: 14 }, (_, i) => (
        <span key={i} className="skeleton" style={{ width: `${30 + ((i * 37) % 55)}%`, marginLeft: (i % 4) * 14 }} />
      ))}
    </div>
  );
}

function ProjectSearch({ onOpen }: { onOpen: (path: string, line?: number) => void }) {
  const [q, setQ] = useState("");
  const [hits, setHits] = useState<GrepHit[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [more, setMore] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => input.current?.focus(), []);
  useEffect(() => {
    const text = q.trim();
    if (text.length < 2) {
      setHits(null);
      return;
    }
    setBusy(true);
    const timer = window.setTimeout(() => {
      void api
        .grep(text)
        .then((r) => {
          setHits(r.hits);
          setMore(r.truncated);
        })
        .catch(() => setHits([]))
        .finally(() => setBusy(false));
    }, 160);
    return () => window.clearTimeout(timer);
  }, [q]);
  const grouped = useMemo(() => {
    const map = new Map<string, GrepHit[]>();
    for (const h of hits ?? []) map.set(h.path, [...(map.get(h.path) ?? []), h]);
    return [...map];
  }, [hits]);
  return (
    <div className="psearch">
      <div className="files-filter">
        <Search size={12} aria-hidden />
        <input ref={input} value={q} onChange={(e) => setQ(e.target.value)} placeholder="Search every file" aria-label="Search every file" spellCheck={false} />
      </div>
      <div className="psearch-list" role="list">
        {busy && !hits && (
          <div className="tree-skel" role="status" aria-label="Searching">
            <span className="skeleton" style={{ width: "60%" }} />
            <span className="skeleton" style={{ width: "80%" }} />
          </div>
        )}
        {hits && hits.length === 0 && <div className="tree-empty">Nothing contains “{q.trim()}”.</div>}
        {grouped.map(([path, list]) => (
          <div key={path} className="psearch-file" role="listitem">
            <div className="psearch-path clip" title={path}>
              {path}
            </div>
            {list.slice(0, 20).map((h) => (
              <button key={`${h.line}:${h.column}`} type="button" className="psearch-hit" onClick={() => onOpen(h.path, h.line)} onMouseEnter={() => prefetchFile(h.path)}>
                <span className="psearch-n">{h.line}</span>
                <span className="clip">{h.text.trim()}</span>
              </button>
            ))}
          </div>
        ))}
        {more && <div className="tree-empty">Showing the first 400 lines. Search for more words to narrow it.</div>}
      </div>
    </div>
  );
}
