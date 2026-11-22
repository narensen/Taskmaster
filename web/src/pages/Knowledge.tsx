// Unified Knowledge Hub (`/knowledge`):
// Brings together the Living Codebase Atlas, Policy-as-Code visual editor,
// Dictionary, Errorography, and Skills into one calm, richly visual workspace.

import { useEffect, useState } from "react";
import {
  BookOpen,
  Bug,
  Flame,
  FolderKanban,
  GitBranch,
  Map as MapIcon,
  Network,
  Puzzle,
  RefreshCw,
  Save,
  Shield,
  Sparkles,
} from "lucide-react";
import { api, type Atlas, type Policy } from "../api";
import { toast } from "../overlay";
import { Dictionary } from "./Dictionary";
import { Errorography } from "./Errorography";
import { Skills } from "./Skills";

type Tab = "atlas" | "policy" | "dictionary" | "errorography" | "skills";

const LANG_COLORS: Record<string, string> = {
  Rust: "#f97316",
  TypeScript: "#3b82f6",
  JavaScript: "#eab308",
  Python: "#22c55e",
  CSS: "#a855f7",
  HTML: "#ec4899",
  Go: "#06b6d4",
  JSON: "#64748b",
  TOML: "#94a3b8",
  Markdown: "#14b8a6",
  Shell: "#10b981",
};

function langColor(lang: string, i: number): string {
  if (LANG_COLORS[lang]) return LANG_COLORS[lang]!;
  const palette = ["#6366f1", "#06b6d4", "#10b981", "#f59e0b", "#ec4899", "#8b5cf6"];
  return palette[i % palette.length]!;
}

function splitLines(raw: string): string[] {
  return raw
    .split(/[\n,]/)
    .map((s) => s.trim())
    .filter(Boolean);
}

function AtlasPanel() {
  const [atlas, setAtlas] = useState<Atlas | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);

  const load = async (refresh = false) => {
    if (refresh) setRefreshing(true);
    else setLoading(true);
    try {
      const data = await api.atlas(refresh);
      setAtlas(data);
      if (refresh) toast("Atlas rebuilt", `Indexed ${data.total_files} files`, 2500);
    } catch (err) {
      toast("Could not load atlas", err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  };

  useEffect(() => {
    void load(false);
  }, []);

  if (loading && !atlas) {
    return <div className="muted small">Indexing repository architecture…</div>;
  }
  if (!atlas) {
    return <div className="muted small">Atlas unavailable for this project.</div>;
  }

  const totalLangLines = atlas.languages.reduce((n, l) => n + l.lines, 0) || 1;
  const maxChurn = Math.max(1, ...atlas.hotspots.map((h) => h.touches));

  return (
    <div className="atlas-view" data-testid="atlas-view">
      {/* Top KPI + Rebuild Bar */}
      <div className="atlas-header">
        <div className="telemetry-kpi-grid">
          <div className="kpi-card tone-indigo">
            <div className="kpi-label"><FolderKanban size={13} aria-hidden /> Source Files</div>
            <div className="kpi-value mono">{atlas.total_files.toLocaleString()}</div>
            <div className="kpi-sub muted">{atlas.modules.length} module clusters</div>
          </div>
          <div className="kpi-card tone-cyan">
            <div className="kpi-label"><Sparkles size={13} aria-hidden /> Lines of Code</div>
            <div className="kpi-value mono">{atlas.total_lines.toLocaleString()}</div>
            <div className="kpi-sub muted">{atlas.languages.length} languages</div>
          </div>
          <div className="kpi-card tone-violet">
            <div className="kpi-label"><Network size={13} aria-hidden /> Import Hubs</div>
            <div className="kpi-value mono">{atlas.hubs.length}</div>
            <div className="kpi-sub muted">High-fan-in core files</div>
          </div>
          <div className="kpi-card tone-emerald">
            <div className="kpi-label"><GitBranch size={13} aria-hidden /> Indexed Commit</div>
            <div className="kpi-value mono">{atlas.head ?? "uncommitted"}</div>
            <div className="kpi-sub muted">Cached at .taskmaster/atlas.json</div>
          </div>
        </div>

        <div className="row between wrap gap">
          <div className="row gap wrap">
            {atlas.conventions.map((c) => (
              <span key={c} className="studio-pill tone-cyan">{c}</span>
            ))}
          </div>
          <button
            type="button"
            className="small"
            disabled={refreshing}
            onClick={() => void load(true)}
            data-testid="atlas-refresh-btn"
          >
            <RefreshCw size={13} className={refreshing ? "spin-ico" : ""} aria-hidden />{" "}
            {refreshing ? "Re-indexing…" : "Rebuild Atlas"}
          </button>
        </div>
      </div>

      {/* Language Composition Bar */}
      {atlas.languages.length > 0 && (
        <section className="studio-card">
          <h3>Language Composition</h3>
          <div className="stacked-bar">
            {atlas.languages.map((l, i) => (
              <div
                key={l.language}
                className="seg"
                style={{
                  width: `${Math.max(1.5, (l.lines / totalLangLines) * 100)}%`,
                  backgroundColor: langColor(l.language, i),
                }}
                title={`${l.language}: ${l.files} files, ${l.lines.toLocaleString()} lines`}
              />
            ))}
          </div>
          <div className="stacked-legend mono small">
            {atlas.languages.slice(0, 8).map((l, i) => (
              <span key={l.language}>
                <i className="dot" style={{ backgroundColor: langColor(l.language, i) }} />
                {l.language} {((l.lines / totalLangLines) * 100).toFixed(1)}% ({l.files})
              </span>
            ))}
          </div>
        </section>
      )}

      {/* Module Clusters Grid */}
      <section className="studio-card">
        <h3>Module Clusters</h3>
        <div className="atlas-modules-grid">
          {atlas.modules.slice(0, 12).map((m) => (
            <div key={m.name} className="atlas-module-card">
              <div className="row between">
                <strong className="mono clip">{m.name}</strong>
                <span className="studio-pill tone-indigo mono">{m.files} files</span>
              </div>
              <div className="atlas-module-stats mono small muted">
                <span>{m.lines.toLocaleString()} loc</span>
                <span>{m.exports} exports</span>
                <span>{m.tests} tests</span>
              </div>
            </div>
          ))}
        </div>
      </section>

      {/* Hubs & Hotspots Split */}
      <div className="atlas-split">
        <section className="studio-card">
          <h3><Network size={14} aria-hidden /> Architectural Import Hubs</h3>
          {atlas.hubs.length === 0 ? (
            <p className="muted small">No internal relative import hubs detected.</p>
          ) : (
            <div className="atlas-table">
              {atlas.hubs.slice(0, 10).map((h) => (
                <div key={h.path} className="atlas-table-row">
                  <code className="mono clip" title={h.path}>{h.path}</code>
                  <span className="studio-pill tone-violet mono">{h.imports_in} refs</span>
                  <span className="muted small mono">{h.exports} exp</span>
                </div>
              ))}
            </div>
          )}
        </section>

        <section className="studio-card">
          <h3><Flame size={14} aria-hidden /> Git Churn Hotspots</h3>
          {atlas.hotspots.length === 0 ? (
            <p className="muted small">No recent git history hotspots yet.</p>
          ) : (
            <div className="atlas-table">
              {atlas.hotspots.slice(0, 10).map((h) => (
                <div key={h.path} className="atlas-hotspot-row">
                  <div className="row between gap">
                    <code className="mono clip" title={h.path}>{h.path}</code>
                    <span className="mono small">{h.touches} touches</span>
                  </div>
                  <div className="heat-track">
                    <div className="heat-fill" style={{ width: `${(h.touches / maxChurn) * 100}%` }} />
                  </div>
                </div>
              ))}
            </div>
          )}
        </section>
      </div>
    </div>
  );
}

function PolicyPanel() {
  const [policy, setPolicy] = useState<Policy | null>(null);
  const [source, setSource] = useState<string | null>(null);
  const [toml, setToml] = useState<string>("");
  const [repoFile, setRepoFile] = useState(true);
  const [saving, setSaving] = useState(false);

  const [deniedText, setDeniedText] = useState("");
  const [protectedText, setProtectedText] = useState("");
  const [allowedHosts, setAllowedHosts] = useState("");
  const [deniedHosts, setDeniedHosts] = useState("");
  const [redactText, setRedactText] = useState("");
  const [maxRunUsd, setMaxRunUsd] = useState("");
  const [dailyUsd, setDailyUsd] = useState("");
  const [crossFamily, setCrossFamily] = useState(false);

  useEffect(() => {
    api
      .policy()
      .then((res) => {
        setPolicy(res.policy);
        setSource(res.source);
        setToml(res.toml);
        setRepoFile(res.source !== ".taskmaster/policy.json");
        setDeniedText(res.policy.denied_globs.join("\n"));
        setProtectedText(res.policy.protected_globs.join("\n"));
        setAllowedHosts(res.policy.allowed_fetch_hosts.join("\n"));
        setDeniedHosts(res.policy.denied_fetch_hosts.join("\n"));
        setRedactText(res.policy.redact_patterns.join("\n"));
        setMaxRunUsd(res.policy.max_run_cost_usd != null ? String(res.policy.max_run_cost_usd) : "");
        setDailyUsd(res.policy.daily_budget_usd != null ? String(res.policy.daily_budget_usd) : "");
        setCrossFamily(res.policy.require_cross_family_review);
      })
      .catch((err) => toast("Could not load policy", err instanceof Error ? err.message : String(err)));
  }, []);

  const save = async () => {
    setSaving(true);
    try {
      const next: Policy = {
        denied_globs: splitLines(deniedText),
        protected_globs: splitLines(protectedText),
        allowed_fetch_hosts: splitLines(allowedHosts),
        denied_fetch_hosts: splitLines(deniedHosts),
        redact_patterns: splitLines(redactText),
        max_run_cost_usd: maxRunUsd.trim() ? Number(maxRunUsd.trim()) : null,
        daily_budget_usd: dailyUsd.trim() ? Number(dailyUsd.trim()) : null,
        require_cross_family_review: crossFamily,
      };
      const res = await api.savePolicy(next, repoFile);
      setPolicy(res.policy);
      setSource(res.source);
      setToml(res.toml);
      toast("Policy saved", `Written to ${res.source}`, 2500);
    } catch (err) {
      toast("Could not save policy", err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  if (!policy) return <div className="muted small">Loading Policy-as-Code…</div>;

  return (
    <div className="policy-view" data-testid="policy-view">
      <div className="studio-card">
        <div className="row between wrap gap">
          <div>
            <h3><Shield size={14} aria-hidden /> Policy-as-Code Guardrails</h3>
            <p className="muted small">
              Source: <code className="mono">{source ?? "built-in defaults"}</code>. Enforced across Code Mode, run spend caps, and Flight Recorder exports.
            </p>
          </div>
          <div className="row gap">
            <label className={`toggle-chip small ${repoFile ? "on" : ""}`}>
              <input type="checkbox" checked={repoFile} onChange={(e) => setRepoFile(e.target.checked)} />
              Commit to taskmaster.policy.toml
            </label>
            <button type="button" className="primary small" disabled={saving} onClick={() => void save()} data-testid="policy-save-btn">
              <Save size={13} aria-hidden /> {saving ? "Saving…" : "Save policy"}
            </button>
          </div>
        </div>

        <div className="policy-grid">
          <label className="workbench-field">
            <span>Denied File Globs (blocked unconditionally)</span>
            <textarea className="mono" rows={4} value={deniedText} onChange={(e) => setDeniedText(e.target.value)} />
          </label>
          <label className="workbench-field">
            <span>Protected File Globs (require human review)</span>
            <textarea className="mono" rows={4} value={protectedText} onChange={(e) => setProtectedText(e.target.value)} placeholder="src/auth/**&#10;migrations/**" />
          </label>
          <label className="workbench-field">
            <span>Allowed Fetch Hosts (optional allowlist)</span>
            <textarea className="mono" rows={3} value={allowedHosts} onChange={(e) => setAllowedHosts(e.target.value)} placeholder="docs.rs&#10;npmjs.org" />
          </label>
          <label className="workbench-field">
            <span>Denied Fetch Hosts</span>
            <textarea className="mono" rows={3} value={deniedHosts} onChange={(e) => setDeniedHosts(e.target.value)} />
          </label>
          <label className="workbench-field">
            <span>Max Run Cost Cap (USD)</span>
            <input className="mono" type="number" min="0.1" step="0.5" value={maxRunUsd} onChange={(e) => setMaxRunUsd(e.target.value)} placeholder="unlimited" />
          </label>
          <label className="workbench-field">
            <span>Daily Budget Cap (USD)</span>
            <input className="mono" type="number" min="0.1" step="1" value={dailyUsd} onChange={(e) => setDailyUsd(e.target.value)} placeholder="unlimited" />
          </label>
          <label className="workbench-field policy-span-2">
            <span>Custom Secret Redaction Literals (one per line, scrubbed from .tmrun.json exports)</span>
            <textarea className="mono" rows={2} value={redactText} onChange={(e) => setRedactText(e.target.value)} placeholder="INTERNAL_API_TOKEN_PREFIX" />
          </label>
        </div>

        <label className={`toggle-chip ${crossFamily ? "on" : ""}`}>
          <input type="checkbox" checked={crossFamily} onChange={(e) => setCrossFamily(e.target.checked)} />
          Require cross-family model provider for code review
        </label>
      </div>

      <section className="studio-card">
        <h3>Rendered TOML Preview</h3>
        <pre className="policy-toml mono">{toml}</pre>
      </section>
    </div>
  );
}

export function Knowledge() {
  const [tab, setTab] = useState<Tab>("atlas");

  const tabs: { id: Tab; label: string; icon: typeof MapIcon }[] = [
    { id: "atlas", label: "Codebase Atlas", icon: MapIcon },
    { id: "policy", label: "Policy-as-Code", icon: Shield },
    { id: "dictionary", label: "Dictionary", icon: BookOpen },
    { id: "errorography", label: "Errorography", icon: Bug },
    { id: "skills", label: "Skills", icon: Puzzle },
  ];

  return (
    <div className="page knowledge-page" data-testid="knowledge-page">
      <header className="knowledge-head">
        <div>
          <h1>Knowledge Hub</h1>
          <p className="muted">Living architectural map, repository guardrails, shared specs, learned mistakes, and agent skills.</p>
        </div>
        <nav className="studio-tabs" role="tablist" aria-label="Knowledge sections">
          {tabs.map((t) => {
            const Icon = t.icon;
            return (
              <button
                key={t.id}
                type="button"
                role="tab"
                aria-selected={tab === t.id}
                className={`studio-tab ${tab === t.id ? "on" : ""}`}
                onClick={() => setTab(t.id)}
              >
                <Icon size={14} aria-hidden /> {t.label}
              </button>
            );
          })}
        </nav>
      </header>

      <div className="knowledge-body">
        {tab === "atlas" && <AtlasPanel />}
        {tab === "policy" && <PolicyPanel />}
        {tab === "dictionary" && <Dictionary />}
        {tab === "errorography" && <Errorography />}
        {tab === "skills" && <Skills />}
      </div>
    </div>
  );
}
