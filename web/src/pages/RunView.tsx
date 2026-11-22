// A run is a clean T3 Code-style activity thread: live headline on top, your prompt,
// Master with its Overall Plan, and each parallel Worker with click-to-inspect activity
// and accessed files. A toggleable side panel holds files, diff, preview, and terminal.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Activity, AlertCircle, ArrowDown, Check, FileDiff, GitBranch, GitMerge, Loader2, MessageSquareText, Network, PanelRight } from "lucide-react";
import { Group, Panel, Separator, useDefaultLayout, usePanelRef } from "react-resizable-panels";
import { api, type DiffStats, type ReviewJob, type RunDetail } from "../api";
import { takeAttachments } from "../App";
import { setRunContext } from "../commands";
import { CancelControl, type ActiveWorkerItem } from "../components/CancelControl";
import { Composer, messageText, type ComposerHandle } from "../composer";
import { phaseLabel, usd } from "../format";
import { FilesPanel } from "../lazy";
import { emit, Sheet, toast, undoable, useOn, type Chip } from "../overlay";
import { ExternalLink, Rich, looksLikePath, resolvedPath } from "../paths";
import { prefs, rememberRun, setPrefs } from "../prefs";
import { useSettings } from "../query";
import { navigate } from "../router";
import { setRunChrome } from "../shell/runChrome";
import { Problem } from "../ui";
import { ApprovalDock } from "../run/ApprovalDock";
import { DiffTab, type Scope } from "../run/Diff";
import { Flow, FlowSkeleton, fileTarget } from "../run/Flow";
import { LimitNotice } from "../run/LimitNotice";
import { LivePreview } from "../run/LivePreview";
import { MissionLens } from "../run/MissionLens";
import { ReviewLens } from "../run/ReviewLens";
import { TelemetryLens } from "../run/TelemetryLens";
import { useRun, useSurfaces, useTurns } from "../run/model";
import { PastTurn } from "../run/Turns";
import { ModePicker, useRunMode } from "../components/ModePicker";
import { BrowserTab, TerminalTab } from "../run/Tabs";
import { useToolApprovals } from "../run/ToolApprovals";
import { WebSources } from "../run/WebSources";
import { morphName } from "./Gallery";

type Lens = "mission" | "stream" | "review" | "telemetry";
type StreamFilter = "all" | "active" | "edits" | "issues";

const PANELS = ["files", "diff", "preview", "terminal"] as const;
const TAB_LABEL: Record<(typeof PANELS)[number], string> = {
  files: "Files",
  diff: "Diff",
  preview: "Preview",
  terminal: "Terminal",
};

function panelTab(saved: string | undefined): (typeof PANELS)[number] {
  if (saved === "browser" || saved === "search") return "preview";
  if (saved && (PANELS as readonly string[]).includes(saved)) return saved as (typeof PANELS)[number];
  return "files";
}

function useNarrow(): boolean {
  const [narrow, setNarrow] = useState(() => window.matchMedia("(max-width: 900px)").matches);
  useEffect(() => {
    const q = window.matchMedia("(max-width: 900px)");
    const on = () => setNarrow(q.matches);
    q.addEventListener("change", on);
    return () => q.removeEventListener("change", on);
  }, []);
  return narrow;
}

function typingTarget(target: EventTarget | null): boolean {
  const el = target instanceof Element ? target : null;
  return !!el?.closest("input, textarea, select, [contenteditable='true'], .xterm, [role='dialog']");
}

function noteTarget(chips: Chip[]): string {
  for (const c of chips) {
    if (c.kind === "agent") {
      if (c.value.startsWith("worker:")) return `page:${c.value.slice(7)}`;
      if (["master", "workers", "reviewer", "verifier"].includes(c.value)) return c.value;
    }
    if (c.kind === "page") return `page:${c.value}`;
  }
  return "master";
}

function noteTargetLabel(to: string): string {
  if (to === "master") return "the master";
  if (to === "workers") return "all workers";
  if (to.startsWith("page:")) return `worker ${to.slice(5)}`;
  if (to.startsWith("unit:")) return `worker ${to.slice(5)}`;
  return to;
}

function ApprovalAsk({
  id,
  detail,
  composer,
  onDone,
  onOptimistic,
}: {
  id: string;
  detail: RunDetail;
  composer: React.RefObject<ComposerHandle | null>;
  onDone: () => void;
  onOptimistic: (change: Partial<RunDetail>) => void;
}) {
  const [reply, setReply] = useState("");
  const [busy, setBusy] = useState(false);
  const plan = detail.plan;
  const approve = async () => {
    setBusy(true);
    try {
      const draft = composer.current && !composer.current.isEmpty() ? messageText(composer.current.read()) : undefined;
      onOptimistic({ phase: "building", active: true });
      await api.approve(id, draft).catch((err) => {
        onOptimistic({ phase: detail.phase, active: detail.active });
        throw err;
      });
      composer.current?.clear();
      onDone();
    } catch (err) {
      toast(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };
  const reject = async () => {
    setBusy(true);
    try {
      const prompt = reply.trim() ? `${detail.prompt ?? ""}\n\nThe person rejected the last plan: ${reply.trim()}`.trim() : undefined;
      await api.reject(id, prompt);
      setReply("");
      onDone();
    } catch (err) {
      toast(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="flow-ask" data-testid="approval">
      <p className="flow-ask-line">
        <span>The master asks to build this plan.</span>
        <span className="flow-by">{plan ? `${plan.pages.length} pages` : "a plan"}</span>
      </p>
      <textarea rows={2} value={reply} placeholder="Write a note to plan again, or approve" onChange={(e) => setReply(e.target.value)} />
      <div className="flow-actions">
        <button type="button" disabled={busy} onClick={() => void reject()}>
          {reply.trim() ? "Reject with reply" : "Reject"}
        </button>
        <button type="button" className="primary" disabled={busy} onClick={() => void approve()}>
          Approve
        </button>
      </div>
    </div>
  );
}

function ReviewAsk({ id }: { id: string }) {
  const [job, setJob] = useState<ReviewJob | null>(null);
  const [busy, setBusy] = useState(false);
  const load = useCallback(async (reviewId: string) => {
    const next = await api.review(reviewId);
    setJob(next);
    return next;
  }, []);
  useEffect(() => {
    void api
      .reviews()
      .then((all) => all.find((r) => r.run === id))
      .then((last) => (last ? load(last.id) : undefined))
      .catch(() => undefined);
  }, [id, load]);
  useEffect(() => {
    if (job?.status !== "running") return;
    const t = setInterval(() => void load(job.id).catch(() => undefined), 1500);
    return () => clearInterval(t);
  }, [job?.id, job?.status, load]);
  const start = async () => {
    setBusy(true);
    try {
      const r = await api.reviewRun(id);
      await load(r.id);
    } catch (err) {
      toast("The review did not start", err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };
  const confirmed = job?.report?.findings.filter((f) => f.outcome === "confirmed") ?? [];
  const running = job?.status === "running";
  return (
    <section className="flow-end" data-testid="review-card" aria-label="Code review">
      <header className="flow-role">
        <h2>Review</h2>
        <span className="flow-state">{running ? "reviewing" : job?.status === "done" ? "done" : job?.status === "failed" ? "failed" : "waiting"}</span>
      </header>
      <p className="flow-life">{running ? "One reviewer is reading the whole diff." : "On demand. One reviewer reads the diff and reports confirmed problems."}</p>
      <div className="flow-actions">
        {running ? (
          <button type="button" className="small" data-testid="review-cancel" onClick={() => void api.cancelReview(job.id).then(() => load(job.id))}>
            Cancel review
          </button>
        ) : (
          <button type="button" className="small" data-testid="review-button" disabled={busy} onClick={() => void start()}>
            {job ? "Review again" : "Review"}
          </button>
        )}
      </div>
      {running && (
        <span className="sr-only" data-testid="review-running">
          Review in progress…
        </span>
      )}
      {job?.status === "failed" && (
        <p className="flow-fail" data-testid="review-error">
          Review failed: {job.error}
        </p>
      )}
      {job?.status === "done" && (
        <div data-testid="review-result">
          <p>{confirmed.length === 0 ? "No confirmed problems." : `${confirmed.length} confirmed problem${confirmed.length === 1 ? "" : "s"}.`}</p>
          {confirmed.length > 0 && (
            <ul className="flow-list">
              {confirmed.map((f, i) => (
                <li key={i}>
                  <span className="mono">
                    {f.file}:{f.line}
                  </span>{" "}
                  {f.failure}
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </section>
  );
}

function EndLine({ detail, onDiff, onRetry }: { detail: RunDetail; onDiff: () => void; onRetry: () => void }) {
  const ok = detail.phase === "done";
  const passed = detail.pages.filter((p) => p.state === "passed").length;
  const word = detail.cancelled ? "Cancelled" : ok ? (detail.merged && detail.merge_commit ? `Merged into ${detail.base || "the project"}` : "Done") : "Failed";
  return (
    <section className="flow-end" data-testid="done-summary" aria-label="Run result">
      <header className="flow-role">
        <h2>{word}</h2>
        <span className="flow-state">
          {passed} of {detail.pages.length} page{detail.pages.length === 1 ? "" : "s"} passed
        </span>
      </header>
      {detail.limit && detail.error ? (
        <LimitNotice id={detail.id} limit={detail.limit} message={detail.error} onDone={onRetry} />
      ) : (
        detail.error && (
          <p className="flow-fail">
            <Rich text={detail.error} />
          </p>
        )
      )}
      <p className="flow-life">
        {detail.pages.map((p) => `${p.id} ${p.state}`).join("  ·  ")}
      </p>
      <div className="flow-actions">
        <button type="button" className="small" onClick={onDiff}>
          View diff
        </button>
      </div>
    </section>
  );
}

export function RunView({ id }: { id: string }) {
  const data = useRun(id);
  const { detail, entries, ledger, notes } = data;
  const narrow = useNarrow();
  const composer = useRef<ComposerHandle>(null);
  const runRef = useRef<HTMLDivElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const [pinned, setPinned] = useState(true);
  const settings = useSettings();
  const toolApprovals = useToolApprovals(id, detail?.pending_approvals, data.entries, () => void data.reload());
  const gh = useLoadGh();
  const [lens, setLens] = useState<Lens>("stream");
  const [streamFilter, setStreamFilter] = useState<StreamFilter>("all");
  const [focus, setFocus] = useState<string | null>(null);
  const [blown, setBlown] = useState<Set<string>>(() => new Set());
  const [sheet, setSheet] = useState(false);
  const [cite, setCite] = useState<{ x: number; y: number; text: string } | null>(null);
  const [citeNote, setCiteNote] = useState("");
  const [pr, setPr] = useState<string | null>(null);
  const [panelOpen, setPanelOpenState] = useState(() => prefs().runPanel);
  const setPanelOpen = useCallback((open: boolean) => {
    setPanelOpenState(open);
    setPrefs({ runPanel: open });
  }, []);
  const [removed, setRemoved] = useState<Set<string>>(() => new Set());
  const [tab, setTabState] = useState<(typeof PANELS)[number]>(() => panelTab(prefs().panelTab[id]));
  const [fileOpen, setFileOpen] = useState<{ path: string; line?: number; n: number } | null>(null);
  const splitPanel = usePanelRef();
  const split = useDefaultLayout({ id: "taskmaster-run", storage: localStorage });
  const [diffRequest, setDiffRequest] = useState<{ scope: Scope; page?: string; attempt?: number; n: number } | null>(null);
  const [diffStats, setDiffStats] = useState<DiffStats | null>(null);
  const setTab = useCallback(
    (t: string) => {
      const next = panelTab(t);
      setTabState(next);
      setPrefs((p) => ({ panelTab: { ...p.panelTab, [id]: next } }));
    },
    [id],
  );

  const shown = entries;
  const ledgerEntries = useMemo(() => ledger?.entries ?? [], [ledger]);
  // Each note on a finished run is a turn of its own; only the latest one is live.
  const turns = useTurns(shown, data.entriesVersion, data.events, detail);
  const current = turns[turns.length - 1]!;
  const turnLedger = useMemo(() => (current.at ? ledgerEntries.filter((e) => e.at >= current.at!) : ledgerEntries), [ledgerEntries, current.at]);
  const list = useSurfaces(detail, shown, turnLedger, data.entriesVersion, current.start);
  const [runMode, setRunMode] = useRunMode();
  const filteredList = useMemo(() => {
    if (streamFilter === "all") return list;
    if (streamFilter === "active") return list.filter((s) => s.state === "working" || s.id === "master");
    if (streamFilter === "edits") return list.filter((s) => s.summary.editedFiles.length > 0 || s.id === "master");
    if (streamFilter === "issues") return list.filter((s) => s.state === "failed" || !!s.summary.error || s.id === "master");
    return list;
  }, [list, streamFilter]);
  const activeWorkers = useMemo<ActiveWorkerItem[]>(
    () =>
      list
        .filter((s) => s.state === "working" && s.page)
        .map((s) => ({
          id: s.id,
          page: s.page!,
          label: s.title || s.page!,
          headline: s.summary.headline || undefined,
        })),
    [list],
  );
  const pages = detail?.plan?.pages.map((p) => p.id) ?? detail?.pages.map((p) => p.id) ?? [];
  const phase = detail?.phase;
  const pageStatesKey = detail?.pages.map((p) => `${p.id}:${p.state}:${p.repairs}`).join(",") ?? "";

  useEffect(() => {
    rememberRun(id);
    setPinned(true);
  }, [id]);

  // Smart auto-scroll: stay pinned to the bottom during active runs until the user scrolls up.
  useEffect(() => {
    if (!detail?.active || !pinned) return;
    const el = scrollRef.current;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
  }, [detail?.active, pinned, data.entriesVersion, list.length]);

  useEffect(() => {
    if (!detail?.branch) {
      setDiffStats(null);
      return;
    }
    let live = true;
    void api
      .diffStats(id)
      .then((stats) => {
        if (live) setDiffStats(stats);
      })
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, [id, detail?.branch, phase, pageStatesKey]);

  const openPageDiff = useCallback((page: string) => {
    emit("diff", { scope: "page", page });
  }, []);

  const restartWorker = useCallback(
    async (page: string) => {
      try {
        const res = await api.restartWorker(id, page);
        data.reloadNotes();
        void data.reload();
        data.refollow();
        toast(res.mode === "restarted" ? "Restarting worker" : "Queued worker retry", page, 2500);
      } catch (err) {
        toast(err instanceof Error ? err.message : String(err));
      }
    },
    [id, data],
  );

  useEffect(() => {
    if (ledger?.totals) setRunChrome({ totals: ledger.totals });
    else if (detail) setRunChrome({ totals: detail.spent });
  }, [ledger, detail]);
  useEffect(() => () => setRunChrome(null), []);

  const focusAgent = useCallback((wanted: string) => {
    // A chip names a page; in a plan nothing shows in a browser, its agent is a unit.
    const other = wanted.startsWith("page:") ? `unit:${wanted.slice(5)}` : wanted;
    const agent = document.getElementById(`agent-${wanted}`) || !document.getElementById(`agent-${other}`) ? wanted : other;
    setFocus(agent);
    const el = document.getElementById(`agent-${agent}`);
    el?.scrollIntoView({ block: "nearest" });
    el?.focus({ preventScroll: true });
  }, []);

  const toggleAgent = useCallback((agentId: string) => {
    setBlown((cur) => {
      const next = new Set(cur);
      if (next.has(agentId)) next.delete(agentId);
      else next.add(agentId);
      return next;
    });
  }, []);

  const inspectAgent = useCallback(
    (agentId: string) => {
      const other = agentId.startsWith("page:") ? `unit:${agentId.slice(5)}` : agentId;
      const resolved = document.getElementById(`agent-${agentId}`) || !document.getElementById(`agent-${other}`) ? agentId : other;
      setBlown((cur) => (cur.has(resolved) ? cur : new Set(cur).add(resolved)));
      focusAgent(resolved);
    },
    [focusAgent],
  );

  const runHeadline = useMemo(() => {
    if (!detail) return "";
    if (detail.phase === "awaiting_approval") return "Plan ready — awaiting your approval";
    if (detail.phase === "planning") {
      const master = list.find((s) => s.id === "master");
      return master?.summary.headline || "Planning";
    }
    if (detail.active) {
      const working = list.filter((s) => s.state === "working");
      const workingWorkers = working.filter((s) => s.role === "worker");
      const active = workingWorkers.length ? workingWorkers : working;
      if (active.length === 1) {
        const only = active[0]!;
        const who = only.id === "master" ? "Master" : only.title.replace(/^Worker · /, "");
        return `${who}: ${only.summary.headline}`;
      }
      if (active.length > 1) {
        return `${active.length} workers active · ${active[0]!.summary.headline}`;
      }
      return phaseLabel(detail);
    }
    if (detail.cancelled) return "Cancelled";
    if (detail.phase === "done") {
      if (detail.merged && detail.merge_commit) return `Merged into ${detail.base || "the project"}`;
      const passed = detail.pages.filter((p) => p.state === "passed").length;
      return detail.pages.length > 0 ? `Completed · ${passed}/${detail.pages.length} passed` : "Completed";
    }
    if (detail.phase === "failed") return "Run failed";
    return phaseLabel(detail);
  }, [detail, list]);

  const openTab = useCallback(
    (t: string) => {
      setTab(t);
      setPanelOpen(true);
      if (narrow) setSheet(true);
    },
    [narrow, setTab],
  );

  const approveNow = useCallback(async () => {
    await api.approve(id).catch((err) => toast(err.message));
    await data.reload();
    data.refollow();
  }, [id, data]);

  useEffect(() => {
    setRunContext({
      id,
      pages,
      focusAgent,
      openTab,
      approve: phase === "awaiting_approval" ? () => void approveNow() : undefined,
      reject: phase === "awaiting_approval" ? () => void api.reject(id).then(() => data.reload()) : undefined,
    });
    return () => setRunContext(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id, pages.join(","), phase, focusAgent, openTab, approveNow]);

  useEffect(() => {
    const waiting = takeAttachments(id);
    if (waiting.length) window.setTimeout(() => composer.current?.attach(waiting), 50);
  }, [id]);

  useOn<{ path: string; line?: number }>("preview", ({ path, line }) => {
    setFileOpen({ path, line, n: Date.now() });
    setTab("files");
    setPanelOpen(true);
    if (narrow) setSheet(true);
  });
  useOn("files-toggle", () => {
    if (tab === "files" && panelOpen && !narrow) setPanelOpen(false);
    else openTab("files");
  });
  useOn<{ scope: Scope; page?: string; attempt?: number }>("diff", (req) => {
    setDiffRequest({ ...req, n: Date.now() });
    openTab("diff");
  });
  useOn<Chip>("chip-jump", (chip) => {
    if (chip.kind === "file") {
      const full = resolvedPath(chip.value) ?? chip.value;
      emit("preview", { path: full, line: chip.line });
    } else if (chip.kind === "page") focusAgent(`page:${chip.value}`);
    else if (chip.kind === "agent") focusAgent(chip.value.startsWith("worker:") ? `page:${chip.value.slice(7)}` : chip.value);
    else if (chip.kind === "key") navigate(`/dictionary?q=${encodeURIComponent(chip.value)}`);
  });

  useEffect(() => {
    const panel = splitPanel.current;
    if (narrow) return;
    if (panelOpen) panel?.expand();
    else panel?.collapse();
  }, [panelOpen, narrow, splitPanel]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.key === "\\") {
        e.preventDefault();
        if (narrow) setSheet((v) => !v);
        else setPanelOpen(!panelOpen);
        return;
      }
      if (typingTarget(e.target)) return;
      if (e.key === "Escape") {
        if (cite) {
          setCite(null);
          return;
        }
        if (narrow && sheet) {
          setSheet(false);
          return;
        }
        if (panelOpen) {
          e.preventDefault();
          setPanelOpen(false);
        }
        return;
      }
      if (mod || e.altKey) return;
      if (e.key === "1") {
        e.preventDefault();
        setLens("mission");
        return;
      }
      if (e.key === "2") {
        e.preventDefault();
        setLens("stream");
        return;
      }
      if (e.key === "3") {
        e.preventDefault();
        setLens("review");
        return;
      }
      if (e.key === "4") {
        e.preventDefault();
        setLens("telemetry");
        return;
      }
      const nodes = [...(runRef.current?.querySelectorAll<HTMLElement>("[data-agent]") ?? [])];
      const ids = nodes.map((n) => n.dataset.agent!).filter(Boolean);
      if (!ids.length) return;
      const at = Math.max(0, ids.indexOf(focus ?? ""));
      if (e.key === "j" || e.key === "k") {
        e.preventDefault();
        const next = e.key === "j" ? Math.min(ids.length - 1, (focus ? at : -1) + 1) : Math.max(0, (focus ? at : 0) - 1);
        const idNext = ids[next];
        if (idNext) focusAgent(idNext);
      } else if (e.key === "Enter" && focus && focus !== "you") {
        e.preventDefault();
        toggleAgent(focus);
      } else if (e.key === "e" && focus) {
        e.preventDefault();
        const surface = list.find((s) => s.id === focus);
        const path = surface ? fileTarget(surface) : null;
        if (path && looksLikePath(path)) emit("preview", { path });
        else openTab("files");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [cite, focus, focusAgent, list, narrow, openTab, panelOpen, sheet, toggleAgent]);

  useEffect(() => {
    const onUp = () => {
      const sel = window.getSelection();
      const text = sel?.toString().trim();
      if (!sel || !text || !sel.rangeCount) return;
      const node = sel.anchorNode instanceof Element ? sel.anchorNode : sel.anchorNode?.parentElement;
      const box = node?.closest("[data-agent-text]") as HTMLElement | null;
      if (!box) return;
      const rect = sel.getRangeAt(0).getBoundingClientRect();
      setCite({ x: rect.left + rect.width / 2, y: rect.top, text });
      setCiteNote("");
    };
    document.addEventListener("mouseup", onUp);
    return () => document.removeEventListener("mouseup", onUp);
  }, []);

  const send = async (message: { text: string; chips: Chip[]; attachments: Chip[] }) => {
    const text = messageText(message);
    if (!text) return;
    const to = noteTarget(message.chips);
    // A note on a finished run starts a new turn below; the mode says who takes it.
    const finished = !!detail && !detail.active && (detail.phase === "done" || detail.phase === "failed");
    try {
      await api.addNote(id, to, text, finished ? runMode : undefined);
      composer.current?.clear();
      data.reloadNotes();
      if (finished) {
        setPinned(true);
        await data.reload();
        data.refollow();
      }
    } catch (err) {
      toast(err instanceof Error ? err.message : String(err));
    }
  };

  if (data.error && !detail) return <Problem error={data.error} onRetry={() => void data.reload()} />;
  if (!detail) return <FlowSkeleton />;

  const pending = notes.filter((n) => !n.delivered && !removed.has(n.id));
  // A note that opened a turn is shown as that turn's message, not again down here.
  const opened = new Set(turns.flatMap((t) => t.notes));
  const delivered = notes.filter((n) => n.delivered && !opened.has(n.id));
  const settled = !detail.active && (detail.phase === "done" || detail.phase === "failed");
  const sideVisible = narrow ? sheet : panelOpen;
  const totalDiff = diffStats?.total;
  const hasTotalDiff = !!totalDiff && (totalDiff.added > 0 || totalDiff.removed > 0 || totalDiff.files > 0);

  const panel = (
    <div className="panel" data-testid="right-panel">
      <div className="panel-tabs" role="tablist">
        {PANELS.map((t) => (
          <div key={t} className={`panel-tab ${t === tab ? "on" : ""}`} role="tab" aria-selected={t === tab}>
            <button type="button" className="panel-tab-name" onClick={() => setTab(t)}>
              {TAB_LABEL[t]}
            </button>
          </div>
        ))}
      </div>
      <div className="panel-body">
        {tab === "files" && <FilesPanel run={id} open={fileOpen} layout="vertical" />}
        {tab === "diff" && <DiffTab run={id} detail={detail} request={diffRequest} />}
        {tab === "preview" && (
          <div className="preview-stack">
            <LivePreview id={id} detail={detail} />
            <BrowserTab run={id} detail={detail} entries={entries} />
          </div>
        )}
        {tab === "terminal" && <TerminalTab run={id} entries={entries} project={settings.data?.project ?? ""} onRan={() => void data.catchUp()} />}
      </div>
    </div>
  );

  const main = (
    <div className="run-main">
      <header className="flow-top" style={{ viewTransitionName: morphName(id) } as React.CSSProperties}>
        <div className="flow-top-status">
          {detail.active ? (
            <Loader2 size={14} strokeWidth={1.8} className="spin-ico agent-ico working" aria-hidden />
          ) : detail.phase === "done" ? (
            <Check size={14} strokeWidth={2} className="agent-ico done" aria-hidden />
          ) : detail.phase === "failed" ? (
            <AlertCircle size={14} strokeWidth={1.8} className="agent-ico failed" aria-hidden />
          ) : null}
          <span className="flow-top-headline clip" data-testid="run-headline" title={runHeadline}>
            {runHeadline}
          </span>
          <span className="flow-top-phase">{phaseLabel(detail)}</span>
          {hasTotalDiff && (
            <button
              type="button"
              className="agent-diff-btn mono"
              data-testid="run-diff-stat"
              title={`View full run diff (${totalDiff.files} file${totalDiff.files === 1 ? "" : "s"})`}
              onClick={() => emit("diff", { scope: "run" })}
            >
              <span className="diff-add">+{totalDiff.added}</span>
              {totalDiff.removed > 0 && <span className="diff-del">-{totalDiff.removed}</span>}
            </button>
          )}
          {detail.merged && detail.merge_commit ? (
            <span className="merged-into" data-testid="merged-into">
              <GitMerge size={12} aria-hidden /> {detail.base || "the project"}{" "}
              <button
                type="button"
                className="mono merge-hash"
                onClick={() => {
                  void navigator.clipboard?.writeText(detail.merge_commit ?? "");
                  toast("Copied", detail.merge_commit ?? "", 2500);
                }}
              >
                {detail.merge_commit}
              </button>
              {!detail.active && <UndoMerge id={id} undone={detail.undone} onDone={() => void data.reload()} />}
            </span>
          ) : (
            detail.branch && (
              <span className="flow-top-branch mono clip" title={detail.branch}>
                <GitBranch size={12} aria-hidden /> {detail.branch}
              </span>
            )
          )}
          {detail.budget_usd && detail.budget_usd > 0 && (
            <span
              className="flow-top-budget mono"
              title={`Spend cap: ${usd(detail.spent.cost_usd)} of ${usd(detail.budget_usd)}`}
            >
              {usd(detail.spent.cost_usd)} / {usd(detail.budget_usd)}
            </span>
          )}
        </div>
        <div className="flow-top-actions">
          {!detail.active && (detail.cancelled || !["done", "failed", "awaiting_approval"].includes(detail.phase)) && (
            <button
              type="button"
              className="ghost small"
              data-testid="run-resume"
              title={detail.cancelled ? "Resume this cancelled run, keeping any passed pages" : "Resume run"}
              onClick={() =>
                void api
                  .resume(id)
                  .then(() => {
                    data.refollow();
                    void data.reload();
                  })
                  .catch((err) => toast("Could not resume run", err instanceof Error ? err.message : String(err)))
              }
            >
              Resume
            </button>
          )}
          {(detail.active || detail.phase === "awaiting_approval") && (
            <CancelControl
              id={id}
              detail={detail}
              activeWorkers={activeWorkers}
              onCancelled={(mode) => {
                data.patch({ cancelling: mode });
                void data.reload();
              }}
              onRestartWorker={(page) => void restartWorker(page)}
            />
          )}
          {detail.phase === "done" && gh && detail.branch && (
            <button
              type="button"
              className="small"
              onClick={() =>
                void api
                  .draftPr(id)
                  .then((r) => setPr(r.url))
                  .catch((err) => toast("The pull request was not drafted", err.message))
              }
            >
              Draft pull request
            </button>
          )}
          {pr && <ExternalLink href={pr}>pull request</ExternalLink>}
          <button
            type="button"
            className={`ghost small icon-btn ${sideVisible ? "on" : ""}`}
            aria-label={sideVisible ? "Hide side panel" : "Show side panel"}
            aria-pressed={sideVisible}
            title="Toggle side panel (⌘\)"
            onClick={() => (narrow ? setSheet((v) => !v) : setPanelOpen(!panelOpen))}
          >
            <PanelRight size={14} strokeWidth={1.7} aria-hidden />
          </button>
        </div>
      </header>

      {/* 4-Lens Run Studio Switcher */}
      <nav className="lens-bar" role="tablist" aria-label="Run studio lenses" data-testid="lens-bar">
        <div className="lens-tabs">
          <button
            type="button"
            role="tab"
            aria-selected={lens === "mission"}
            className={`lens-tab ${lens === "mission" ? "on" : ""}`}
            onClick={() => setLens("mission")}
            data-testid="lens-mission"
          >
            <Network size={13} aria-hidden />
            <span>Mission</span>
            {detail.plan && <span className="lens-badge mono">{detail.plan.pages.length}</span>}
            <kbd>1</kbd>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={lens === "stream"}
            className={`lens-tab ${lens === "stream" ? "on" : ""}`}
            onClick={() => setLens("stream")}
            data-testid="lens-stream"
          >
            <MessageSquareText size={13} aria-hidden />
            <span>Stream</span>
            {activeWorkers.length > 0 && <span className="lens-badge live mono">{activeWorkers.length}</span>}
            <kbd>2</kbd>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={lens === "review"}
            className={`lens-tab ${lens === "review" ? "on" : ""}`}
            onClick={() => setLens("review")}
            data-testid="lens-review"
          >
            <FileDiff size={13} aria-hidden />
            <span>Review</span>
            {hasTotalDiff && <span className="lens-badge diff mono">+{totalDiff.added}</span>}
            <kbd>3</kbd>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={lens === "telemetry"}
            className={`lens-tab ${lens === "telemetry" ? "on" : ""}`}
            onClick={() => setLens("telemetry")}
            data-testid="lens-telemetry"
          >
            <Activity size={13} aria-hidden />
            <span>Telemetry</span>
            {detail.spent.cost_usd > 0 && <span className="lens-badge mono">{usd(detail.spent.cost_usd)}</span>}
            <kbd>4</kbd>
          </button>
        </div>

        {lens === "stream" && list.length > 1 && (
          <div className="stream-filters" role="group" aria-label="Filter stream">
            {(
              [
                ["all", "All"],
                ["active", "Active"],
                ["edits", "Edits"],
                ["issues", "Issues"],
              ] as const
            ).map(([key, label]) => (
              <button
                key={key}
                type="button"
                className={`filter-chip ${streamFilter === key ? "on" : ""}`}
                onClick={() => setStreamFilter(key)}
              >
                {label}
              </button>
            ))}
          </div>
        )}
      </nav>

      <div
        ref={scrollRef}
        className={`run-scroll ${lens !== "stream" ? "run-scroll-wide" : ""}`}
        onScroll={(e) => {
          const el = e.currentTarget;
          const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight <= 64;
          setPinned((prev) => (prev === nearBottom ? prev : nearBottom));
        }}
      >
        {lens === "mission" && (
          <MissionLens
            id={id}
            detail={detail}
            surfaces={list}
            diffStats={diffStats}
            onReload={() => void data.reload()}
            onRestartWorker={(page) => void restartWorker(page)}
            onOpenDiff={(page) => {
              openPageDiff(page);
            }}
            onJumpToStream={(agentId) => {
              setLens("stream");
              window.setTimeout(() => inspectAgent(agentId), 30);
            }}
            onApproved={() => {
              void data.reload();
              data.refollow();
            }}
          />
        )}

        {lens === "review" && (
          <ReviewLens
            id={id}
            detail={detail}
            diffStats={diffStats}
            ghReady={!!gh}
            onReload={() => {
              void data.reload();
              data.reloadNotes();
            }}
          />
        )}

        {lens === "telemetry" && <TelemetryLens id={id} detail={detail} ledger={ledger} />}

        {lens === "stream" && (
          <div className="run-thread">
            {turns.slice(0, -1).map((t, i) => (
              <PastTurn key={t.n} turn={t} next={turns[i + 1]} entries={shown} detail={detail} ledger={ledgerEntries} />
            ))}
            <Flow
              turn={turns.length > 1 ? { n: current.n, text: current.text, mode: current.mode, past: false } : undefined}
              detail={detail}
              list={filteredList}
              focus={focus}
              blown={blown}
              onToggleAgent={toggleAgent}
              onFocusAgent={inspectAgent}
              diffStats={diffStats}
              onOpenDiff={openPageDiff}
              onRestartWorker={(page) => void restartWorker(page)}
              approvals={toolApprovals.list}
              onTool={toolApprovals.answer}
              ask={
                phase === "awaiting_approval" ? (
                  <ApprovalAsk
                    id={id}
                    detail={detail}
                    composer={composer}
                    onOptimistic={data.patch}
                    onDone={() => {
                      void data.reload();
                      data.refollow();
                    }}
                  />
                ) : null
              }
            />

            <WebSources runId={id} entries={entries} research={detail.plan?.research} />

            {detail.limit && detail.error && phase !== "done" && phase !== "failed" ? (
              <LimitNotice id={id} limit={detail.limit} message={detail.error} onDone={() => { void data.reload(); data.refollow(); }} />
            ) : (
              detail.error && phase !== "done" && phase !== "failed" && (
                <p className="flow-fail">
                  <Rich text={detail.error} />
                </p>
              )
            )}
            {settled && <EndLine detail={detail} onDiff={() => setLens("review")} onRetry={() => { void data.reload(); data.refollow(); }} />}
            {settled && detail.branch && <ReviewAsk id={id} />}

            {(pending.length > 0 || delivered.length > 0) && (
              <section className="flow-notes">
                {pending.map((n) => (
                  <div key={n.id} className={`flow-note ${settled ? "is-next-turn" : ""}`} data-testid="draft">
                    <header className="flow-role">
                      <h2>You</h2>
                      <span className="flow-state">{settled ? "starting a new turn…" : `waiting for ${noteTargetLabel(n.to)}`}</span>
                      <button
                        type="button"
                        className="link small"
                        onClick={() => {
                          const text = window.prompt("Edit the note", n.text);
                          if (text !== null) void api.editNote(id, n.id, text || undefined).then(data.reloadNotes);
                        }}
                      >
                        edit
                      </button>
                      <button
                        type="button"
                        className="link small"
                        onClick={() => {
                          setRemoved((r) => new Set(r).add(n.id));
                          void api.editNote(id, n.id).then(data.reloadNotes, () => {
                            setRemoved((r) => {
                              const next = new Set(r);
                              next.delete(n.id);
                              return next;
                            });
                            toast("The note was not removed");
                          });
                          undoable("Removed the note", () => void api.addNote(id, n.to, n.text).then(data.reloadNotes));
                        }}
                      >
                        remove
                      </button>
                    </header>
                    <div className="flow-said">
                      <Rich text={n.text} />
                    </div>
                  </div>
                ))}
                {delivered.map((n) => (
                  <p key={n.id} className="flow-life">
                    Note to {noteTargetLabel(n.to)}: <Rich text={n.text} />
                  </p>
                ))}
              </section>
            )}
          </div>
        )}
      </div>

      {/* Ambient Floating Approval Dock across all 4 lenses */}
      {lens !== "stream" && <ApprovalDock list={toolApprovals.list} onAnswer={toolApprovals.answer} />}

      {lens === "stream" && detail.active && !pinned && (
        <button
          type="button"
          className="jump-live"
          data-testid="jump-live"
          onClick={() => {
            setPinned(true);
            const el = scrollRef.current;
            if (el) el.scrollTop = el.scrollHeight;
          }}
        >
          <ArrowDown size={13} aria-hidden />
          Live
        </button>
      )}
      <div className="composer-bar">
        <Composer
          ref={composer}
          bus
          pages={pages}
          agents={["master", "workers", "reviewer", "verifier", ...pages.map((p) => `worker:${p}`)]}
          placeholder={
            settled
              ? runMode === "code"
                ? "Ask the master for a change. It starts a new turn below."
                : "Ask for a fix or a change. It starts a new turn below."
              : "Steer master or @worker:<id>. @ files, # pull requests, $ skills, / commands"
          }
          onSend={(m) => void send(m)}
          footer={
            settled ? (
              <div className="composer-foot run-composer-foot">
                <div className="composer-actions">
                  <ModePicker value={runMode} onChange={setRunMode} label="Who takes the next turn" />
                </div>
                <span className="muted small">Enter to send</span>
              </div>
            ) : undefined
          }
        />
      </div>
    </div>
  );

  return (
    <div ref={runRef} className={`run ${narrow ? "narrow" : ""} ${!narrow && !panelOpen ? "panel-shut" : ""}`}>
      {narrow ? (
        <>
          {main}
          {sheet && (
            <Sheet open={sheet} onClose={() => setSheet(false)}>
              {panel}
            </Sheet>
          )}
        </>
      ) : (
        <Group orientation="horizontal" id="taskmaster-run" className="run-split" defaultLayout={split.defaultLayout} onLayoutChanged={split.onLayoutChanged}>
          <Panel id="run-main" minSize="40%" className="run-split-main">
            {main}
          </Panel>
          <Separator className={`split-handle ${panelOpen ? "" : "gone"}`} disabled={!panelOpen} />
          <Panel id="run-panel" panelRef={splitPanel} defaultSize="38%" minSize="24%" maxSize="60%" collapsible collapsedSize={0} className="run-split-panel">
            <aside className="run-panel" hidden={!panelOpen}>
              {panel}
            </aside>
          </Panel>
        </Group>
      )}
      {cite && (
        <div className="cite-pop" style={{ left: Math.max(8, cite.x - 120), top: Math.max(8, cite.y - 76) }} onMouseUp={(e) => e.stopPropagation()}>
          <input placeholder="Optional comment" value={citeNote} onChange={(e) => setCiteNote(e.target.value)} />
          <div className="row gap">
            <button
              type="button"
              className="primary small"
              onClick={() => {
                composer.current?.insert({
                  kind: "cite",
                  label: `“${cite.text.slice(0, 28)}${cite.text.length > 28 ? "…" : ""}”`,
                  value: citeNote ? `${cite.text}\n${citeNote}` : cite.text,
                  to: "master",
                  comment: citeNote || undefined,
                });
                setCite(null);
                window.getSelection()?.removeAllRanges();
              }}
            >
              Cite
            </button>
            <button type="button" className="ghost small" onClick={() => setCite(null)}>
              ×
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

function useLoadGh(): boolean {
  const [ok, setOk] = useState(false);
  useEffect(() => {
    api.gh().then((g) => setOk(!!g.account)).catch(() => setOk(false));
  }, []);
  return ok;
}

/** Take a merged run back out of the project, after asking once. The daemon refuses, with
 * the reason, when the run's files were edited since. */
function UndoMerge({ id, undone, onDone }: { id: string; undone?: RunDetail["undone"]; onDone: () => void }) {
  const [asking, setAsking] = useState(false);
  const [busy, setBusy] = useState(false);
  if (undone) {
    return (
      <span className="undo-merge-done" data-testid="run-undone">
        undone in <span className="mono">{undone.commit}</span>
      </span>
    );
  }
  if (!asking) {
    return (
      <button type="button" className="undo-merge-btn" data-testid="undo-merge" title="Put back every file this run changed" onClick={() => setAsking(true)}>
        Undo
      </button>
    );
  }
  const confirm = async () => {
    setBusy(true);
    try {
      const done = await api.undo(id);
      toast("Run undone", `Commit ${done.commit} put ${done.files} file${done.files === 1 ? "" : "s"} back.`);
      onDone();
    } catch (err) {
      toast("Not undone", err instanceof Error ? err.message : String(err), 10000);
    } finally {
      setBusy(false);
      setAsking(false);
    }
  };
  return (
    <span className="undo-merge-ask" role="group" aria-label="Undo this run">
      <span>Put back every file it changed, in a new commit?</span>
      <button type="button" className="undo-merge-btn" data-testid="undo-merge-confirm" disabled={busy} onClick={() => void confirm()}>
        {busy ? "Undoing…" : "Undo run"}
      </button>
      <button type="button" className="undo-merge-btn quiet" disabled={busy} onClick={() => setAsking(false)}>
        Keep
      </button>
    </span>
  );
}
