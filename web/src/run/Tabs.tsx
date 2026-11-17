// The right panel's tabs other than the diff: the browser preview and screenshots, web
// search, the terminal transcript, and the read-only file preview.

import { Fragment, useEffect, useMemo, useRef, useState } from "react";
import { api, searchWeb, type AgentEntry, type RunDetail } from "../api";
import { ExternalLink, Rich, label } from "../paths";
import { emit, openMenu, toast } from "../overlay";
import { usePrefs } from "../prefs";

// Browser.

export function BrowserTab({ run, detail, entries }: { run: string; detail: RunDetail | null; entries: AgentEntry[] }) {
  const routes = useMemo(() => {
    const fromPlan = detail?.plan?.pages.map((p) => ({ page: p.id, route: p.route })) ?? [];
    return fromPlan.length ? fromPlan : (detail?.pages ?? []).map((p) => ({ page: p.id, route: p.route ?? "/" }));
  }, [detail]);
  const [page, setPage] = useState<string>("");
  const [shots, setShots] = useState<string[]>([]);
  const [img, setImg] = useState<string | null>(null);
  const key = `taskmaster.preview.${run}`;
  const [base, setBase] = useState(() => localStorage.getItem(key) ?? "");
  const [pane, setPane] = useState<string | null>(null);
  const checks = entries.filter((e) => e.kind === "preview");

  useEffect(() => {
    api.screens(run).then(setShots).catch(() => setShots([]));
  }, [run, detail?.phase]);
  useEffect(() => {
    if (!page && routes[0]) setPage(routes[0].page);
  }, [routes, page]);
  useEffect(() => {
    setImg(null);
    if (!page || !shots.includes(page)) return;
    let url: string | null = null;
    api
      .screen(run, page)
      .then((u) => {
        url = u;
        setImg(u);
      })
      .catch(() => setImg(null));
    return () => {
      if (url) URL.revokeObjectURL(url);
    };
  }, [run, page, shots]);

  const route = routes.find((r) => r.page === page)?.route ?? "/";
  const address = base ? base.replace(/\/+$/, "") + route : "";

  return (
    <div className="browser-tab">
      <div className="toolbar">
        <select value={page} onChange={(e) => setPage(e.target.value)} aria-label="Route">
          {routes.map((r) => (
            <option key={r.page} value={r.page}>
              {r.route} · {r.page}
              {shots.includes(r.page) ? "" : " (no picture)"}
            </option>
          ))}
        </select>
        <input
          className="grow"
          placeholder="Preview address, e.g. http://127.0.0.1:3000"
          value={base}
          onChange={(e) => {
            setBase(e.target.value);
            localStorage.setItem(key, e.target.value);
          }}
        />
      </div>
      <div className="muted small pad-x">
        {checks.length
          ? `The daemon's browser check opened ${checks[checks.length - 1]!.routes?.length ?? 0} route(s); its screenshots are below.`
          : "The daemon has not run a browser check for this run yet."}{" "}
        This is the product's preview, not your desktop browser.
      </div>
      <div className="route-links small">
        {routes.map((r) => {
          const href = base ? base.replace(/\/+$/, "") + r.route : "";
          return (
            <a
              key={r.page}
              className={href ? "link-word" : "muted"}
              href={href || undefined}
              onClick={(e) => {
                e.preventDefault();
                setPage(r.page);
                if (href) setPane(href);
              }}
              onContextMenu={(e) => {
                if (!href) return;
                e.preventDefault();
                openMenu(e.clientX, e.clientY, [
                  { label: "Open in the pane", run: () => (setPage(r.page), setPane(href)) },
                  { label: "Open in the system browser", run: () => window.open(href, "_blank", "noopener") },
                ]);
              }}
            >
              {r.route}
            </a>
          );
        })}
      </div>
      {pane && address && (
        <div className="preview-frame">
          <div className="row between small pad-x">
            <ExternalLink href={pane} />
            <button className="ghost small" onClick={() => setPane(null)}>
              close preview
            </button>
          </div>
          <iframe title="Preview" src={pane} sandbox="allow-scripts allow-same-origin allow-forms" />
        </div>
      )}
      <div className="shot-wrap">
        {img ? (
          <img className="shot-full" src={img} alt={`Screenshot of ${route}`} />
        ) : (
          <div className="frame typed" data-testid="no-picture">
            <div className="typed-line">{route}</div>
            <div className="typed-phase">No picture of this route</div>
          </div>
        )}
      </div>
    </div>
  );
}

// Web search.

interface Result {
  title: string;
  url: string;
  snippet: string;
  engine: string;
}

export function SearchTab() {
  const { searchEndpoint } = usePrefs();
  const [q, setQ] = useState("");
  const [results, setResults] = useState<Result[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [via, setVia] = useState("local SearXNG (127.0.0.1:8888)");
  const search = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!q.trim()) return;
    setBusy(true);
    setError(null);
    try {
      const found = await searchWeb(q.trim(), searchEndpoint);
      setVia(found.endpoint);
      setResults(found.results.map((r) => ({ title: r.title, url: r.url, snippet: r.content, engine: r.engine })));
    } catch (err) {
      setError(`${err instanceof Error ? err.message : String(err)}`);
      setResults(null);
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="search-tab">
      <form className="toolbar" onSubmit={search}>
        <input className="grow" placeholder="Search the web" value={q} onChange={(e) => setQ(e.target.value)} />
        <button className="primary small" disabled={busy}>
          {busy ? <span className="spin" aria-hidden /> : null}
          Search
        </button>
      </form>
      <div className="muted small pad-x">via {via}</div>
      {error && <div className="pad error-line">{error}</div>}
      {results && results.length === 0 && <div className="tab-empty">No results. Try fewer or different words.</div>}
      {!results && !error && <div className="tab-empty">Search the web from inside the run. Attach a result and it rides along with your next note.</div>}
      <ol className="results">
        {results?.map((r) => (
          <li key={r.url} className="result">
            <ExternalLink href={r.url}>{r.title}</ExternalLink>
            <div className="muted small break result-url">
              {r.url}
              {r.engine && <span className="muted small"> · {r.engine}</span>}
            </div>
            {r.snippet && <p className="small">{r.snippet}</p>}
            <button
              className="ghost small"
              onClick={() => {
                emit("chip", { kind: "search", label: r.title.slice(0, 40), value: r.url, url: r.url });
                toast("Attached to the next note", r.url, 2500);
              }}
            >
              Attach to next note
            </button>
          </li>
        ))}
      </ol>
    </div>
  );
}

// Terminal.

export function TerminalTab({ run, entries, project, onRan }: { run: string; entries: AgentEntry[]; project: string; onRan: () => void }) {
  const lines = entries.filter((e) => (e.agent.id === "runtime" || e.agent.id === "person") && e.kind !== "preview");
  const [command, setCommand] = useState("");
  const [busy, setBusy] = useState(false);
  const end = useRef<HTMLDivElement>(null);
  useEffect(() => {
    // Chrome returns a promise from scrollIntoView; an effect may only return a cleanup.
    void end.current?.scrollIntoView({ block: "end" });
  }, [lines.length]);
  const runIt = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!command.trim()) return;
    setBusy(true);
    try {
      await api.terminal(run, command);
      setCommand("");
      onRan();
    } catch (err) {
      toast(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="terminal-tab">
      <div className="term" aria-label="Commands the daemon ran">
        {lines.length === 0 && <div className="tab-empty">The daemon has not run a command for this run yet.</div>}
        {lines.map((l, i) => (
          <Fragment key={i}>
            {l.kind === "command" ? (
              <div className={`term-cmd ${l.typed ? "typed" : ""}`}>
                <span className="muted">{l.typed ? "you $" : "daemon $"}</span> <Rich text={l.command ?? ""} />
              </div>
            ) : (
              <pre className={`term-out ${l.ok === false ? "bad" : ""}`}>
                <Rich text={l.text ?? ""} />
                {l.typed && l.code !== undefined && <span className="muted">{`\n[exit ${l.code ?? "?"}]`}</span>}
              </pre>
            )}
          </Fragment>
        ))}
        <div ref={end} />
      </div>
      <form className="term-input" onSubmit={runIt}>
        <div className="term-label small">
          <span className="strong">Your commands</span> run on the project machine as you, in {project || "the project"}. Page workers still cannot run shell.
        </div>
        <div className="row gap">
          <span className="muted">$</span>
          <input
            className="grow mono"
            value={command}
            onChange={(e) => setCommand(e.target.value)}
            placeholder="git status"
            aria-label="Command to run on the project machine"
            spellCheck={false}
          />
          <button className="small" disabled={busy}>
            {busy ? "running…" : "Run"}
          </button>
        </div>
      </form>
    </div>
  );
}

// Preview.

const KEYWORDS = new Set(
  (
    "as async await break case catch class const continue crate default def do dyn else enum export extends false fn for from func " +
    "function if impl import in interface let loop match mod module mut new nil None null package pub return self Self static " +
    "struct super switch this throw trait true try type typeof use var void where while with yield"
  ).split(" "),
);

function tokenize(line: string): { t: string; k: "kw" | "str" | "com" | "num" | "plain" }[] {
  const out: { t: string; k: "kw" | "str" | "com" | "num" | "plain" }[] = [];
  const re = /(\/\/.*$|#(?![[!]).*$|--.*$)|("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`)|(\b\d[\d_.]*\b)|([A-Za-z_][\w]*)|(\s+|.)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(line))) {
    if (m[1]) out.push({ t: m[1], k: "com" });
    else if (m[2]) out.push({ t: m[2], k: "str" });
    else if (m[3]) out.push({ t: m[3], k: "num" });
    else if (m[4]) out.push({ t: m[4], k: KEYWORDS.has(m[4]) ? "kw" : "plain" });
    else out.push({ t: m[0], k: "plain" });
  }
  return out;
}

export function PreviewTab({ path, line }: { path: string; line?: number }) {
  const [file, setFile] = useState<{ content: string; full: string; truncated: boolean } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const target = useRef<HTMLTableRowElement>(null);
  useEffect(() => {
    setFile(null);
    setError(null);
    api
      .file(path)
      .then(setFile)
      .catch((err) => setError(err instanceof Error ? err.message : String(err)));
  }, [path]);
  useEffect(() => {
    target.current?.scrollIntoView({ block: "center" });
  }, [file, line]);
  if (error) return <div className="pad error-line">{error}</div>;
  if (!file) return <div className="pad muted">Loading {path}…</div>;
  const lines = file.content.split("\n");
  return (
    <div className="preview-tab">
      <div className="muted small pad-x break" title={file.full}>
        {file.full} · read-only{file.truncated ? " · first 512 KiB" : ""}
      </div>
      <table className="code-view" data-testid="preview">
        <tbody>
          {lines.map((l, i) => (
            <tr key={i} ref={i + 1 === line ? target : undefined} className={i + 1 === line ? "hl" : ""}>
              <td className="gutter">{i + 1}</td>
              <td className="code-cell">
                {tokenize(l).map((t, j) =>
                  t.k === "plain" ? (
                    <Fragment key={j}>{t.t}</Fragment>
                  ) : (
                    <span key={j} className={`tok-${t.k}`}>
                      {t.t}
                    </span>
                  ),
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function previewTitle(path: string): string {
  return label(path);
}
