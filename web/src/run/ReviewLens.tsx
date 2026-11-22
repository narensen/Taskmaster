// Lens 3 — Review & Ship Studio:
// Full-bleed unit-grouped diff workspace with click-a-line inline review comments
// routed directly to the owning unit's worker (`page:<id>`), split Live Preview toggle,
// on-demand AI Review trigger, Draft PR, and Undo Merge controls.

import { useEffect, useMemo, useState } from "react";
import {
  Check,
  CheckCircle2,
  Eye,
  FileDiff,
  GitFork,
  GitPullRequest,
  MessageSquarePlus,
  RefreshCw,
  RotateCcw,
  Search,
  Send,
  ShieldCheck,
  X,
} from "lucide-react";
import { api, type DiffStats, type PreviewCapture, type PrSyncReport, type ReviewJob, type RunDetail } from "../api";
import { toast } from "../overlay";
import { ExternalLink } from "../paths";
import { navigate } from "../router";
import { parseDiff } from "./Diff";
import { LivePreview } from "./LivePreview";
import { useReviewComments } from "./reviewDrafts";

export function ReviewLens({
  id,
  detail,
  diffStats,
  ghReady,
  onReload,
  onCapture,
}: {
  id: string;
  detail: RunDetail;
  diffStats: DiffStats | null;
  ghReady: boolean;
  onReload: () => void;
  onCapture?: (capture: PreviewCapture) => void;
}) {
  const pages = detail.plan?.pages ?? [];
  const [scope, setScope] = useState<"run" | string>("run");
  const [rawDiff, setRawDiff] = useState<string>("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [find, setFind] = useState("");
  const [viewed, setViewed] = useState<Set<string>>(() => new Set());
  const [showPreview, setShowPreview] = useState(false);

  // Inline comment composer anchored to a clicked diff line (O3 batched drafts)
  const [commentAt, setCommentAt] = useState<{ file: string; line: number } | null>(null);
  const [commentText, setCommentText] = useState("");
  const { comments, drafts, sending: sendingDrafts, addDraft, removeComment, toggleResolve, sendDrafts } = useReviewComments(id);

  // Ship actions
  const [prUrl, setPrUrl] = useState<string | null>(null);
  const [prSyncReport, setPrSyncReport] = useState<PrSyncReport | null>(null);
  const [reviewJob, setReviewJob] = useState<ReviewJob | null>(null);
  const [busyAction, setBusyAction] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    setError(null);
    const req = scope === "run" ? api.diff(id, "run") : api.diff(id, "page", scope);
    req
      .then((res) => {
        if (!alive) return;
        setRawDiff(res.diff);
      })
      .catch((err) => {
        if (!alive) return;
        setRawDiff("");
        setError(err instanceof Error ? err.message : String(err));
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [id, scope, detail.phase, detail.pages.length]);

  const files = useMemo(() => parseDiff(rawDiff), [rawDiff]);
  const filteredFiles = useMemo(() => {
    const q = find.trim().toLowerCase();
    if (!q) return files;
    return files.filter(
      (f) => f.path.toLowerCase().includes(q) || f.lines.some((l) => l.text.toLowerCase().includes(q)),
    );
  }, [files, find]);

  const ownerForFile = (filePath: string): string => {
    if (scope !== "run") return `page:${scope}`;
    for (const p of pages) {
      const prefix = p.owned_glob.replace(/\*+.*$/, "").replace(/\/+$/, "");
      if (prefix && filePath.startsWith(prefix)) return `page:${p.id}`;
    }
    return pages[0] ? `page:${pages[0].id}` : "master";
  };

  const saveDraftComment = () => {
    if (!commentAt || !commentText.trim()) return;
    addDraft(commentAt.file, commentAt.line, commentText.trim(), ownerForFile(commentAt.file));
    setCommentAt(null);
    setCommentText("");
  };

  const sendAllDrafts = async () => {
    try {
      const res = await sendDrafts();
      if (res.count > 0) {
        toast(
          `Sent ${res.owners} note${res.owners === 1 ? "" : "s"}`,
          `${res.count} comment${res.count === 1 ? "" : "s"} grouped by owning worker in one turn.`,
          3000,
        );
        onReload();
      }
    } catch (err) {
      toast("Could not send review notes", err instanceof Error ? err.message : String(err));
    }
  };

  const runReview = async () => {
    setBusyAction("review");
    try {
      const started = await api.reviewRun(id);
      const job = await api.review(started.id);
      setReviewJob(job);
      toast("Adversarial review started", `Review ${started.id}`, 2500);
    } catch (err) {
      toast("Review failed to start", err instanceof Error ? err.message : String(err));
    } finally {
      setBusyAction(null);
    }
  };

  const draftPr = async () => {
    setBusyAction("pr");
    try {
      const res = await api.draftPr(id);
      setPrUrl(res.url);
      toast("Draft PR created", res.url, 3500);
    } catch (err) {
      toast("Could not draft PR", err instanceof Error ? err.message : String(err));
    } finally {
      setBusyAction(null);
    }
  };

  const syncPr = async () => {
    setBusyAction("pr-sync");
    try {
      const report = await api.prSync(id);
      setPrSyncReport(report);
      toast(
        "PR CI synced",
        report.failing_checks > 0
          ? `${report.failing_checks} failing check(s) routed to owning units`
          : `${report.checks.length} check(s) inspected`,
        3000,
      );
      if (report.queued_notes.length > 0) onReload();
    } catch (err) {
      toast("PR sync failed", err instanceof Error ? err.message : String(err));
    } finally {
      setBusyAction(null);
    }
  };

  const undoMerge = async () => {
    setBusyAction("undo");
    try {
      const res = await api.undo(id);
      toast("Merge undone", `Reverted ${res.files} files in ${res.commit}`, 3000);
      onReload();
    } catch (err) {
      toast("Could not undo merge", err instanceof Error ? err.message : String(err));
    } finally {
      setBusyAction(null);
    }
  };

  const forkRun = async () => {
    setBusyAction("fork");
    try {
      const res = await api.forkRun(id, { yes: false });
      toast("Run forked", `Opening ${res.id}`, 2500);
      navigate(`/runs/${encodeURIComponent(res.id)}`);
    } catch (err) {
      toast("Could not fork run", err instanceof Error ? err.message : String(err));
    } finally {
      setBusyAction(null);
    }
  };

  const totalStat = diffStats?.total;
  const viewedCount = files.filter((f) => viewed.has(f.path)).length;

  return (
    <div className="review-studio" data-testid="review-lens">
      {/* Ship Action Bar */}
      <header className="review-bar">
        <div className="review-bar-left">
          <span className="studio-pill tone-indigo">
            <FileDiff size={13} aria-hidden /> {files.length} file{files.length === 1 ? "" : "s"}
          </span>
          {totalStat && (
            <span className="mono small review-totals">
              <span className="diff-add">+{totalStat.added}</span>{" "}
              <span className="diff-del">-{totalStat.removed}</span>
            </span>
          )}
          <span className="muted small">
            {viewedCount}/{files.length} viewed
          </span>
          <label className="review-search">
            <Search size={13} aria-hidden />
            <input
              type="text"
              value={find}
              onChange={(e) => setFind(e.target.value)}
              placeholder="Filter files or diff lines…"
            />
          </label>
        </div>

        <div className="review-bar-actions">
          {drafts.length > 0 && (
            <button
              type="button"
              className="primary small"
              data-testid="send-review-notes"
              disabled={sendingDrafts}
              onClick={() => void sendAllDrafts()}
            >
              <Send size={13} aria-hidden />{" "}
              {sendingDrafts
                ? "Sending…"
                : `Send ${new Set(drafts.map((d) => d.owner)).size} note${new Set(drafts.map((d) => d.owner)).size === 1 ? "" : "s"} (${drafts.length})`}
            </button>
          )}
          <button
            type="button"
            className={`ghost small ${showPreview ? "on" : ""}`}
            onClick={() => setShowPreview((v) => !v)}
          >
            <Eye size={13} aria-hidden /> Split Preview
          </button>
          <button type="button" className="small" disabled={busyAction !== null} onClick={() => void runReview()}>
            <ShieldCheck size={13} aria-hidden /> {busyAction === "review" ? "Starting…" : "AI Review"}
          </button>
          {detail.plan && (
            <button type="button" className="ghost small" disabled={busyAction !== null} onClick={() => void syncPr()}>
              <RefreshCw size={13} aria-hidden /> {busyAction === "pr-sync" ? "Syncing…" : "Sync PR CI"}
            </button>
          )}
          <button type="button" className="ghost small" disabled={busyAction !== null} onClick={() => void forkRun()}>
            <GitFork size={13} aria-hidden /> Fork
          </button>
          {detail.merged && !detail.undone && !detail.active && (
            <button type="button" className="ghost small" disabled={busyAction !== null} onClick={() => void undoMerge()}>
              <RotateCcw size={13} aria-hidden /> Undo Merge
            </button>
          )}
          {detail.phase === "done" && ghReady && detail.branch && (
            <button type="button" className="primary small" disabled={busyAction !== null} onClick={() => void draftPr()}>
              <GitPullRequest size={13} aria-hidden /> {busyAction === "pr" ? "Drafting…" : "Draft PR"}
            </button>
          )}
          {prUrl && <ExternalLink href={prUrl}>Open PR</ExternalLink>}
        </div>
      </header>

      {prSyncReport && (
        <div className="review-banner" data-testid="pr-sync-banner">
          <RefreshCw size={14} aria-hidden />
          <span>
            PR CI: <strong>{prSyncReport.checks.length}</strong> check(s),{" "}
            <strong>{prSyncReport.failing_checks}</strong> failing
            {prSyncReport.queued_notes.length > 0 && ` · queued ${prSyncReport.queued_notes.length} repair note(s)`}
          </span>
          <span className="grow" />
          <button type="button" className="icon" onClick={() => setPrSyncReport(null)} aria-label="Dismiss PR sync">
            <X size={13} />
          </button>
        </div>
      )}

      {reviewJob && (
        <div className="review-banner">
          <ShieldCheck size={14} aria-hidden />
          <span>
            Review <strong className="mono">{reviewJob.id}</strong> ({reviewJob.status})
          </span>
          <button
            type="button"
            className="ghost small"
            onClick={() => navigate(`/reviews/${encodeURIComponent(reviewJob.id)}`)}
          >
            Inspect report
          </button>
        </div>
      )}

      <div className={`review-body ${showPreview ? "with-preview" : ""}`}>
        {/* Left Unit & File Rail */}
        <aside className="review-rail" aria-label="Diff scopes">
          <div className="review-rail-title">Units</div>
          <button
            type="button"
            className={`review-scope-btn ${scope === "run" ? "on" : ""}`}
            onClick={() => setScope("run")}
          >
            <span>All changes</span>
            {totalStat && <span className="mono small diff-add">+{totalStat.added}</span>}
          </button>
          {detail.pages.map((p) => {
            const stat = diffStats?.pages[p.id];
            return (
              <button
                key={p.id}
                type="button"
                className={`review-scope-btn ${scope === p.id ? "on" : ""}`}
                onClick={() => setScope(p.id)}
              >
                <span className="row gap">
                  <span className={`review-dot st-${p.state}`} />
                  <span className="clip">{p.id}</span>
                </span>
                {stat && (
                  <span className="mono small">
                    <span className="diff-add">+{stat.added}</span>
                  </span>
                )}
              </button>
            );
          })}

          {files.length > 0 && (
            <>
              <div className="review-rail-title">Changed Files</div>
              <div className="review-file-list">
                {files.map((f) => {
                  const isViewed = viewed.has(f.path);
                  return (
                    <a key={f.path} href={`#rev-file-${f.hash}`} className={`review-file-link ${isViewed ? "viewed" : ""}`}>
                      {isViewed ? <CheckCircle2 size={12} className="diff-add" /> : <FileDiff size={12} />}
                      <span className="mono clip" title={f.path}>
                        {f.path.split("/").pop()}
                      </span>
                    </a>
                  );
                })}
              </div>
            </>
          )}
        </aside>

        {/* Center Unified Diff with Inline Line Comments */}
        <div className="review-diff-pane">
          {loading && <div className="muted small review-placeholder">Loading diff…</div>}
          {!loading && error && <div className="muted small review-placeholder">{error}</div>}
          {!loading && !error && filteredFiles.length === 0 && (
            <div className="muted small review-placeholder">No diff changes in this scope yet.</div>
          )}

          {filteredFiles.map((file) => {
            const isViewed = viewed.has(file.path);
            return (
              <article key={file.path} id={`rev-file-${file.hash}`} className={`review-file-card ${isViewed ? "is-viewed" : ""}`}>
                <header className="review-file-head">
                  <div className="row gap clip">
                    <code className="mono">{file.path}</code>
                    <span className="mono small diff-add">+{file.added}</span>
                    {file.removed > 0 && <span className="mono small diff-del">-{file.removed}</span>}
                  </div>
                  <button
                    type="button"
                    className={`ghost small ${isViewed ? "on" : ""}`}
                    onClick={() =>
                      setViewed((cur) => {
                        const next = new Set(cur);
                        if (next.has(file.path)) next.delete(file.path);
                        else next.add(file.path);
                        return next;
                      })
                    }
                  >
                    <Check size={12} aria-hidden /> {isViewed ? "Viewed" : "Mark viewed"}
                  </button>
                </header>

                {!isViewed && (
                  <div className="review-lines mono">
                    {file.lines.slice(0, 400).map((line, i) => {
                      const lineNo = line.new ?? line.old;
                      const isCommenting = commentAt?.file === file.path && commentAt?.line === lineNo && line.kind !== "hunk";
                      const pinned = lineNo ? comments.filter((c) => c.file === file.path && c.line === lineNo) : [];
                      return (
                        <div key={i}>
                          <div className={`review-line kind-${line.kind}`}>
                            <button
                              type="button"
                              className="review-gutter"
                              disabled={!lineNo || line.kind === "hunk"}
                              title={lineNo ? `Comment on ${file.path}:${lineNo}` : undefined}
                              onClick={() => {
                                if (!lineNo) return;
                                setCommentAt({ file: file.path, line: lineNo });
                                setCommentText("");
                              }}
                            >
                              <span className="review-gutter-num">{lineNo ?? ""}</span>
                              {lineNo && line.kind !== "hunk" && (
                                <MessageSquarePlus size={11} className="review-gutter-plus" aria-hidden />
                              )}
                            </button>
                            <span className="review-sign">
                              {line.kind === "add" ? "+" : line.kind === "del" ? "-" : line.kind === "hunk" ? "@@" : " "}
                            </span>
                            <span className="review-code">{line.text || " "}</span>
                          </div>
                          {pinned.map((q) => (
                            <div key={q.id} className={`review-inline-comment review-pin is-${q.status}`}>
                              <div className="row between gap">
                                <span className="small">
                                  <strong className="mono">{q.status}</strong> → <span className="mono">{q.owner}</span>: {q.text}
                                </span>
                                {q.status === "draft" ? (
                                  <button type="button" className="ghost small" onClick={() => removeComment(q.id)}>
                                    Remove
                                  </button>
                                ) : (
                                  <button
                                    type="button"
                                    className="ghost small"
                                    data-testid="resolve-review-comment"
                                    onClick={() => toggleResolve(q.id)}
                                  >
                                    {q.status === "resolved" ? "Reopen" : "Resolve"}
                                  </button>
                                )}
                              </div>
                            </div>
                          ))}
                          {isCommenting && (
                            <div className="review-inline-comment">
                              <div className="row between">
                                <span className="small muted">
                                  Draft comment for <strong className="mono">{ownerForFile(file.path)}</strong> on{" "}
                                  <code className="mono">{file.path}:{lineNo}</code>
                                </span>
                                <button type="button" className="icon" onClick={() => setCommentAt(null)} aria-label="Close">
                                  <X size={13} />
                                </button>
                              </div>
                              <div className="row gap">
                                <input
                                  type="text"
                                  autoFocus
                                  value={commentText}
                                  onChange={(e) => setCommentText(e.target.value)}
                                  placeholder="Explain what should change on this line…"
                                  onKeyDown={(e) => {
                                    if (e.key === "Enter") saveDraftComment();
                                    if (e.key === "Escape") setCommentAt(null);
                                  }}
                                />
                                <button
                                  type="button"
                                  className="primary small"
                                  disabled={!commentText.trim()}
                                  onClick={saveDraftComment}
                                >
                                  <Send size={12} aria-hidden /> Add draft
                                </button>
                              </div>
                            </div>
                          )}
                        </div>
                      );
                    })}
                  </div>
                )}
              </article>
            );
          })}
        </div>

        {/* Optional Split Live Preview */}
        {showPreview && (
          <aside className="review-preview-pane">
            <LivePreview id={id} detail={detail} onCapture={onCapture} />
          </aside>
        )}
      </div>
    </div>
  );
}
