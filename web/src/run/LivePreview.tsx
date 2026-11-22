// The run's live preview: its dev server, started on request and stopped with the run.
// Not the file preview tab (a code table); this is the running app. It always opens: the
// daemon falls back to serving the project's files when there is no dev server, or when
// the dev server dies, and says so here.

import { useCallback, useEffect, useRef, useState } from "react";
import { Crosshair, MonitorPlay, Play, Square, ExternalLink as Out } from "lucide-react";
import { api, type PreviewCapture, type PreviewView, type RunDetail } from "../api";
import { toast } from "../overlay";

const TONE: Record<PreviewView["state"], string> = { none: "quiet", starting: "ask", running: "good", stopped: "quiet", failed: "bad" };

const KEY_STYLES = [
  "display",
  "position",
  "color",
  "background-color",
  "font-family",
  "font-size",
  "font-weight",
  "line-height",
  "padding",
  "margin",
  "border",
  "border-radius",
  "width",
  "height",
  "gap",
] as const;

function buildCssSelector(el: Element): string {
  if (el.id) return `#${CSS.escape ? CSS.escape(el.id) : el.id}`;
  const parts: string[] = [];
  let cur: Element | null = el;
  while (cur && cur.nodeType === 1 && cur.tagName.toLowerCase() !== "html") {
    const tag = cur.tagName.toLowerCase();
    if (cur.id) {
      parts.unshift(`#${CSS.escape ? CSS.escape(cur.id) : cur.id}`);
      break;
    }
    let seg = tag;
    const cls = typeof cur.className === "string"
      ? cur.className
          .trim()
          .split(/\s+/)
          .filter((c) => c && !c.startsWith("tm-design-"))
          .slice(0, 2)
      : [];
    if (cls.length) seg += "." + cls.join(".");
    const parent: Element | null = cur.parentElement;
    if (parent) {
      const sameTag = Array.from(parent.children).filter((c) => c.tagName === cur!.tagName);
      if (sameTag.length > 1) {
        seg += `:nth-of-type(${sameTag.indexOf(cur) + 1})`;
      }
    }
    parts.unshift(seg);
    if (tag === "body") break;
    cur = parent;
  }
  return parts.join(" > ");
}

function sourceHint(el: Element): string | null {
  let cur: Element | null = el;
  while (cur) {
    for (const attr of ["data-source", "data-file", "data-source-file", "data-component-path"]) {
      const val = cur.getAttribute(attr);
      if (val && val.trim()) return val.trim();
    }
    cur = cur.parentElement;
  }
  return null;
}

function previewRoutePath(id: string, pathname: string): string {
  const prefix = `/api/runs/${encodeURIComponent(id)}/preview/proxy`;
  let rel = pathname;
  if (rel.startsWith(prefix)) rel = rel.slice(prefix.length);
  if (!rel.startsWith("/")) rel = "/" + rel;
  return rel || "/";
}

export function LivePreview({
  id,
  detail,
  onCapture,
}: {
  id: string;
  detail: RunDetail;
  onCapture?: (capture: PreviewCapture) => void;
}) {
  const [view, setView] = useState<PreviewView | null>(null);
  const [busy, setBusy] = useState(false);
  const [frame, setFrame] = useState(false);
  const [designMode, setDesignMode] = useState(false);
  const [capturing, setCapturing] = useState(false);
  const iframeRef = useRef<HTMLIFrameElement | null>(null);
  const cleanupRef = useRef<(() => void) | null>(null);
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
    setDesignMode(false);
    load();
  }, [load]);
  // Follow the server from starting to running, or to its exit.
  useEffect(() => {
    if (!live) return;
    const t = window.setInterval(load, 1200);
    return () => window.clearInterval(t);
  }, [live, load]);

  const detachDesignListeners = useCallback(() => {
    cleanupRef.current?.();
    cleanupRef.current = null;
  }, []);

  const attachDesignListeners = useCallback(() => {
    detachDesignListeners();
    if (!designMode) return;
    const iframe = iframeRef.current;
    if (!iframe) return;
    let doc: Document | null = null;
    try {
      doc = iframe.contentDocument;
    } catch {
      return;
    }
    if (!doc || !doc.body) return;

    const overlay = doc.createElement("div");
    overlay.setAttribute("data-tm-design-overlay", "true");
    overlay.style.position = "fixed";
    overlay.style.pointerEvents = "none";
    overlay.style.zIndex = "2147483647";
    overlay.style.border = "2px solid #ffffff";
    overlay.style.outline = "1px solid #111111";
    overlay.style.background = "rgba(255, 255, 255, 0.12)";
    overlay.style.boxSizing = "border-box";
    overlay.style.display = "none";
    doc.body.appendChild(overlay);

    const onMove = (e: MouseEvent) => {
      const target = e.target instanceof Element ? e.target : null;
      if (!target || target === overlay || target === doc!.documentElement || target === doc!.body) {
        overlay.style.display = "none";
        return;
      }
      const rect = target.getBoundingClientRect();
      overlay.style.display = "block";
      overlay.style.left = `${rect.left}px`;
      overlay.style.top = `${rect.top}px`;
      overlay.style.width = `${rect.width}px`;
      overlay.style.height = `${rect.height}px`;
    };

    const onLeave = () => {
      overlay.style.display = "none";
    };

    const onClick = (e: MouseEvent) => {
      const target = e.target instanceof Element ? e.target : null;
      if (!target || target === overlay) return;
      e.preventDefault();
      e.stopPropagation();

      const selector = buildCssSelector(target);
      const outer_html = (target.outerHTML || "").trim().slice(0, 1200);
      const computed = doc!.defaultView?.getComputedStyle(target);
      const styles: Record<string, string> = {};
      if (computed) {
        for (const prop of KEY_STYLES) {
          const v = computed.getPropertyValue(prop).trim();
          if (v) styles[prop] = v;
        }
      }
      const path = previewRoutePath(id, doc!.location?.pathname || "/");
      const source_file = sourceHint(target);
      const text = (target.textContent || "").trim().slice(0, 200) || null;

      setCapturing(true);
      void api
        .previewCapture(id, { selector, outer_html, styles, path, source_file, text })
        .then((capture) => {
          onCapture?.(capture);
          toast(`Captured ${capture.selector}`, `Routed to ${capture.owner}`, 2500);
        })
        .catch((err) => {
          toast("Could not capture element", err instanceof Error ? err.message : String(err));
        })
        .finally(() => setCapturing(false));
    };

    doc.addEventListener("mousemove", onMove, true);
    doc.addEventListener("mouseleave", onLeave, true);
    doc.addEventListener("click", onClick, true);

    cleanupRef.current = () => {
      doc?.removeEventListener("mousemove", onMove, true);
      doc?.removeEventListener("mouseleave", onLeave, true);
      doc?.removeEventListener("click", onClick, true);
      overlay.remove();
    };
  }, [designMode, detachDesignListeners, id, onCapture]);

  useEffect(() => {
    if (!designMode || !frame) {
      detachDesignListeners();
      return;
    }
    attachDesignListeners();
    return detachDesignListeners;
  }, [designMode, frame, attachDesignListeners, detachDesignListeners]);

  if (!view) return null;

  const act = async (start: boolean) => {
    setBusy(true);
    try {
      setView(await (start ? api.startPreview(id) : api.stopPreview(id)));
      setFrame(start);
      if (!start) setDesignMode(false);
    } catch (err) {
      toast(start ? "The preview did not start" : "The preview did not stop", err instanceof Error ? err.message : String(err));
      load();
    } finally {
      setBusy(false);
    }
  };

  const toggleDesignMode = () => {
    setDesignMode((on) => {
      const next = !on;
      if (next) setFrame(true);
      return next;
    });
  };

  const frameSrc = designMode ? api.previewProxyUrl(id) : view.url ?? "";

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
            <>
              <button
                type="button"
                className={`ghost small ${designMode ? "on" : ""}`}
                aria-pressed={designMode}
                disabled={capturing}
                onClick={toggleDesignMode}
                data-testid="design-mode-toggle"
                title="Hover to highlight and click any element to capture its selector and styles into the composer"
              >
                <Crosshair size={12} aria-hidden /> {designMode ? "Design mode on" : "Design mode"}
              </button>
              <button type="button" className="ghost small" onClick={() => setFrame((f) => !f)}>
                {frame ? "Hide frame" : "Show here"}
              </button>
            </>
          )}
          {live ? (
            <button type="button" className="small" disabled={busy} onClick={() => void act(false)} data-testid="live-preview-stop">
              <Square size={12} aria-hidden /> Stop
            </button>
          ) : (
            <button type="button" className="small" disabled={busy || !!reason} title={reason ?? "Start the project's dev server for this run"} onClick={() => void act(true)} data-testid="live-preview-start">
              <Play size={12} aria-hidden /> {view.state === "none" ? "Start preview" : "Start again"}
            </button>
          )}
        </div>
      </div>
      {designMode && live && view.state === "running" && (
        <div className="small muted" data-testid="design-mode-hint">
          Click any element in the preview to attach its CSS selector, HTML, and styles to the composer.
        </div>
      )}
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
      {/* Only once it answers: a frame loaded while the server is starting stays broken. */}
      {frame && view.state === "running" && frameSrc && (
        <iframe
          ref={iframeRef}
          key={`${designMode ? "proxy" : "direct"}:${view.url}:${view.started ?? ""}`}
          className={`live-preview-frame ${designMode ? "is-design-mode" : ""}`}
          title="Live preview"
          src={frameSrc}
          data-testid="live-preview-frame"
          onLoad={() => {
            if (designMode) attachDesignListeners();
          }}
          style={{ height: 320 }}
        />
      )}
    </section>
  );
}
