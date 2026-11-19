// A run is one transcript: you, then the master, then each worker. A side panel holds
// files, the diff, the preview and the terminal. Tokens and the approval mode live in
// the title bar. A small screen in the corner shows the app this run is building.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { GitBranch, GitMerge } from "lucide-react";
import { Group, Panel, Separator, useDefaultLayout, usePanelRef } from "react-resizable-panels";
import { api, type ReviewJob, type RunDetail } from "../api";
import { takeAttachments } from "../App";
import { setRunContext } from "../commands";
import { Composer, messageText, type ComposerHandle } from "../composer";
import { phaseLabel } from "../format";
import { FilesPanel } from "../lazy";
import { emit, Sheet, toast, undoable, useOn, type Chip } from "../overlay";
import { ExternalLink, Rich, looksLikePath, resolvedPath } from "../paths";
import { prefs, rememberRun, setPrefs } from "../prefs";
import { useSettings } from "../query";
import { navigate } from "../router";
import { setRunChrome } from "../shell/runChrome";
import { Problem } from "../ui";
import { DiffTab, type Scope } from "../run/Diff";
import { Flow, FlowSkeleton, fileTarget } from "../run/Flow";
import { LimitNotice } from "../run/LimitNotice";
import { LivePreview } from "../run/LivePreview";
import { surfaces, useRun } from "../run/model";
import { Stage } from "../run/Stage";
import { BrowserTab, TerminalTab } from "../run/Tabs";
import { useToolApprovals } from "../run/ToolApprovals";
import { morphName } from "./Gallery";

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
  const settings = useSettings();
  const toolApprovals = useToolApprovals(id, detail?.pending_approvals, data.entries, () => void data.reload());
  const gh = useLoadGh();
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
  const setTab = useCallback(
    (t: string) => {
      const next = panelTab(t);
      setTabState(next);
      setPrefs((p) => ({ panelTab: { ...p.panelTab, [id]: next } }));
    },
    [id],
  );

  const shown = entries;
  const list = useMemo(() => surfaces(detail, shown, ledger?.entries ?? []), [detail, shown, ledger]);
  const pages = detail?.plan?.pages.map((p) => p.id) ?? detail?.pages.map((p) => p.id) ?? [];
  const phase = detail?.phase;

  useEffect(() => {
    rememberRun(id);
  }, [id]);

  useEffect(() => {
    if (ledger?.totals) setRunChrome({ totals: ledger.totals });
    else if (detail) setRunChrome({ totals: detail.spent });
  }, [ledger, detail]);
  useEffect(() => () => setRunChrome(null), []);

  const focusAgent = useCallback((agent: string) => {
    setFocus(agent);
    const el = document.getElementById(`agent-${agent}`);
    el?.scrollIntoView({ block: "nearest" });
    el?.focus({ preventScroll: true });
  }, []);

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
        setBlown((cur) => {
          const n = new Set(cur);
          if (n.has(focus)) n.delete(focus);
          else n.add(focus);
          return n;
        });
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
  }, [cite, focus, focusAgent, list, narrow, openTab, panelOpen, sheet]);

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
    try {
      await api.addNote(id, "master", text);
      composer.current?.clear();
      data.reloadNotes();
    } catch (err) {
      toast(err instanceof Error ? err.message : String(err));
    }
  };

  if (data.error && !detail) return <Problem error={data.error} onRetry={() => void data.reload()} />;
  if (!detail) return <FlowSkeleton />;

  const pending = notes.filter((n) => !n.delivered && !removed.has(n.id));
  const delivered = notes.filter((n) => n.delivered);
  const settled = !detail.active && (detail.phase === "done" || detail.phase === "failed");

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
      <div className="run-scroll with-stage">
        <div className="flow-top" style={{ viewTransitionName: morphName(id) } as React.CSSProperties}>
          <span className="flow-phase">{phaseLabel(detail)}</span>
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
            </span>
          ) : (
            detail.branch && (
              <span className="mono">
                <GitBranch size={12} aria-hidden /> {detail.branch}
              </span>
            )
          )}
          <span className="grow" />
          {!detail.active && !["done", "failed", "awaiting_approval"].includes(detail.phase) && (
            <button type="button" className="ghost small" onClick={() => void api.resume(id).then(() => (data.refollow(), data.reload()))}>
              Resume
            </button>
          )}
          {(detail.active || detail.phase === "awaiting_approval") && (
            <button type="button" className="ghost small" onClick={() => void api.cancel(id).then(() => data.reload())}>
              Cancel
            </button>
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
        </div>

        <Flow
          detail={detail}
          list={list}
          focus={focus}
          blown={blown}
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

        {detail.limit && detail.error && phase !== "done" && phase !== "failed" ? (
          <LimitNotice id={id} limit={detail.limit} message={detail.error} onDone={() => { void data.reload(); data.refollow(); }} />
        ) : (
          detail.error && phase !== "done" && phase !== "failed" && (
            <p className="flow-fail">
              <Rich text={detail.error} />
            </p>
          )
        )}
        {settled && <EndLine detail={detail} onDiff={() => openTab("diff")} onRetry={() => { void data.reload(); data.refollow(); }} />}
        {settled && detail.branch && <ReviewAsk id={id} />}

        {(pending.length > 0 || delivered.length > 0) && (
          <section className="flow-notes">
            {pending.map((n) => (
              <div key={n.id} className="flow-note" data-testid="draft">
                <header className="flow-role">
                  <h2>You</h2>
                  <span className="flow-state">waiting for the master</span>
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
                      undoable("Removed the note", () => void api.addNote(id, "master", n.text).then(data.reloadNotes));
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
                Note to the master: <Rich text={n.text} />
              </p>
            ))}
          </section>
        )}
      </div>
      <Stage id={id} detail={detail} entries={entries} />
      <div className="composer-bar">
        <Composer
          ref={composer}
          bus
          pages={pages}
          agents={["master", "reviewer", "verifier", ...pages.map((p) => `worker:${p}`)]}
          placeholder="A note for the master. @ files, # pull requests, $ skills, / commands"
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
