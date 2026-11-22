// Sophisticated 2-mode run stop control:
// - Prevents accidental 1-click run destruction while workers are building.
// - Offers Graceful stop ("finish current step, then stop") vs Immediate force stop
//   (with inline 2-step arm confirmation during active builds).
// - Shows a live draining pill with 1-click escalation to Force stop.
// - Includes a targeted per-worker Restart escape hatch right inside the menu so a
//   single stuck worker can be bounced without cancelling the whole run.

import * as Popover from "@radix-ui/react-popover";
import { ChevronDown, Clock, GitBranch, Loader2, RotateCw, Square, Zap } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, type CancelMode, type RunDetail } from "../api";
import { phaseLabel, spendLabel, usd } from "../format";
import { toast } from "../overlay";

export interface ActiveWorkerItem {
  id: string;
  page: string;
  label: string;
  headline?: string;
}

function isMac(): boolean {
  return typeof navigator !== "undefined" && /Mac|iPhone|iPad|iPod/i.test(navigator.platform || navigator.userAgent);
}

function isTypingTarget(target: EventTarget | null): boolean {
  const el = target instanceof Element ? target : null;
  return !!el?.closest("input, textarea, select, [contenteditable='true'], .xterm, [role='dialog']");
}

export function CancelControl({
  id,
  detail,
  activeWorkers,
  onCancelled,
  onRestartWorker,
}: {
  id: string;
  detail: RunDetail;
  activeWorkers: ActiveWorkerItem[];
  onCancelled: (mode: CancelMode) => void;
  onRestartWorker?: (page: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [armedForce, setArmedForce] = useState(false);
  const [busy, setBusy] = useState<CancelMode | null>(null);
  const [localMode, setLocalMode] = useState<CancelMode | null>(null);
  const armTimer = useRef<number | null>(null);

  const effectiveMode: CancelMode | null = detail.active ? (detail.cancelling ?? localMode) : null;

  // Clear optimistic local mode once the run stops or changes id.
  useEffect(() => {
    setLocalMode(null);
    setBusy(null);
    setArmedForce(false);
    setOpen(false);
  }, [id]);

  useEffect(() => {
    if (!detail.active || detail.cancelled) {
      setLocalMode(null);
      setBusy(null);
      setArmedForce(false);
      setOpen(false);
    }
  }, [detail.active, detail.cancelled]);

  // Reset inline force-arm confirmation when popover closes or after 6 seconds.
  useEffect(() => {
    if (!open) setArmedForce(false);
  }, [open]);

  useEffect(() => {
    if (!armedForce) return;
    armTimer.current = window.setTimeout(() => setArmedForce(false), 6000);
    return () => {
      if (armTimer.current !== null) window.clearTimeout(armTimer.current);
    };
  }, [armedForce]);

  // Close popover on viewport resize so it never detaches awkwardly.
  useEffect(() => {
    if (!open) return;
    const close = () => setOpen(false);
    window.addEventListener("resize", close);
    return () => window.removeEventListener("resize", close);
  }, [open]);

  const canGraceful = detail.active && detail.phase === "building";
  const needsForceArm = detail.active && (detail.phase === "building" || activeWorkers.length > 0);
  const shortcut = useMemo(() => (isMac() ? "⌘." : "Ctrl+."), []);

  const passedCount = useMemo(() => detail.pages.filter((p) => p.state === "passed").length, [detail.pages]);
  const totalPages = detail.pages.length;

  const sendCancel = useCallback(
    async (mode: CancelMode) => {
      if (busy) return;
      setBusy(mode);
      setArmedForce(false);
      try {
        const res = await api.cancel(id, mode);
        const applied = res?.mode ?? mode;
        setLocalMode(applied);
        setOpen(false);
        onCancelled(applied);
      } catch (err) {
        const message = err instanceof Error ? err.message : "Could not cancel the run";
        toast("Run was not stopped", message);
      } finally {
        setBusy(null);
      }
    },
    [busy, id, onCancelled],
  );

  // Global shortcut: ⌘. / Ctrl+. opens the Stop control while the run is active or awaiting approval.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "." || !(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey) return;
      if (isTypingTarget(e.target)) return;
      if (!detail.active && detail.phase !== "awaiting_approval") return;
      e.preventDefault();
      if (effectiveMode === "graceful") {
        void sendCancel("force");
      } else if (!effectiveMode) {
        setOpen((v) => !v);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [detail.active, detail.phase, effectiveMode, sendCancel]);

  // State C: Force stop in progress
  if (effectiveMode === "force") {
    return (
      <div className="cancel-draining" role="status" aria-live="polite" data-testid="cancel-stopping">
        <span className="cancel-draining-label">
          <Loader2 size={12} strokeWidth={1.9} className="spin-ico" aria-hidden />
          <span>Stopping…</span>
        </span>
      </div>
    );
  }

  // State B: Graceful draining active — show live status + instant Force stop escalation
  if (effectiveMode === "graceful") {
    return (
      <div className="cancel-draining" role="status" aria-live="polite" data-testid="cancel-draining">
        <span
          className="cancel-draining-label"
          title="Queued units are paused; waiting for active workers to finish and save cleanly"
        >
          <Loader2 size={12} strokeWidth={1.9} className="spin-ico" aria-hidden />
          <span>Stopping after step…</span>
        </span>
        <span className="cancel-draining-sep" aria-hidden />
        <button
          type="button"
          className="cancel-escalate"
          disabled={busy !== null}
          data-testid="cancel-force-escalate"
          title={`Abort active workers immediately (${shortcut})`}
          onClick={() => void sendCancel("force")}
        >
          <Zap size={11} strokeWidth={2} aria-hidden />
          <span>Force stop</span>
        </button>
      </div>
    );
  }

  const workerCount = activeWorkers.length;
  const forceTitle =
    detail.phase === "awaiting_approval"
      ? "Cancel run"
      : detail.phase === "planning"
        ? "Stop planning now"
        : "Stop immediately";
  const forceDesc =
    detail.phase === "awaiting_approval"
      ? "Discard this draft plan before any files are changed."
      : detail.phase === "planning"
        ? "Interrupt planning immediately and close the session."
        : "Aborts active agents, sandboxes, and worktrees now. Passed pages stay resumable.";

  // State A: Normal trigger + sophisticated Popover menu
  return (
    <Popover.Root open={open} onOpenChange={setOpen} modal={false}>
      <Popover.Trigger asChild>
        <button
          type="button"
          className="ghost small cancel-trigger"
          data-testid="cancel-trigger"
          aria-haspopup="dialog"
          aria-expanded={open}
          title={`Stop run (${shortcut})`}
        >
          <Square size={10} strokeWidth={2.2} className="cancel-sq" aria-hidden />
          <span>Stop</span>
          <ChevronDown size={11} strokeWidth={1.8} className="cancel-chev" aria-hidden />
        </button>
      </Popover.Trigger>
      {open && (
        <Popover.Portal>
          <Popover.Content
            asChild
            sideOffset={6}
            align="end"
            collisionPadding={12}
            sticky="always"
            hideWhenDetached
          >
            <div className="pop cancel-pop" role="dialog" aria-label="Stop run options" data-testid="cancel-popover">
              <div className="cancel-head">
                <div className="cancel-head-row">
                  <span className="cancel-head-title">{phaseLabel(detail)}</span>
                  <span className="cancel-head-meta mono">
                    {spendLabel(detail.spent)}
                    {detail.budget_usd && detail.budget_usd > 0 ? ` / ${usd(detail.budget_usd)}` : ""}
                  </span>
                </div>
                <div className="cancel-head-sub">
                  {totalPages > 0 ? (
                    <span>
                      {passedCount}/{totalPages} passed
                      {workerCount > 0 ? ` · ${workerCount} active` : ""}
                    </span>
                  ) : detail.phase === "awaiting_approval" ? (
                    <span>No files modified yet</span>
                  ) : (
                    <span>In progress</span>
                  )}
                  {detail.branch && (
                    <span className="cancel-branch mono clip" title={`Branch ${detail.branch} stays resumable`}>
                      <GitBranch size={11} aria-hidden />
                      <span>{detail.branch}</span>
                    </span>
                  )}
                </div>
              </div>

              <div className="cancel-actions">
                {canGraceful && (
                  <button
                    type="button"
                    className="cancel-opt"
                    disabled={busy !== null}
                    data-testid="cancel-graceful"
                    onClick={() => void sendCancel("graceful")}
                  >
                    <Clock size={14} strokeWidth={1.8} className="cancel-opt-ico" aria-hidden />
                    <span className="cancel-opt-body">
                      <span className="cancel-opt-title">Finish current step, then stop</span>
                      <span className="cancel-opt-desc">
                        Stops queued units now; lets{" "}
                        {workerCount > 0
                          ? `${workerCount} active worker${workerCount === 1 ? "" : "s"}`
                          : "in-flight work"}{" "}
                        finish and save cleanly.
                      </span>
                    </span>
                  </button>
                )}

                {armedForce ? (
                  <div className="cancel-arm" role="alert" data-testid="cancel-force-confirm">
                    <div className="cancel-arm-copy">
                      <span className="cancel-arm-title">Abort in-flight work now?</span>
                      <span className="cancel-arm-desc">
                        Active workers stop immediately.{" "}
                        {passedCount > 0
                          ? `${passedCount} passed ${passedCount === 1 ? "page stays" : "pages stay"} resumable.`
                          : "You can resume this run later."}
                      </span>
                    </div>
                    <div className="cancel-arm-btns">
                      <button
                        type="button"
                        className="ghost small"
                        disabled={busy !== null}
                        onClick={() => setArmedForce(false)}
                      >
                        Keep running
                      </button>
                      <button
                        type="button"
                        className="small cancel-arm-go"
                        disabled={busy !== null}
                        data-testid="cancel-force-submit"
                        onClick={() => void sendCancel("force")}
                      >
                        {busy === "force" ? (
                          <Loader2 size={12} className="spin-ico" aria-hidden />
                        ) : (
                          <Zap size={12} aria-hidden />
                        )}
                        <span>Confirm stop</span>
                      </button>
                    </div>
                  </div>
                ) : (
                  <button
                    type="button"
                    className="cancel-opt cancel-opt-force"
                    disabled={busy !== null}
                    data-testid="cancel-force"
                    onClick={() => {
                      if (needsForceArm) setArmedForce(true);
                      else void sendCancel("force");
                    }}
                  >
                    <Zap size={14} strokeWidth={1.8} className="cancel-opt-ico" aria-hidden />
                    <span className="cancel-opt-body">
                      <span className="cancel-opt-title">{forceTitle}</span>
                      <span className="cancel-opt-desc">{forceDesc}</span>
                    </span>
                  </button>
                )}
              </div>

              {onRestartWorker && workerCount > 0 && (
                <div className="cancel-workers">
                  <div className="cancel-section-label">Or restart one worker</div>
                  {activeWorkers.slice(0, 4).map((w) => (
                    <div key={w.id} className="cancel-worker-row">
                      <div className="cancel-worker-info">
                        <span className="cancel-worker-name mono clip">{w.label}</span>
                        {w.headline && <span className="cancel-worker-head clip">{w.headline}</span>}
                      </div>
                      <button
                        type="button"
                        className="ghost small cancel-worker-btn"
                        title={`Restart only ${w.label}`}
                        onClick={() => {
                          setOpen(false);
                          onRestartWorker(w.page);
                        }}
                      >
                        <RotateCw size={11} strokeWidth={1.9} aria-hidden />
                        <span>Restart</span>
                      </button>
                    </div>
                  ))}
                </div>
              )}

              <div className="cancel-foot">
                <span>Passed work stays resumable</span>
                <span className="cancel-kbd mono">{shortcut}</span>
              </div>
            </div>
          </Popover.Content>
        </Popover.Portal>
      )}
    </Popover.Root>
  );
}
