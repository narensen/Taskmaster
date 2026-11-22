// Per-run Web sources card (Phase 1, Item 1.7): displays each agent's web searches,
// fetched source URLs, local docs lookups, and planner research decisions.

import { useEffect, useMemo, useState } from "react";
import { AlertTriangle, BookOpen, Globe, Search, ShieldAlert } from "lucide-react";
import { api, type AgentEntry, type ResearchNote, type WebActivityItem } from "../api";
import { ExternalLink } from "../paths";

function agentLabel(item: WebActivityItem): string {
  const a = item.agent;
  if (!a) return "Agent";
  if (a.role === "master" || a.id === "master") return "Planner";
  if (a.page) return `Worker · ${a.page}`;
  if (a.role) return a.role[0]!.toUpperCase() + a.role.slice(1);
  return a.id || "Agent";
}

function safeHttps(url: string | undefined): string | null {
  if (!url) return null;
  const trimmed = url.trim();
  if (trimmed.startsWith("https://")) return trimmed;
  return null;
}

function itemKey(item: WebActivityItem): string {
  return `${item.at}|${item.agent?.id ?? ""}|${item.kind}|${item.query ?? ""}|${item.url ?? ""}|${item.package ?? ""}|${item.ecosystem ?? ""}`;
}

export function WebSources({
  runId,
  entries,
  research,
}: {
  runId: string;
  entries: AgentEntry[];
  research?: ResearchNote[];
}) {
  const [fetched, setFetched] = useState<WebActivityItem[]>([]);

  useEffect(() => {
    let alive = true;
    void api
      .runWebActivity(runId)
      .then((items) => {
        if (alive) setFetched(items);
      })
      .catch(() => {
        if (alive) setFetched([]);
      });
    return () => {
      alive = false;
    };
  }, [runId]);

  const items = useMemo<WebActivityItem[]>(() => {
    const seen = new Set<string>();
    const out: WebActivityItem[] = [];
    const push = (it: WebActivityItem) => {
      const k = itemKey(it);
      if (seen.has(k)) return;
      seen.add(k);
      out.push(it);
    };
    for (const it of fetched) push(it);
    for (const e of entries) {
      if (e.kind === "web_search" || e.kind === "web_fetch" || e.kind === "docs" || e.kind === "advisories") {
        push(e as WebActivityItem);
      }
    }
    return out;
  }, [fetched, entries]);

  const notes = research ?? [];
  if (items.length === 0 && notes.length === 0) return null;

  const searches = items.filter((i) => i.kind === "web_search").length;
  const fetches = items.filter((i) => i.kind === "web_fetch").length;
  const localDocs = items.filter((i) => i.kind === "docs").length;
  const advisoryChecks = items.filter((i) => i.kind === "advisories").length;
  const flagged = items.filter((i) => i.injection_flagged).length;

  return (
    <section className="web-sources" data-testid="web-sources" aria-label="Web sources">
      <header className="web-sources-head">
        <div className="row gap">
          <Globe size={14} aria-hidden />
          <h2>Web sources</h2>
        </div>
        <div className="row gap wrap web-sources-counts small muted">
          {searches > 0 && <span>{searches} search{searches === 1 ? "" : "es"}</span>}
          {fetches > 0 && <span>{fetches} fetch{fetches === 1 ? "" : "es"}</span>}
          {localDocs > 0 && <span>{localDocs} local docs</span>}
          {advisoryChecks > 0 && <span>{advisoryChecks} advisory check{advisoryChecks === 1 ? "" : "s"}</span>}
          {flagged > 0 && (
            <span className="web-sources-flag" data-testid="web-sources-injection-flag">
              <AlertTriangle size={12} aria-hidden />
              {flagged} injection warning{flagged === 1 ? "" : "s"}
            </span>
          )}
        </div>
      </header>

      {notes.length > 0 && (
        <div className="web-sources-research" data-testid="web-sources-research">
          <div className="small strong">Planner research decisions</div>
          <ul className="web-sources-list">
            {notes.map((n, idx) => {
              const href = safeHttps(n.source);
              return (
                <li key={idx} className="web-sources-row">
                  <span className="web-sources-body">{n.decision}</span>
                  <span className="row gap wrap small muted">
                    {n.version && <code className="mono">{n.version}</code>}
                    {href ? <ExternalLink href={href}>{href}</ExternalLink> : n.source && <span className="mono">{n.source}</span>}
                  </span>
                </li>
              );
            })}
          </ul>
        </div>
      )}

      {items.length > 0 && (
        <ul className="web-sources-list">
          {items.map((item, idx) => {
            const href = safeHttps(item.final_url || item.url);
            return (
              <li key={`${itemKey(item)}-${idx}`} className="web-sources-row" data-testid={`web-source-${item.kind}`}>
                <div className="row between gap wrap">
                  <span className="row gap small">
                    {item.kind === "web_search" ? (
                      <Search size={12} aria-hidden />
                    ) : item.kind === "docs" ? (
                      <BookOpen size={12} aria-hidden />
                    ) : item.kind === "advisories" ? (
                      <ShieldAlert size={12} aria-hidden />
                    ) : (
                      <Globe size={12} aria-hidden />
                    )}
                    <span className="strong">{agentLabel(item)}</span>
                    <span className="ops-chip">
                      {item.kind === "web_search"
                        ? "search"
                        : item.kind === "docs"
                          ? "local docs"
                          : item.kind === "advisories"
                            ? "advisories"
                            : "fetch"}
                    </span>
                    {item.cached && <span className="ops-chip">cached</span>}
                    {item.injection_flagged && (
                      <span className="web-sources-flag">
                        <AlertTriangle size={11} aria-hidden />
                        untrusted prompt pattern flagged
                      </span>
                    )}
                  </span>
                  {item.provider && <span className="muted small mono">{item.provider}</span>}
                </div>

                {item.kind === "web_search" && (
                  <div className="web-sources-detail">
                    <div className="web-sources-query">
                      Query: <span className="mono">{item.query}</span>
                      {typeof item.hits === "number" && (
                        <span className="muted small"> ({item.hits} hit{item.hits === 1 ? "" : "s"})</span>
                      )}
                    </div>
                    {item.sources && item.sources.length > 0 && (
                      <ul className="web-sources-links">
                        {item.sources.slice(0, 6).map((src, i) => {
                          const sHref = safeHttps(src.url);
                          return (
                            <li key={i} className="small">
                              {sHref ? (
                                <ExternalLink href={sHref}>{src.title || sHref}</ExternalLink>
                              ) : (
                                <span>{src.title || src.url}</span>
                              )}
                            </li>
                          );
                        })}
                      </ul>
                    )}
                  </div>
                )}

                {item.kind === "web_fetch" && (
                  <div className="web-sources-detail small">
                    {href ? <ExternalLink href={href}>{href}</ExternalLink> : <span className="mono">{item.url}</span>}
                    {typeof item.bytes === "number" && (
                      <span className="muted"> · {(item.bytes / 1024).toFixed(1)} KB</span>
                    )}
                  </div>
                )}

                {item.kind === "docs" && (
                  <div className="web-sources-detail small">
                    <span className="mono">
                      {item.package}
                      {item.version ? `@${item.version}` : ""}
                      {item.symbol ? ` · ${item.symbol}` : ""}
                    </span>
                    {item.path && <span className="muted mono"> ({item.path})</span>}
                  </div>
                )}

                {item.kind === "advisories" && (
                  <div className="web-sources-detail small">
                    <span className="mono">
                      {item.ecosystem ? `${item.ecosystem}: ` : ""}
                      {item.package}
                      {item.version ? `@${item.version}` : ""}
                    </span>
                    {typeof item.count === "number" && (
                      <span className="muted">
                        {" "}
                        · {item.count === 0 ? "no known advisories" : `${item.count} advisor${item.count === 1 ? "y" : "ies"}`}
                      </span>
                    )}
                    {item.advisories && item.advisories.length > 0 && (
                      <ul className="web-sources-links">
                        {item.advisories.slice(0, 5).map((adv, i) => (
                          <li key={i} className="small">
                            <span className="mono">{adv.id}</span>
                            {adv.summary ? ` — ${adv.summary}` : ""}
                          </li>
                        ))}
                      </ul>
                    )}
                  </div>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
