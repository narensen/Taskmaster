// The run's live preview: its dev server, started on request and stopped with the run.
// Not the file preview tab (a code table); this is the running app. It always opens: the
// daemon falls back to serving the project's files when there is no dev server, or when
// the dev server dies, and says so here.

import { useCallback, useEffect, useState } from "react";
import { MonitorPlay, Play, Square, ExternalLink as Out } from "lucide-react";
import { api, type PreviewView, type RunDetail } from "../api";
import { toast } from "../overlay";

const TONE: Record<PreviewView["state"], string> = { none: "quiet", starting: "ask", running: "good", stopped: "quiet", failed: "bad" };

export function LivePreview({ id, detail }: { id: string; detail: RunDetail }) {
  const [view, setView] = useState<PreviewView | null>(null);
  const [busy, setBusy] = useState(false);
  const [frame, setFrame] = useState(false);
  const live = view?.state === "starting" || view?.state === "running";
  const passed = detail.pages.some((p) => p.state === "passed");
  const building = (detail.phase === "planning" || detail.phase === "awaiting_approval") && !passed;
  const reason = building ? "Nothing to preview until the run has started building." : null;
  const hint = !building && detail.active ? "The run is still building: the preview shows what has passed so far." : null;

  const load = useCallback(() => {
    api.preview(id).then(setView).catch(() => {});
  }, [id]);
  useEffect(() => {
    setView(null);
    setFrame(false);
    load();
  }, [load]);
  // Follow the server from starting to running, or to its exit.
  useEffect(() => {
    if (!live) return;
    const t = window.setInterval(load, 1200);
    return () => window.clearInterval(t);
  }, [live, load]);

  if (!view) return null;

  const act = async (start: boolean) => {
    setBusy(true);
    try {
      setView(await (start ? api.startPreview(id) : api.stopPreview(id)));
      setFrame(start);
    } catch (err) {
      toast(start ? "The preview did not start" : "The preview did not stop", err instanceof Error ? err.message : String(err));
      load();
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="live-preview" data-testid="live-preview" data-state={view.state}>
      <div className="row between gap wrap">
        <div className="row gap">
          <span className="lp-ico" aria-hidden>
            <MonitorPlay size={14} />
          </span>
          <span className="strong">Live preview</span>
          <span className={`badge ${TONE[view.state]}`} data-testid="live-preview-state">
            {view.state === "none" ? "not started" : view.state}
          </span>
          {live && view.url && (
            <a href={view.url} target="_blank" rel="noreferrer" className="small row gap lp-url mono" data-testid="live-preview-url">
              {view.url} <Out size={12} aria-hidden />
            </a>
          )}
        </div>
        <div className="row gap">
          {live && view.state === "running" && (
            <button className="ghost small" onClick={() => setFrame((f) => !f)}>
              {frame ? "Hide frame" : "Show here"}
            </button>
          )}
          {live ? (
            <button className="small" disabled={busy} onClick={() => void act(false)} data-testid="live-preview-stop">
              <Square size={12} aria-hidden /> Stop
            </button>
          ) : (
            <button className="small" disabled={busy || !!reason} title={reason ?? "Start the project's dev server for this run"} onClick={() => void act(true)} data-testid="live-preview-start">
              <Play size={12} aria-hidden /> {view.state === "none" ? "Start preview" : "Start again"}
            </button>
          )}
        </div>
      </div>
      {!live && reason && <div className="muted small">{reason}</div>}
      {hint && <div className="muted small">{hint}</div>}
      {live && view.reason && <div className="small lp-note" data-testid="live-preview-note">{view.reason}</div>}
      {view.command && <div className="muted small clip mono lp-cmd">$ {view.command}</div>}
      {view.state === "failed" && (
        <div className="live-preview-fail small" data-testid="live-preview-failure">
          <div className="strong">{view.reason ?? "The dev server failed."}</div>
          {view.output && <pre>{view.output}</pre>}
        </div>
      )}
      {frame && live && view.url && (
        <iframe
          className="live-preview-frame"
          title="Live preview"
          src={view.url}
          data-testid="live-preview-frame"
          style={{ height: 320 }}
        />
      )}
    </section>
  );
}
