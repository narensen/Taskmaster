// A small screen pinned to the bottom right of a run. It shows the app this run
// is building: the live server once the run has settled, otherwise the latest
// screenshot, otherwise the route and the last real action.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ExternalLink as Out, Maximize2, Minimize2 } from "lucide-react";
import { api, type AgentEntry, type PreviewView, type RunDetail } from "../api";

function routesOf(detail: RunDetail): { page: string; route: string }[] {
  const fromPlan = detail.plan?.pages.map((p) => ({ page: p.id, route: p.route })) ?? [];
  if (fromPlan.length) return fromPlan;
  return detail.pages.map((p) => ({ page: p.id, route: p.route ?? "/" }));
}

function pageOf(entry: AgentEntry): string | null {
  const id = entry.agent.id;
  if (id.startsWith("page:")) {
    const page = id.slice("page:".length).split("/")[0];
    return page || null;
  }
  return entry.agent.page ?? null;
}

/** A dev server that renders the app. A bare file server is not one: its routes 404. */
function appServer(view: PreviewView | null, route: string): string | null {
  if (view?.state !== "running" || !view.url) return null;
  const command = view.command ?? "";
  if (/http\.server/i.test(command)) return null;
  const origin = view.url.replace(/\/+$/, "");
  if (/static file server/i.test(command)) return `${origin}/`;
  return `${origin}${route.startsWith("/") ? route : `/${route}`}`;
}

/**
 * The page as written, when nothing is serving it yet. Counts in an empty list
 * become 0. Anything we cannot read stays out of the frame.
 */
function sketchJsx(source: string): string | null {
  const marked =
    source.match(/return\s*\(\s*([\s\S]*?)\n\s*\)\s*;/)?.[1] ??
    source.match(/return\s*(<[\s\S]*?<\/[A-Za-z0-9.]+>)\s*;/)?.[1];
  if (!marked || !marked.includes("<")) return null;
  const lengths = new Map<string, number>();
  for (const match of source.matchAll(/const\s+(\w+)(?:\s*:[^=]+)?\s*=\s*\[([\s\S]*?)\]/g)) {
    const name = match[1];
    const inner = match[2]?.trim() ?? "";
    if (!name) continue;
    lengths.set(name, inner ? inner.split(",").filter((part) => part.trim()).length : 0);
  }
  let html = marked.replace(/\{\s*\/\*[\s\S]*?\*\/\s*\}/g, "");
  html = html.replace(/\{\s*(\w+)\.length\s*\}/g, (full, name: string) => (lengths.has(name) ? String(lengths.get(name)) : full));
  html = html.replace(/\bclassName=/g, "class=").replace(/\bhtmlFor=/g, "for=");
  return `<!doctype html><html><head><meta charset="utf-8"><style>html,body{margin:0;background:#fff;color:#1c1b18;font:16px/1.45 Inter,ui-sans-serif,system-ui,sans-serif}body{padding:28px}</style></head><body>${html}</body></html>`;
}

function pageDocument(source: string): string | null {
  if (/^\s*<!doctype html/i.test(source) || /<html[\s>]/i.test(source)) return source;
  if (/^\s*</.test(source) && /<\/[a-z0-9]+>\s*$/i.test(source)) {
    return `<!doctype html><html><head><meta charset="utf-8"></head><body>${source}</body></html>`;
  }
  return sketchJsx(source);
}

/** The latest tool, command, browser check, or thought. Nothing is invented. */
function captionOf(entries: AgentEntry[]): string {
  for (let i = entries.length - 1; i >= 0; i--) {
    const entry = entries[i];
    if (!entry) continue;
    if (entry.kind === "tool_call") {
      const name = entry.name ?? entry.tool ?? "tool";
      return entry.target ? `${name} ${entry.target}` : name;
    }
    if (entry.kind === "command") return entry.command ? `$ ${entry.command}` : "command";
    if (entry.kind === "preview") {
      const line = entry.step || entry.text;
      return line?.trim() || "browser check";
    }
    if (entry.kind === "thought") {
      const line = entry.text?.split("\n").find((part) => part.trim());
      if (line) return line.trim().slice(0, 88);
    }
  }
  return "";
}

export function Stage({ id, detail, entries }: { id: string; detail: RunDetail; entries: AgentEntry[] }) {
  const [view, setView] = useState<PreviewView | null>(null);
  const [grown, setGrown] = useState(false);
  const [picked, setPicked] = useState<string | null>(null);
  const [shots, setShots] = useState<string[]>([]);
  const [img, setImg] = useState<string | null>(null);
  const [doc, setDoc] = useState<string | null>(null);
  const [lift, setLift] = useState(76);
  const started = useRef(false);
  const filesRef = useRef<string[] | null>(null);
  const root = useRef<HTMLElement>(null);

  const routes = useMemo(() => routesOf(detail), [detail]);
  const followed = useMemo(() => {
    for (let i = entries.length - 1; i >= 0; i--) {
      const entry = entries[i];
      if (!entry) continue;
      const page = pageOf(entry);
      if (page && routes.some((route) => route.page === page)) return page;
    }
    return routes[0]?.page ?? null;
  }, [entries, routes]);
  const page = picked && routes.some((route) => route.page === picked) ? picked : followed;
  const route = routes.find((item) => item.page === page)?.route ?? "/";
  const caption = useMemo(() => captionOf(entries), [entries]);

  const load = useCallback(() => {
    api.preview(id).then(setView).catch(() => {});
  }, [id]);

  useEffect(() => {
    started.current = false;
    filesRef.current = null;
    setPicked(null);
    setGrown(false);
    setDoc(null);
  }, [id]);

  useEffect(() => {
    load();
  }, [load, detail.phase, detail.active]);

  useEffect(() => {
    if (view?.state !== "starting" && view?.state !== "running") return;
    const timer = window.setInterval(load, 1500);
    return () => window.clearInterval(timer);
  }, [view?.state, load]);

  useEffect(() => {
    let stop = false;
    const pull = () => {
      api
        .screens(id)
        .then((pages) => {
          if (stop) return;
          setShots((prev) => (prev.join("\n") === pages.join("\n") ? prev : pages));
        })
        .catch(() => {});
    };
    pull();
    if (!detail.active) return () => {
      stop = true;
    };
    const timer = window.setInterval(pull, 2000);
    return () => {
      stop = true;
      window.clearInterval(timer);
    };
  }, [id, detail.phase, detail.active]);

  useEffect(() => {
    const which = page && shots.includes(page) ? page : shots[shots.length - 1];
    if (!which) {
      setImg(null);
      return;
    }
    let url: string | null = null;
    let dead = false;
    api
      .screen(id, which)
      .then((next) => {
        if (dead) {
          URL.revokeObjectURL(next);
          return;
        }
        url = next;
        setImg(next);
      })
      .catch(() => {
        if (!dead) setImg(null);
      });
    return () => {
      dead = true;
      if (url) URL.revokeObjectURL(url);
    };
  }, [id, page, shots]);

  const owned = detail.plan?.pages.find((item) => item.id === page)?.owned_glob ?? "";
  useEffect(() => {
    const prefix = owned.replace(/\/\*\*$/, "").replace(/^\.\//, "") || (page ? `app/${page}` : "");
    if (!page || !prefix) {
      setDoc(null);
      return;
    }
    let dead = false;
    void (async () => {
      const files = filesRef.current ?? (await api.files());
      if (dead) return;
      filesRef.current = files;
      const under = files.filter((file) => file === prefix || file.startsWith(`${prefix}/`));
      const pick =
        under.find((file) => /\/page\.(tsx|jsx|html)$/.test(file)) ??
        under.find((file) => /\/index\.(html|tsx|jsx)$/.test(file)) ??
        under.find((file) => /\.(html|tsx|jsx|svelte|vue)$/.test(file));
      if (!pick) {
        setDoc(null);
        return;
      }
      const file = await api.file(pick);
      if (!dead) setDoc(pageDocument(file.content));
    })().catch(() => {
      if (!dead) setDoc(null);
    });
    return () => {
      dead = true;
    };
  }, [page, owned, entries.length]);

  // One attempt, after there is something to serve. A missing dev script stays quiet.
  useEffect(() => {
    if (started.current || !view?.available || view.state !== "none") return;
    const blocked =
      detail.phase === "planning" || detail.phase === "awaiting_approval" || (detail.active && detail.phase !== "done");
    if (blocked) return;
    started.current = true;
    api.startPreview(id).then(setView).catch(() => {});
  }, [view, detail.phase, detail.active, id]);

  useEffect(() => {
    const main = root.current?.parentElement;
    const bar = main?.querySelector<HTMLElement>(".composer-bar");
    const scroll = main?.querySelector<HTMLElement>(".run-scroll");
    const stage = root.current;
    if (!bar || !scroll || !stage) return;
    const place = () => {
      setLift(Math.ceil(bar.getBoundingClientRect().height) + 12);
      scroll.style.setProperty("--stage-pad", `${Math.ceil(stage.getBoundingClientRect().height) + 24}px`);
    };
    place();
    const observer = new ResizeObserver(place);
    observer.observe(bar);
    observer.observe(stage);
    return () => {
      observer.disconnect();
      scroll.style.removeProperty("--stage-pad");
    };
  }, [grown, id]);

  const frameUrl = appServer(view, route);
  const shown = frameUrl ? "live" : img ? "shot" : doc ? "page" : null;
  const status = shown ?? (detail.active || detail.phase === "planning" || detail.phase === "awaiting_approval" ? "building" : "idle");

  return (
    <aside ref={root} className={`stage ${grown ? "grown" : ""}`} data-testid="stage" data-status={status} style={{ bottom: lift }}>
      <div className="stage-bar">
        <span className="stage-route clip" title={route}>
          {route}
        </span>
        <span className="stage-status">{status}</span>
        <span className="grow" />
        <button type="button" className="stage-ico" aria-label={grown ? "Smaller screen" : "Larger screen"} onClick={() => setGrown((value) => !value)}>
          {grown ? <Minimize2 size={12} strokeWidth={1.5} /> : <Maximize2 size={12} strokeWidth={1.5} />}
        </button>
        {frameUrl && (
          <a className="stage-ico" href={frameUrl} target="_blank" rel="noreferrer" aria-label="Open the preview">
            <Out size={12} strokeWidth={1.5} />
          </a>
        )}
      </div>
      {routes.length > 1 && (
        <div className="stage-routes" role="tablist" aria-label="Pages">
          {routes.map((item) => (
            <button key={item.page} type="button" role="tab" aria-selected={item.page === page} className={item.page === page ? "on" : ""} onClick={() => setPicked(item.page)}>
              {item.route}
            </button>
          ))}
        </div>
      )}
      <button type="button" className="stage-screen" aria-label={grown ? "Shrink the preview" : "Grow the preview"} onClick={() => setGrown((value) => !value)}>
        {frameUrl ? (
          <iframe title="The app this run is building" src={frameUrl} tabIndex={-1} sandbox="allow-scripts allow-same-origin allow-forms" />
        ) : img ? (
          <img src={img} alt="" />
        ) : doc ? (
          <iframe title="The page this run is writing" srcDoc={doc} tabIndex={-1} sandbox="" />
        ) : (
          <div className="stage-empty">
            <div className="stage-empty-route">{route}</div>
            {caption ? <div className="stage-caption clip">{caption}</div> : <div className="stage-caption">Waiting for a page</div>}
          </div>
        )}
      </button>
      {shown && caption && <div className="stage-foot clip">{caption}</div>}
    </aside>
  );
}
