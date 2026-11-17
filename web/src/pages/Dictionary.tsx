import { useEffect, useState } from "react";
import { BookOpen, Search, SearchX } from "lucide-react";
import { api, type DictionaryEntry } from "../api";
import { Badge, Code, Empty, Loading, Problem, useLoad } from "../ui";

const KIND: Record<DictionaryEntry["kind"], string> = {
  schema: "schema",
  routes: "routes",
  component_api: "component",
  pattern: "pattern",
  hint: "hint",
};

export function Dictionary() {
  const [query, setQuery] = useState(() => new URLSearchParams(window.location.search).get("q") ?? "");
  const [search, setSearch] = useState(query);
  useEffect(() => {
    const timer = window.setTimeout(() => setSearch(query.trim()), 250);
    return () => window.clearTimeout(timer);
  }, [query]);
  const found = useLoad(() => api.dictionary(search), [search]);

  return (
    <div className="page">
      <header className="page-head">
        <div className="grow">
          <h1>Dictionary</h1>
          <p className="page-lede">
            What pages share: schemas, routes, component APIs and patterns. Workers cite an entry instead of guessing. The Commons
            tier is shared between projects and earns its pass rate in use.
          </p>
        </div>
        {found.data && found.data.length > 0 && (
          <span className="head-count">
            <strong>{found.data.length}</strong> entr{found.data.length === 1 ? "y" : "ies"}
          </span>
        )}
      </header>
      <div className="search-field">
        <Search size={15} aria-hidden />
        <input
          className="search"
          type="search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search keys, summaries and content"
          aria-label="Search the Dictionary"
          autoFocus
        />
      </div>
      <Problem error={found.error} onRetry={found.reload} />
      {found.loading && !found.data && <Loading rows={3} tall label="Loading the Dictionary" />}
      {found.data && found.data.length === 0 && (
        <Empty title={search ? "Nothing matches" : "The Dictionary is empty"} icon={search ? <SearchX size={18} strokeWidth={1.5} /> : <BookOpen size={18} strokeWidth={1.5} />}>
          {search ? undefined : "A run's plan publishes the first entries."}
        </Empty>
      )}
      <div className="list">
        {(found.data ?? []).map((entry) => (
          <details key={`${entry.tier}-${entry.key}`} className="item" open={found.data?.length === 1}>
            <summary>
              <div className="row between gap">
                <strong className="mono">{entry.key}</strong>
                <div className="row gap small keep">
                  <Badge tone="quiet">{KIND[entry.kind] ?? entry.kind}</Badge>
                  <Badge tone={entry.tier === "commons" ? "good" : "busy"}>{entry.tier}</Badge>
                </div>
              </div>
              <div className="muted small item-sub">{entry.summary}</div>
            </summary>
            <div className="stack small">
              <div className="row wrap gap muted">
                {entry.stack && <span>{entry.stack}</span>}
                <span className="mono">{entry.hash.slice(0, 12)}</span>
                {entry.stats && (
                  <span>
                    {entry.stats.reuses
                      ? `used ${entry.stats.reuses}×, ${Math.round(entry.stats.pass_rate * 100)}% passed`
                      : "not used here yet"}
                  </span>
                )}
              </div>
              <Code>{entry.content}</Code>
            </div>
          </details>
        ))}
      </div>
    </div>
  );
}
