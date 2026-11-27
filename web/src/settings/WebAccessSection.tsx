// Settings -> Web access (Phase 1, Item 1.7): provider, vaulted API key,
// per-role search/fetch matrix, domain allowlist, planner budgets, and fetch cache.

import { useState } from "react";
import { api, type WebAccessPayload, type WebAccessSettings, type WebRoleMatrix } from "../api";
import { toast } from "../overlay";
import { Problem, Switch, useLoad } from "../ui";
import { SelectMenu } from "../components/SelectMenu";

const PROVIDER_LABELS: Record<string, string> = {
  searxng: "SearXNG (local / self-hosted)",
  brave: "Brave Search",
  tavily: "Tavily",
  exa: "Exa",
  native: "Provider native web search",
};

const PROVIDER_PLACEHOLDERS: Record<string, string> = {
  searxng: "http://127.0.0.1:8888/search",
  brave: "https://api.search.brave.com/res/v1/web/search",
  tavily: "https://api.tavily.com/search",
  exa: "https://api.exa.ai/search",
  native: "Uses the model provider's built-in search",
};

const NEEDS_KEY = new Set(["brave", "tavily", "exa"]);

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
}

function parseDomains(raw: string): string[] {
  return raw
    .split(/[\n,]+/)
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);
}

const DEFAULT_WEB: WebAccessSettings = {
  enabled: false,
  provider: "searxng",
  endpoint: "",
  roles: {
    planner: { search: true, fetch: true },
    worker: { search: false, fetch: true },
    reviewer: { search: false, fetch: true },
    verifier: { search: false, fetch: false },
  },
  allowlist: [],
  planner_max_searches: 5,
  planner_max_fetches: 8,
  max_fetch_bytes: 2 * 1024 * 1024,
  cache_ttl_secs: 86400,
  cache_max_mb: 128,
};

export function WebAccessSection() {
  const loaded = useLoad(api.webAccess, []);
  const [draft, setDraft] = useState<WebAccessSettings | null>(null);
  const [allowText, setAllowText] = useState<string | null>(null);
  const [apiKey, setApiKey] = useState("");
  const [saving, setSaving] = useState(false);
  const [clearingCache, setClearingCache] = useState(false);

  const data: WebAccessPayload | null = loaded.data ?? null;
  const cfg: WebAccessSettings = draft ?? data?.web_access ?? DEFAULT_WEB;
  const hasKey = data?.providers_with_keys
    ? data.providers_with_keys.includes(cfg.provider)
    : Boolean(data?.has_api_key && cfg.provider === data?.web_access.provider);
  const domainsValue = allowText ?? cfg.allowlist.join(", ");
  const providers = data?.providers?.length ? data.providers : ["searxng", "brave", "tavily", "exa", "native"];

  const patch = (next: Partial<WebAccessSettings>) => setDraft({ ...cfg, ...next });

  const patchRole = (role: keyof WebRoleMatrix, field: "search" | "fetch", value: boolean) => {
    setDraft({
      ...cfg,
      roles: {
        ...cfg.roles,
        [role]: { ...cfg.roles[role], [field]: value },
      },
    });
  };

  const save = async () => {
    setSaving(true);
    try {
      const next = await api.updateWebAccess({
        enabled: cfg.enabled,
        provider: cfg.provider,
        endpoint: cfg.endpoint.trim(),
        api_key: apiKey.trim() || undefined,
        roles: {
          ...cfg.roles,
          verifier: { search: false, fetch: false },
        },
        allowlist: parseDomains(domainsValue),
        planner_max_searches: cfg.planner_max_searches,
        planner_max_fetches: cfg.planner_max_fetches,
        max_fetch_bytes: cfg.max_fetch_bytes,
        cache_ttl_secs: cfg.cache_ttl_secs,
        cache_max_mb: cfg.cache_max_mb,
      });
      loaded.setData(next);
      setDraft(null);
      setAllowText(null);
      setApiKey("");
      toast("Web access saved", next.web_access.enabled ? `Provider: ${next.web_access.provider}` : "Web access is off");
    } catch (err) {
      toast("Web access was not saved", err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  const clearKey = async () => {
    setSaving(true);
    try {
      const next = await api.updateWebAccess({ provider: cfg.provider, clear_api_key: true });
      loaded.setData(next);
      setApiKey("");
      toast("API key removed from vault");
    } catch (err) {
      toast("Could not clear API key", err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  const clearCache = async () => {
    setClearingCache(true);
    try {
      const res = await api.clearWebCache();
      if (data) loaded.setData({ ...data, cache: res.cache });
      toast("Web cache cleared", `${res.removed} cached page${res.removed === 1 ? "" : "s"} removed`);
    } catch (err) {
      toast("Could not clear web cache", err instanceof Error ? err.message : String(err));
    } finally {
      setClearingCache(false);
    }
  };

  return (
    <section id="web-access" className="settings-section" aria-labelledby="h-web-access">
      <div className="settings-head">
        <h2 className="section-title" id="h-web-access">
          Web access
        </h2>
        <p className="muted small">
          Daemon-mediated web search, HTTPS-only SSRF-checked fetch, and local-first package docs. Off by default; every fetched page is converted to markdown, cached offline, and framed as untrusted data. API keys stay in the OS vault and are never written to config files.
        </p>
      </div>
      <div className="settings-body">

      <div className="ops-stack web-access-panel" data-testid="web-access-section">
        <label className="row between gap" data-testid="web-access-enabled">
          <span>Enable agent web access</span>
          <Switch
            on={cfg.enabled}
            label="Enable agent web access"
            onChange={(enabled) => patch({ enabled })}
          />
        </label>

        <div className="web-access-grid">
          <div className="web-access-field">
            <span className="muted small">Search provider</span>
            <SelectMenu
              ariaLabel="Web search provider"
              testId="web-access-provider"
              value={cfg.provider}
              options={providers.map((id) => ({ value: id, label: PROVIDER_LABELS[id] ?? id }))}
              onChange={(v) => patch({ provider: v })}
            />
          </div>

          <label className="web-access-field">
            <span className="muted small">Provider endpoint (optional)</span>
            <input
              className="mono"
              aria-label="Web provider endpoint"
              data-testid="web-access-endpoint"
              placeholder={PROVIDER_PLACEHOLDERS[cfg.provider] ?? "https://…"}
              value={cfg.endpoint}
              onChange={(e) => patch({ endpoint: e.target.value })}
            />
          </label>
        </div>

        <div className="web-access-key-row">
          <label className="web-access-field grow">
            <span className="muted small">
              Provider API key{" "}
              {NEEDS_KEY.has(cfg.provider) ? "(stored in OS vault)" : "(optional for this provider)"}
            </span>
            <input
              type="password"
              autoComplete="new-password"
              aria-label="Web provider API key"
              data-testid="web-access-key"
              placeholder={hasKey ? "Enter a new key to replace the vaulted one" : "Paste API key (stored in OS vault)"}
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
            />
          </label>
          <div className="row gap wrap web-access-key-meta">
            {hasKey && (
              <span className="ops-chip" data-testid="web-access-key-saved">
                Saved in vault ({data?.vault_backend})
              </span>
            )}
            {hasKey && (
              <button type="button" className="ghost small" disabled={saving} onClick={() => void clearKey()}>
                Clear key
              </button>
            )}
          </div>
        </div>

        <div className="web-access-block">
          <div className="small strong">Per-role permissions</div>
          <table className="table small web-access-roles">
            <thead>
              <tr>
                <th scope="col">Role</th>
                <th scope="col">Web search</th>
                <th scope="col">Web fetch</th>
                <th scope="col">Scope</th>
              </tr>
            </thead>
            <tbody>
              <tr>
                <td>Planner</td>
                <td>
                  <Switch
                    on={cfg.roles.planner.search}
                    label="Planner web search"
                    onChange={(v) => patchRole("planner", "search", v)}
                  />
                </td>
                <td>
                  <Switch
                    on={cfg.roles.planner.fetch}
                    label="Planner web fetch"
                    onChange={(v) => patchRole("planner", "fetch", v)}
                  />
                </td>
                <td className="muted">Budgeted research phase; writes compact decisions to PROJECT.md</td>
              </tr>
              <tr>
                <td>Worker</td>
                <td>
                  <Switch
                    on={cfg.roles.worker.search}
                    label="Worker web search"
                    onChange={(v) => patchRole("worker", "search", v)}
                  />
                </td>
                <td>
                  <Switch
                    on={cfg.roles.worker.fetch}
                    label="Worker web fetch"
                    onChange={(v) => patchRole("worker", "fetch", v)}
                  />
                </td>
                <td className="muted">Project-derived + extra allowlist; off-list domains go through Tool approvals</td>
              </tr>
              <tr>
                <td>Reviewer</td>
                <td>
                  <Switch
                    on={cfg.roles.reviewer.search}
                    label="Reviewer web search"
                    onChange={(v) => patchRole("reviewer", "search", v)}
                  />
                </td>
                <td>
                  <Switch
                    on={cfg.roles.reviewer.fetch}
                    label="Reviewer web fetch"
                    onChange={(v) => patchRole("reviewer", "fetch", v)}
                  />
                </td>
                <td className="muted">Security advisories &amp; changelogs ({data?.advisory_hosts?.length ?? 0} default hosts)</td>
              </tr>
              <tr>
                <td>Verifier</td>
                <td className="muted">Off</td>
                <td className="muted">Off</td>
                <td className="muted">Verifiers never get web access</td>
              </tr>
            </tbody>
          </table>
        </div>

        <div className="web-access-block">
          <label className="web-access-field">
            <span className="small strong">Extra allowed fetch domains</span>
            <span className="muted small">Comma-separated hosts or wildcards (for example: docs.rs, *.python.org).</span>
            <input
              className="mono"
              aria-label="Extra allowed fetch domains"
              data-testid="web-access-allowlist"
              placeholder="docs.rs, developer.mozilla.org, *.tokio.rs"
              value={domainsValue}
              onChange={(e) => setAllowText(e.target.value)}
            />
          </label>
          {data && data.derived_allowlist.length > 0 && (
            <div className="web-access-derived small">
              <span className="muted">Active worker allowlist ({data.derived_allowlist.length}): </span>
              <span className="mono">{data.derived_allowlist.slice(0, 10).join(", ")}{data.derived_allowlist.length > 10 ? "…" : ""}</span>
            </div>
          )}
        </div>

        <div className="web-access-budgets">
          <label className="web-access-num">
            <span className="muted small">Planner max searches</span>
            <input
              type="number"
              min={0}
              max={100}
              aria-label="Planner max searches"
              value={cfg.planner_max_searches}
              onChange={(e) => patch({ planner_max_searches: Math.max(0, Number(e.target.value) || 0) })}
            />
          </label>
          <label className="web-access-num">
            <span className="muted small">Planner max fetches</span>
            <input
              type="number"
              min={0}
              max={100}
              aria-label="Planner max fetches"
              value={cfg.planner_max_fetches}
              onChange={(e) => patch({ planner_max_fetches: Math.max(0, Number(e.target.value) || 0) })}
            />
          </label>
          <label className="web-access-num">
            <span className="muted small">Max fetch size (KB)</span>
            <input
              type="number"
              min={16}
              max={20480}
              aria-label="Max fetch size in KB"
              value={Math.round(cfg.max_fetch_bytes / 1024)}
              onChange={(e) => patch({ max_fetch_bytes: Math.max(16, Number(e.target.value) || 16) * 1024 })}
            />
          </label>
          <label className="web-access-num">
            <span className="muted small">Cache TTL (hours)</span>
            <input
              type="number"
              min={0}
              max={720}
              aria-label="Web cache TTL in hours"
              value={Math.round(cfg.cache_ttl_secs / 3600)}
              onChange={(e) => patch({ cache_ttl_secs: Math.max(0, Number(e.target.value) || 0) * 3600 })}
            />
          </label>
          <label className="web-access-num">
            <span className="muted small">Cache cap (MB)</span>
            <input
              type="number"
              min={1}
              max={4096}
              aria-label="Web cache cap in MB"
              value={cfg.cache_max_mb}
              onChange={(e) => patch({ cache_max_mb: Math.max(1, Number(e.target.value) || 1) })}
            />
          </label>
        </div>

        <div className="row between gap wrap web-access-foot">
          <div className="muted small" data-testid="web-access-cache-stats">
            {data
              ? `Cache: ${data.cache.entries} page${data.cache.entries === 1 ? "" : "s"} (${formatBytes(data.cache.bytes)})`
              : "Loading cache stats…"}
          </div>
          <div className="row gap wrap">
            <button
              type="button"
              className="ghost small"
              data-testid="web-access-clear-cache"
              disabled={clearingCache}
              onClick={() => void clearCache()}
            >
              {clearingCache ? "Clearing…" : "Clear cache"}
            </button>
            <button
              type="button"
              className="primary small"
              data-testid="web-access-save"
              disabled={saving}
              onClick={() => void save()}
            >
              {saving ? "Saving…" : "Save web access"}
            </button>
          </div>
        </div>
      </div>

      <Problem error={loaded.error} onRetry={() => void loaded.reload()} />
      </div>
    </section>
  );
}
