// A run: the master, each page worker, the reviewer and the verifier as their own
// surfaces, the live plan, approvals, the notes waiting for the next brief, and a right
// panel with the plan, the diff, the browser, web search, the terminal and file previews.

import { startTransition, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import gsap from "gsap";
import { CircleCheck, CircleX, Coins, GitBranch, GitCompare, GitMerge, Loader2, PanelRightClose, PanelRightOpen, ShieldAlert } from "lucide-react";
import { api, type ReviewJob, type RunDetail } from "../api";
import { setRunContext } from "../commands";
import { Composer, messageText, type ComposerHandle } from "../composer";
import { isUnpriced, pageTone, phaseLabel, spendLabel, tokens, usd } from "../format";
import { emit, Sheet, toast, undoable, useOn, type Chip } from "../overlay";
import { ExternalLink, Rich, resolvedPath } from "../paths";
import { navigate } from "../router";
import { motionOn, prefs, rememberRun, setPrefs } from "../prefs";
import { RoleMark } from "../marks";
import { bezier, dur, ease, fadeUp } from "../motion";
import { Dot, Loading, Problem, useLoad } from "../ui";
import { takeAttachments } from "../App";
import { AgentSurface } from "../run/AgentSurface";
import { DiffTab, type Scope } from "../run/Diff";
import { PlanView } from "../run/PlanView";
import { LivePreview } from "../run/LivePreview";
import { TokenLedger } from "../run/Tokens";
import { ToolApprovals, useToolApprovals } from "../run/ToolApprovals";
import { BrowserTab, SearchTab, TerminalTab } from "../run/Tabs";
import { surfaces, useRun, type Surface } from "../run/model";
import { Lanes } from "../run/Lanes";
import { Timeline } from "../run/Timeline";
import { FilesPanel, Transcript } from "../lazy";

import { morphName } from "./Gallery";
import { Group, Panel, Separator, useDefaultLayout, usePanelRef } from "react-resizable-panels";

const FIXED = ["plan", "files", "diff", "browser", "search", "terminal"] as const;
const TAB_LABEL: Record<string, string> = { plan: "Plan", files: "Files", diff: "Diff", browser: "Browser", search: "Web search", terminal: "Terminal" };

/** An agent's transcript as terminal text: what it said, its tool calls, and how each turn ended. */
function logText(s: Surface | undefined): string {
  if (!s) return "";
  const dim = (t: string) => `\x1b[2m${t}\x1b[0m`;
  const bold = (t: string) => `\x1b[1m${t}\x1b[0m`;
  const out: string[] = [];
  for (const call of s.calls) {
    out.push(dim(`── turn ${call.call + 1}${call.tag.model ? ` · ${call.tag.harness ?? ""} ${call.tag.model}` : ""} · ${call.at}`));
    for (const item of call.items) {
      if (item.t === "text") out.push(item.text.trimEnd());
      else if (item.t === "thought") out.push(dim(item.text.trim()));
      else if (item.t === "life") out.push(dim(`· ${item.text}`));
      else out.push(`${bold(`$ ${item.name}`)}${item.target ? ` ${item.target}` : ""}${item.allowed === false ? dim("  (refused)") : ""}`);
    }
    if (call.end) out.push(call.end.ok === false ? bold(`✕ ${call.end.error ?? "the turn failed"}`) : dim("✓ turn ended"));
  }
  return out.join("\n") + "\n";
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

// On-demand code review of a finished run: one combined reviewer over the whole diff.
function ReviewCard({ id }: { id: string }) {
  const [job, setJob] = useState<ReviewJob | null>(null);
  const [busy, setBusy] = useState(false);
  const jobId = useRef<string | null>(null);
  const load = useCallback(async (reviewId: string) => {
    const next = await api.review(reviewId);
    jobId.current = reviewId;
    setJob(next);
    return next;
  }, []);
  // Pick up the latest review of this run after a reload.
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
    <section className={`review-card ${running ? "is-running" : ""}`} data-testid="review-card" aria-label="Code review">
      <div className="review-card-head">
        <span className="review-ico" aria-hidden>
          {running ? <Loader2 size={16} className="spin-ico" /> : <RoleMark role="reviewer" size={16} />}
        </span>
        <div className="grow">
          <div className="strong">Code review</div>
          <div className="muted small">
            {running
              ? "One reviewer is reading the whole diff. This takes a few minutes."
              : "On demand: one reviewer reads the whole diff and reports only confirmed problems."}
          </div>
        </div>
        {running ? (
          <>
            <span className="sr-only" data-testid="review-running">Review in progress…</span>
            <span className="badge busy" aria-hidden>
              Reviewing
            </span>
            <button className="ghost small" data-testid="review-cancel" onClick={() => void api.cancelReview(job.id).then(() => load(job.id))}>
              Cancel review
            </button>
          </>
        ) : (
          <button className={job ? "small" : "primary small"} data-testid="review-button" disabled={busy} onClick={() => void start()}>
            {job ? "Review again" : "Review"}
          </button>
        )}
      </div>
      {job?.status === "failed" && <div className="error-line" data-testid="review-error">Review failed: {job.error}</div>}
      {job?.status === "done" && (
        <div className={`review-result ${confirmed.length ? "bad" : "good"}`} data-testid="review-result">
          <div className="review-verdict">
            {confirmed.length === 0 ? <CircleCheck size={15} aria-hidden /> : <ShieldAlert size={15} aria-hidden />}
            <span>{confirmed.length === 0 ? "Review finished: no confirmed problems." : `Review finished: ${confirmed.length} confirmed problem(s).`}</span>
          </div>
          {confirmed.length > 0 && (
            <ul className="review-findings">
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

/** What a finished run produced and what to do next. */
function DoneSummary({ detail, onDiff }: { detail: RunDetail; onDiff: () => void }) {
  const ok = detail.phase === "done";
  const passed = detail.pages.filter((p) => p.state === "passed").length;
  return (
    <section className={`done-card ${ok ? "good" : "bad"}`} data-testid="done-summary" aria-label="Run result">
      <span className="done-ico" aria-hidden>
        {ok ? <CircleCheck size={20} /> : <CircleX size={20} />}
      </span>
      <div className="grow">
        <div className="done-title">
          {ok
            ? detail.merged && detail.merge_commit
              ? `Merged into ${detail.base || "the project"}`
              : "Run finished"
            : detail.cancelled
              ? "Run cancelled"
              : "Run failed"}
        </div>
        <div className="muted small">
          {passed} of {detail.pages.length} page{detail.pages.length === 1 ? "" : "s"} passed their checks
          {detail.pages.some((p) => p.repairs > 0) ? ` · ${detail.pages.reduce((n, p) => n + p.repairs, 0)} repair(s)` : ""}
        </div>
        <div className="done-pages">
          {detail.pages.map((p) => (
            <span key={p.id} className="page-pill">
              <Dot tone={pageTone(p.state)} />
              {p.id}
            </span>
          ))}
        </div>
      </div>
      <button className="small" onClick={onDiff}>
        <GitCompare size={14} aria-hidden /> View diff
      </button>
    </section>
  );
}

function ApprovalCard({
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
      // A draft in the composer rides along as the note on this approval.
      const draft = composer.current && !composer.current.isEmpty() ? messageText(composer.current.read()) : undefined;
      // The card goes and the run reads as building at once; a refusal puts it back.
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
      // A text reply rejects this plan; the master plans again with it as the note.
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
    <motion.div
      className="approval"
      data-testid="approval"
      initial={motionOn() ? { ...fadeUp.initial, scale: 0.98 } : false}
      animate={{ ...fadeUp.animate, scale: 1 }}
      transition={{ duration: motionOn() ? fadeUp.transition.duration : 0, ease: bezier.out }}
    >
      <div className="approval-head">
        <span className="approval-ico" aria-hidden>
          <ShieldAlert size={16} />
        </span>
        <div className="grow">
          <div className="strong">The master asks to build this plan</div>
          <div className="small muted">
            {plan ? `${plan.pages.length} page(s): ${plan.pages.map((p) => `${p.id} → ${p.owned_glob}`).join(", ")}` : "a plan"} · Build stays blocked until you approve.
          </div>
        </div>
      </div>
      <textarea
        rows={2}
        value={reply}
        placeholder="Reply in words to reject it and plan again with your note"
        onChange={(e) => setReply(e.target.value)}
      />
      <div className="row gap wrap approval-actions">
        <button disabled={busy} onClick={() => void reject()}>
          {reply.trim() ? "Reject with reply" : "Reject"}
        </button>
        <button className="primary" disabled={busy} onClick={() => void approve()}>
          Approve
        </button>
      </div>
    </motion.div>
  );
}

export function RunView({ id }: { id: string }) {
  const data = useRun(id);
  const { detail, entries, ledger, notes } = data;
  const narrow = useNarrow();
  const composer = useRef<ComposerHandle>(null);
  const settings = useLoad(api.settings, []);
  const reloadRun = data.reload;
  const toolApprovals = useToolApprovals(id, detail?.pending_approvals, data.entries, () => void reloadRun());
  const gh = useLoad(api.gh, []);
  const [focus, setFocus] = useState<string | null>(null);
  const [to, setTo] = useState("master");
  const [sheet, setSheet] = useState(false);
  const [cite, setCite] = useState<{ x: number; y: number; text: string; agent: string } | null>(null);
  const [citeNote, setCiteNote] = useState("");
  const [pr, setPr] = useState<string | null>(null);
  const panelBody = useRef<HTMLDivElement>(null);
  const runRef = useRef<HTMLDivElement>(null);
  const panelRef = useRef<HTMLElement>(null);
  const [panelOpen, setPanelOpenState] = useState(() => prefs().runPanel);
  const setPanelOpen = useCallback((open: boolean) => {
    setPanelOpenState(open);
    setPrefs({ runPanel: open });
  }, []);
  const [removed, setRemoved] = useState<Set<string>>(() => new Set());
  const skipPanelEnter = useRef(true);

  // The right panel: fixed tabs plus file previews; the active one is kept per run.
  const [tabs, setTabs] = useState<string[]>([...FIXED]);
  const [closed, setClosed] = useState<string[]>([]);
  const [tab, setTabState] = useState<string>(() => prefs().panelTab[id] ?? "plan");
  const [fileOpen, setFileOpen] = useState<{ path: string; line?: number; n: number } | null>(null);
  const [at, setAt] = useState<number | null>(null);
  const progress = useRef<HTMLDivElement>(null);
  const head = useRef<HTMLElement>(null);
  const splitPanel = usePanelRef();
  const tabStrip = useRef<HTMLDivElement>(null);
  // The first frame draws the header, the lanes and the timeline; what is below them follows
  // in the next one, as a transition React can interrupt, so opening a run never stalls.
  const [below, setBelow] = useState(false);
  useEffect(() => {
    const frame = requestAnimationFrame(() => startTransition(() => setBelow(true)));
    return () => cancelAnimationFrame(frame);
  }, []);
  // The active tab is kept in view in the strip, sideways only.
  useLayoutEffect(() => {
    const strip = tabStrip.current;
    const on = strip?.querySelector<HTMLElement>(".panel-tab.on");
    if (!strip || !on) return;
    const left = on.offsetLeft;
    const right = left + on.offsetWidth;
    if (left < strip.scrollLeft) strip.scrollLeft = left - 8;
    else if (right > strip.scrollLeft + strip.clientWidth) strip.scrollLeft = right - strip.clientWidth + 8;
  });
  const split = useDefaultLayout({ id: "taskmaster-run", storage: localStorage });
  const [diffRequest, setDiffRequest] = useState<{ scope: Scope; page?: string; attempt?: number; n: number } | null>(null);
  const setTab = useCallback(
    (t: string) => {
      setTabState(t);
      setTabs((all) => (all.includes(t) ? all : [...all, t]));
      setPrefs((p) => ({ panelTab: { ...p.panelTab, [id]: t } }));
    },
    [id],
  );

  useEffect(() => {
    rememberRun(id);
  }, [id]);

  // Scrubbing the timeline replays the run: only what had happened by then is shown.
  const shown = useMemo(() => (at === null ? entries : entries.filter((e) => Date.parse(e.at) <= at)), [entries, at]);
  const list = useMemo(() => surfaces(detail, shown, ledger?.entries ?? []), [detail, shown, ledger]);
  const master = list.find((s) => s.id === "master");
  const workers = list.filter((s) => s.page);
  const rest = list.filter((s) => s.id !== "master" && !s.page);
  const maxTokens = Math.max(1, ...list.map((s) => s.tokens));
  const pageStates = useMemo(() => Object.fromEntries((detail?.pages ?? []).map((p) => [p.id, p.state])), [detail?.pages]);
  const pages = detail?.plan?.pages.map((p) => p.id) ?? detail?.pages.map((p) => p.id) ?? [];
  const phase = detail?.phase;

  const focusAgent = useCallback((agent: string) => {
    setFocus(agent);
    const el = document.getElementById(`agent-${agent}`);
    el?.scrollIntoView({ block: "start", behavior: "smooth" });
    el?.focus({ preventScroll: true });
    setTo(agent.startsWith("page:") ? agent : agent === "master" || agent === "reviewer" || agent === "verifier" ? agent : "master");
  }, []);

  const openTab = useCallback(
    (t: string) => {
      setTab(t);
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

  // Files dropped on this run's card elsewhere.
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
    if (tab === "files" && panelOpen) togglePanel();
    else {
      setTab("files");
      setPanelOpen(true);
      if (narrow) setSheet(true);
    }
  });
  const openAgent = useCallback(
    (agent: string) => {
      setTab(`agent:${agent}`);
      setPanelOpen(true);
      if (narrow) setSheet(true);
    },
    [narrow, setTab],
  );
  const openPageDiff = useCallback((page: string) => emit("diff", { scope: "page", page }), []);
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
  useOn<Chip>("chip", (chip) => {
    if (chip.to) setTo(chip.to.startsWith("page:") || ["master", "reviewer", "verifier", "workers"].includes(chip.to) ? chip.to : "master");
  });

  // Command-W closes the focused tab; Command-Shift-T opens the last one closed.
  const closeTab = useCallback(
    (t: string) => {
      setTabs((all) => {
        const next = all.filter((x) => x !== t);
        if (t === tab) setTabState(next[next.length - 1] ?? "plan");
        return next;
      });
      setClosed((c) => [...c, t]);
    },
    [tab],
  );
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.key === "\\") {
        e.preventDefault();
        togglePanel();
      } else if (mod && !e.shiftKey && e.key.toLowerCase() === "w") {
        e.preventDefault();
        closeTab(tab);
      } else if (mod && e.shiftKey && e.key.toLowerCase() === "t") {
        e.preventDefault();
        const last = closed[closed.length - 1];
        if (last) {
          setClosed((c) => c.slice(0, -1));
          setTab(last);
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [tab, closed, closeTab, setTab]);

  useLayoutEffect(() => {
    const el = panelBody.current;
    if (!el || !motionOn()) return;
    el.classList.remove("tm-fade");
    void el.offsetWidth;
    el.classList.add("tm-fade");
  }, [tab]);

  // The side panel opens and closes on the split: its content fades and slides by
  // transform while the split itself changes once, so nothing is laid out per frame.
  function togglePanel() {
    if (narrow) {
      setSheet((v) => !v);
      return;
    }
    const body = panelRef.current;
    if (!panelOpen) {
      setPanelOpen(true);
      return;
    }
    if (!body || !motionOn()) {
      setPanelOpen(false);
      return;
    }
    gsap.to(body, { opacity: 0, x: 12, duration: dur.fast, ease: ease.out, onComplete: () => setPanelOpen(false) });
  }

  useLayoutEffect(() => {
    if (narrow) return;
    const panel = splitPanel.current;
    if (panelOpen) panel?.expand();
    else panel?.collapse();
    if (!panelOpen || skipPanelEnter.current) {
      skipPanelEnter.current = false;
      return;
    }
    const body = panelRef.current;
    if (!body || !motionOn()) return;
    const tween = gsap.fromTo(body, { opacity: 0, x: 12 }, { opacity: 1, x: 0, duration: dur.base, ease: ease.out, clearProps: "opacity,transform" });
    return () => void tween.progress(1).kill();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [panelOpen, narrow]);

  // A run starting: the header rises in, line by line.
  const intro = useRef(false);
  const introTl = useRef<gsap.core.Timeline | null>(null);
  useLayoutEffect(() => {
    if (intro.current || !detail || !head.current) return;
    intro.current = true;
    if (!motionOn() || !["planning", "awaiting_approval"].includes(detail.phase)) return;
    const parts = head.current.querySelectorAll(".eyebrow, .run-title, .run-meta, .prompt");
    // Not killed when the run updates: it plays out, and a page that closes finishes it.
    const tl = gsap.timeline();
    tl.fromTo(parts, { opacity: 0, y: 6 }, { opacity: 1, y: 0, duration: 0.22, ease: ease.out, stagger: 0.04, clearProps: "opacity,transform" });
    introTl.current = tl;
  }, [detail]);
  useEffect(() => () => void introTl.current?.progress(1).kill(), []);

  // Progress: the bar follows the pages that passed; finishing fills it and lifts the result in.
  const total = detail?.pages.length ?? 0;
  const passed = detail?.pages.filter((p) => p.state === "passed").length ?? 0;
  const lastPhase = useRef<string | null>(null);
  useLayoutEffect(() => {
    const bar = progress.current;
    if (!bar || !detail) return;
    const to = detail.phase === "done" ? 1 : total ? passed / total : 0;
    const was = lastPhase.current;
    lastPhase.current = detail.phase;
    const finishing = !!was && was !== "done" && detail.phase === "done";
    // A run that was already over when opened shows no bar.
    const settled = !finishing && !detail.active && ["done", "failed"].includes(detail.phase);
    if (!motionOn() || settled) {
      gsap.set(bar, { scaleX: to, opacity: settled ? 0 : 1 });
      return;
    }
    const tl = gsap.timeline();
    tl.to(bar, { scaleX: to, opacity: 1, duration: 0.3, ease: ease.out });
    if (finishing) {
      tl.to(bar, { opacity: 0, duration: 0.24, ease: ease.out }, "+=0.2");
      const card = document.querySelector("[data-testid=done-summary]");
      if (card) tl.fromTo(card, { opacity: 0, y: 8 }, { opacity: 1, y: 0, duration: 0.26, ease: ease.out, clearProps: "opacity,transform" }, "<");
    }
    return () => void tl.progress(1).kill();
  }, [detail?.phase, detail?.active, passed, total]);

  // Cite: select text in an agent's reply.
  useEffect(() => {
    const onUp = () => {
      const sel = window.getSelection();
      const text = sel?.toString().trim();
      if (!sel || !text || !sel.rangeCount) return;
      const node = sel.anchorNode instanceof Element ? sel.anchorNode : sel.anchorNode?.parentElement;
      const box = node?.closest("[data-agent-text]") as HTMLElement | null;
      if (!box) return;
      const rect = sel.getRangeAt(0).getBoundingClientRect();
      setCite({ x: rect.left + rect.width / 2, y: rect.top, text, agent: box.dataset.agentText! });
      setCiteNote("");
    };
    document.addEventListener("mouseup", onUp);
    return () => document.removeEventListener("mouseup", onUp);
  }, []);

  const send = async (message: { text: string; chips: Chip[]; attachments: Chip[] }) => {
    const text = messageText(message);
    if (!text) return;
    try {
      await api.addNote(id, to, text);
      composer.current?.clear();
      data.reloadNotes();
    } catch (err) {
      toast(err instanceof Error ? err.message : String(err));
    }
  };

  if (data.error && !detail) return <Problem error={data.error} onRetry={() => void data.reload()} />;
  if (!detail)
    return (
      <div className="page">
        <Loading rows={2} label="Opening the run" />
        <Loading rows={2} tall label="Opening the run" />
      </div>
    );

  const recipients = ["master", "workers", ...pages.map((p) => `page:${p}`), "reviewer", "verifier"];
  const pending = notes.filter((n) => !n.delivered && !removed.has(n.id));
  const delivered = notes.filter((n) => n.delivered);
  const panelTabs = tabs;
  const panel = (
    <div className="panel" data-testid="right-panel">
      <div className="panel-tabs" role="tablist" ref={tabStrip}>
        {panelTabs.map((t) => (
          <div key={t} className={`panel-tab ${t === tab ? "on" : ""}`} role="tab" aria-selected={t === tab}>
            <button className="panel-tab-name" onClick={() => setTab(t)}>
              {t.startsWith("agent:") ? `${t.slice(6).replace(/^page:/, "")} log` : TAB_LABEL[t]}
            </button>
            <button className="chip-x" aria-label={`Close ${t}`} onClick={() => closeTab(t)}>
              ×
            </button>
          </div>
        ))}
        {closed.length > 0 && (
          <button className="ghost small" title="Reopen the last closed tab (⌘⇧T)" onClick={() => setTab(closed[closed.length - 1]!)}>
            +
          </button>
        )}
      </div>
      <div className="panel-body" ref={panelBody}>
        {tab === "plan" && (detail.plan ? <PlanView plan={detail.plan} unpriced={isUnpriced(ledger?.totals ?? detail.spent)} /> : <div className="tab-empty"><span className="spin" aria-hidden /> No plan yet. The master is still planning.</div>)}
        {tab === "diff" && <DiffTab run={id} detail={detail} request={diffRequest} />}
        {tab === "browser" && <BrowserTab run={id} detail={detail} entries={entries} />}
        {tab === "search" && <SearchTab />}
        {tab === "terminal" && <TerminalTab run={id} entries={entries} project={settings.data?.project ?? ""} onRan={() => void data.catchUp()} />}
        {tab === "files" && <FilesPanel run={id} open={fileOpen} layout="vertical" />}
        {tab.startsWith("agent:") &&
          (list.find((x) => x.id === tab.slice(6))?.calls.length ? (
            <Transcript label={`${tab.slice(6)} log`} text={logText(list.find((x) => x.id === tab.slice(6)))} />
          ) : (
            <div className="tab-empty">{tab.slice(6).replace(/^page:/, "")} has nothing in its log for this run.</div>
          ))}
        {!panelTabs.includes(tab) && <div className="tab-empty">Every tab is closed. <kbd>⌘</kbd><kbd>⇧</kbd><kbd>T</kbd> reopens the last one.</div>}
      </div>
    </div>
  );

  const main = (
      <div className="run-main">
        <div className="run-scroll">
        <header className="run-head" ref={head} style={{ viewTransitionName: morphName(id) } as React.CSSProperties}>
          <div className="run-progress" aria-hidden>
            <div className="run-progress-fill" ref={progress} />
          </div>
          <div className="grow">
            <div className="eyebrow">{settings.data?.project ?? "run"}</div>
            <div className="run-title">
              <h1 className="break">{id}</h1>
              <span className={`phase phase-${phase}`}>
                {phaseLabel(detail)}
              </span>
            </div>
            <div className="run-meta small">
              {detail.active && <span className="live-tag">live</span>}
              {detail.merged && detail.merge_commit ? (
                <span className="merged-into" data-testid="merged-into">
                  <GitMerge size={12} aria-hidden />
                  Merged into {detail.base || "the project"}{" "}
                  <button
                    type="button"
                    className="mono merge-hash"
                    title="Copy commit hash"
                    onClick={() => {
                      void navigator.clipboard?.writeText(detail.merge_commit ?? "");
                      toast("Copied", detail.merge_commit ?? "", 2500);
                    }}
                  >
                    {detail.merge_commit}
                  </button>
                </span>
              ) : (
                detail.branch && (
                  <span className="mono">
                    <GitBranch size={12} aria-hidden />
                    {detail.branch}
                  </span>
                )
              )}
              <span>
                <Coins size={12} aria-hidden />
                {spendLabel(ledger?.totals ?? detail.spent)}
              </span>
              {detail.estimate &&
                (isUnpriced(ledger?.totals ?? detail.spent) ? (
                  <span>estimate about {tokens(detail.estimate.tokens)} tokens</span>
                ) : (
                  <span>estimate {usd(detail.estimate.usd)}</span>
                ))}
            </div>
          </div>
          <div className="run-actions">
            {!detail.active && !["done", "failed", "awaiting_approval"].includes(detail.phase) && (
              <button className="ghost" onClick={() => void api.resume(id).then(() => (data.refollow(), data.reload()))}>Resume</button>
            )}
            {(detail.active || detail.phase === "awaiting_approval") && (
              <button className="ghost" onClick={() => void api.cancel(id).then(() => data.reload())}>
                Cancel
              </button>
            )}
            {detail.phase === "done" && gh.data?.account && detail.branch && (
              <button
                className="primary"
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
            {narrow ? (
              <button className="ghost" onClick={() => setSheet(true)}>
                Panel
              </button>
            ) : (
              <button className="ghost icon" aria-label={panelOpen ? "Hide panel" : "Show panel"} title={panelOpen ? "Hide panel (⌘\\)" : "Show panel (⌘\\)"} onClick={togglePanel}>
                {panelOpen ? <PanelRightClose size={16} /> : <PanelRightOpen size={16} />}
              </button>
            )}
          </div>
          {detail.prompt && (
            <p className="prompt">
              <Rich text={detail.prompt.split("\n")[0] ?? ""} />
            </p>
          )}
          {detail.error && (
            <div className="error-line small">
              <Rich text={detail.error} />
            </div>
          )}
        </header>

        {phase === "planning" && (
          <div className="banner status-strip small" role="status">
            <span className="spin" aria-hidden />
            <span>
              <strong>Planning.</strong> The master's text appears below as it arrives; the locked plan replaces this line when it is ready.
            </span>
          </div>
        )}
        {["done", "failed"].includes(detail.phase) && !detail.active && <DoneSummary detail={detail} onDiff={() => (openTab("diff"), setPanelOpen(true))} />}
        {["done", "failed"].includes(detail.phase) && detail.branch && <ReviewCard id={id} />}
        {phase === "awaiting_approval" && (
          <ApprovalCard
            id={id}
            detail={detail}
            composer={composer}
            onOptimistic={data.patch}
            onDone={() => {
              void data.reload();
              data.refollow();
            }}
          />
        )}
        <Lanes workers={workers} pages={pageStates} onOpen={openAgent} onDiff={openPageDiff} />
        <Timeline detail={detail} entries={entries} at={at} onScrub={setAt} />

        {below && detail.plan && <PlanView plan={detail.plan} unpriced={isUnpriced(ledger?.totals ?? detail.spent)} arrived={data.planArrived} compact open={phase === "awaiting_approval"} />}

        {below && ledger && <TokenLedger totals={ledger.totals} open={["done", "failed"].includes(detail.phase) && !detail.active} />}

        <ToolApprovals list={toolApprovals.list} answer={toolApprovals.answer} />
        <LivePreview id={id} detail={detail} />

        {!below && <Loading rows={3} tall label="Opening the run" />}
        {below && <div className="agents">
          {master && (
            <AgentSurface index={0} surface={master} detail={detail} maxTokens={maxTokens} events={data.events.length} focused={focus === "master"} />
          )}
          {workers.length > 0 && (
            <div className="workers">
              {workers.map((s, i) => (
                <AgentSurface key={s.id} index={i + 1} surface={s} detail={detail} maxTokens={maxTokens} events={data.events.length} focused={focus === s.id} />
              ))}
            </div>
          )}
          <div className="workers">
            {rest.map((s, i) => (
              <AgentSurface
                key={s.id}
                index={workers.length + 1 + i}
                surface={s}
                detail={detail}
                maxTokens={maxTokens}
                events={data.events.length}
                focused={focus === s.id}
                compact={s.state === "waiting" && s.calls.length === 0 && !detail.active}
              />
            ))}
          </div>
        </div>}

        <section className="drafts">
          {pending.map((n) => (
            <div key={n.id} className="draft" data-testid="draft">
              <div className="row between small">
                <span className="muted">waiting for the next brief to {n.to}</span>
                <span className="row gap">
                  <button
                    className="link small"
                    onClick={() => {
                      const text = window.prompt("Edit the note", n.text);
                      if (text !== null) void api.editNote(id, n.id, text || undefined).then(data.reloadNotes);
                    }}
                  >
                    edit
                  </button>
                  <button
                    className="link small"
                    onClick={() => {
                      // Gone at once; Undo puts the same note back for the same agent.
                      setRemoved((r) => new Set(r).add(n.id));
                      void api.editNote(id, n.id).then(data.reloadNotes, () => {
                        setRemoved((r) => (r.delete(n.id), new Set(r)));
                        toast("The note was not removed");
                      });
                      undoable(`Removed the note to ${n.to}`, () => void api.addNote(id, n.to, n.text).then(data.reloadNotes));
                    }}
                  >
                    remove
                  </button>
                </span>
              </div>
              <div className="pre-wrap">
                <Rich text={n.text} />
              </div>
            </div>
          ))}
          {delivered.length > 0 && (
            <details className="small">
              <summary>{delivered.length} note(s) delivered</summary>
              {delivered.map((n) => (
                <div key={n.id} className="delivered">
                  <span className="muted">to {n.to}: </span>
                  <Rich text={n.text} />
                </div>
              ))}
            </details>
          )}
        </section>

        </div>
        <div className="composer-bar">
          <div className="row gap wrap small">
            <label className="muted" htmlFor="note-to">
              Note to
            </label>
            <select id="note-to" value={to} onChange={(e) => setTo(e.target.value)}>
              {recipients.map((r) => (
                <option key={r} value={r}>
                  {r === "workers" ? "every worker" : r}
                </option>
              ))}
            </select>
            <span className="muted note-hint">Delivered with the next approval or repair. It does not interrupt a reply.</span>
          </div>
          <Composer
            ref={composer}
            bus
            pages={pages}
            agents={["master", "reviewer", "verifier", ...pages.map((p) => `worker:${p}`)]}
            placeholder="A note for the next brief · @ file, page, agent or key · # pull request · / commands"
            onSend={(m) => void send(m)}
          />
        </div>
      </div>

  );

  return (
    <div ref={runRef} className={`run ${narrow ? "narrow" : ""} ${!narrow && !panelOpen ? "panel-shut" : ""}`}>
      {narrow ? (
        <>
          {main}
          <Sheet open={sheet} onClose={() => setSheet(false)}>
            {panel}
          </Sheet>
        </>
      ) : (
        <Group orientation="horizontal" id="taskmaster-run" className="run-split" defaultLayout={split.defaultLayout} onLayoutChanged={split.onLayoutChanged}>
          <Panel id="run-main" minSize="40%" className="run-split-main">
            {main}
          </Panel>
          <Separator className={`split-handle ${panelOpen ? "" : "gone"}`} disabled={!panelOpen} />
          <Panel id="run-panel" panelRef={splitPanel} defaultSize="38%" minSize="24%" maxSize="60%" collapsible collapsedSize={0} className="run-split-panel">
            <aside ref={panelRef} className="run-panel" hidden={!panelOpen}>
              {panel}
            </aside>
          </Panel>
        </Group>
      )}

      <AnimatePresence>
      {cite && (
        <motion.div
          key="cite"
          className="cite-pop"
          style={{ left: Math.max(8, cite.x - 120), top: Math.max(8, cite.y - 76) }}
          onMouseUp={(e) => e.stopPropagation()}
          initial={motionOn() ? { opacity: 0, y: 4 } : false}
          animate={{ opacity: 1, y: 0 }}
          exit={motionOn() ? { opacity: 0, y: 4 } : { opacity: 1, transition: { duration: 0 } }}
          transition={{ duration: motionOn() ? dur.fast : 0, ease: bezier.out }}
        >
          <input placeholder="Optional comment" value={citeNote} onChange={(e) => setCiteNote(e.target.value)} />
          <div className="row gap">
            <button
              className="primary small"
              onClick={() => {
                const target = cite.agent;
                setTo(target);
                composer.current?.insert({
                  kind: "cite",
                  label: `“${cite.text.slice(0, 28)}${cite.text.length > 28 ? "…" : ""}”`,
                  value: cite.text,
                  to: target,
                  comment: citeNote || undefined,
                });
                setCite(null);
                window.getSelection()?.removeAllRanges();
              }}
            >
              Cite
            </button>
            <button className="ghost small" onClick={() => setCite(null)}>
              ×
            </button>
          </div>
        </motion.div>
      )}
      </AnimatePresence>
    </div>
  );
}
