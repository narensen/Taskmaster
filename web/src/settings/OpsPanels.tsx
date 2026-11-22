// Settings sections for Phase 3-5 daemon operations: notifications & webhooks,
// scheduled runs, user MCP servers, remote runners & environments, monthly spend,
// retention GC, egress audit log, stratified scorecard, and eval suite overview.

import { useState, type ReactNode } from "react";
import {
  api,
  type GcReport,
  type NodeHealth,
  type NotificationConfig,
} from "../api";
import { Problem, Switch, useLoad } from "../ui";
import { toast } from "../overlay";

function OpsSection({ id, title, note, children }: { id: string; title: string; note?: string; children: ReactNode }) {
  return (
    <section id={id} className="section">
      <h2>{title}</h2>
      {note && <p className="muted small section-note">{note}</p>}
      {children}
    </section>
  );
}

export function WebhookNotificationsSection() {
  const loaded = useLoad(api.notifications, []);
  const [draft, setDraft] = useState<NotificationConfig | null>(null);
  const [saving, setSaving] = useState(false);
  const cfg: NotificationConfig = draft ?? loaded.data ?? {
    desktop: true,
    webhook_url: "",
    on_approval: true,
    on_limit: true,
    on_finish: true,
  };

  const save = async () => {
    setSaving(true);
    try {
      const next = await api.updateNotifications({
        ...cfg,
        webhook_url: cfg.webhook_url?.trim() || null,
      });
      loaded.setData(next);
      setDraft(null);
      toast("Daemon alerts saved");
    } catch (err) {
      toast("Could not save alerts", err instanceof Error ? err.message : String(err));
    } finally {
      setSaving(false);
    }
  };

  const sendTest = async () => {
    try {
      const res = await api.testNotification();
      toast(res.sent ? "Test alert dispatched" : "Alerts are disabled");
    } catch (err) {
      toast("Test alert failed", err instanceof Error ? err.message : String(err));
    }
  };

  return (
    <OpsSection
      id="daemon-alerts"
      title="Daemon alerts & webhooks"
      note="Daemon-side desktop notifications and HTTPS webhooks (Slack, Discord, or custom). Payloads are scrubbed for secrets before sending."
    >
      <div className="ops-stack" data-testid="ops-notifications">
        <label className="row between gap">
          <span>Daemon desktop notifications</span>
          <Switch on={cfg.desktop} label="Daemon desktop notifications" onChange={(desktop) => setDraft({ ...cfg, desktop })} />
        </label>
        <div className="row gap wrap">
          <label className="check small">
            <input type="checkbox" checked={cfg.on_approval} onChange={(e) => setDraft({ ...cfg, on_approval: e.target.checked })} />
            Approval waiting
          </label>
          <label className="check small">
            <input type="checkbox" checked={cfg.on_limit} onChange={(e) => setDraft({ ...cfg, on_limit: e.target.checked })} />
            Provider limit hit
          </label>
          <label className="check small">
            <input type="checkbox" checked={cfg.on_finish} onChange={(e) => setDraft({ ...cfg, on_finish: e.target.checked })} />
            Run finished
          </label>
        </div>
        <div className="row gap wrap">
          <input
            className="grow mono"
            placeholder="https://hooks.example.com/taskmaster"
            value={cfg.webhook_url ?? ""}
            onChange={(e) => setDraft({ ...cfg, webhook_url: e.target.value })}
          />
          <button type="button" className="small" disabled={saving} onClick={() => void save()}>
            {saving ? "Saving…" : "Save"}
          </button>
          <button type="button" className="ghost small" onClick={() => void sendTest()}>
            Send test
          </button>
        </div>
      </div>
      <Problem error={loaded.error} onRetry={() => void loaded.reload()} />
    </OpsSection>
  );
}

export function SchedulesSection() {
  const loaded = useLoad(api.schedules, []);
  const [name, setName] = useState("");
  const [schedule, setSchedule] = useState("nightly");
  const [prompt, setPrompt] = useState("");
  const [cap, setCap] = useState("");
  const [busy, setBusy] = useState(false);

  const add = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!name.trim() || !prompt.trim()) return;
    setBusy(true);
    try {
      const parsedCap = cap.trim() ? Number(cap.trim()) : undefined;
      await api.upsertSchedule({
        name: name.trim(),
        schedule: schedule.trim() || "nightly",
        prompt: prompt.trim(),
        max_cost_usd: parsedCap && Number.isFinite(parsedCap) ? parsedCap : undefined,
      });
      setName("");
      setPrompt("");
      setCap("");
      await loaded.reload();
      toast("Schedule saved");
    } catch (err) {
      toast("Invalid schedule", err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <OpsSection
      id="schedules"
      title="Scheduled runs"
      note="Recurring runs on a preset (hourly, nightly, weekly) or 5-field cron expression."
    >
      <div className="ops-stack" data-testid="ops-schedules">
        {(loaded.data ?? []).length === 0 ? (
          <div className="muted small">No scheduled runs configured.</div>
        ) : (
          <ul className="plain-list">
            {(loaded.data ?? []).map((s) => (
              <li key={s.id} className="ops-row">
                <div className="grow">
                  <div className="row gap">
                    <strong>{s.name}</strong>
                    <span className="mono small muted">{s.schedule}</span>
                    {s.max_cost_usd != null && <span className="mono small muted">cap ${s.max_cost_usd}</span>}
                  </div>
                  <div className="muted small clip">{s.prompt}</div>
                </div>
                <Switch
                  on={s.enabled}
                  label={`Enable ${s.name}`}
                  onChange={(enabled) => void api.toggleSchedule(s.id, enabled).then(() => loaded.reload())}
                />
                <button type="button" className="icon" onClick={() => void api.removeSchedule(s.id).then(() => loaded.reload())}>
                  remove
                </button>
              </li>
            ))}
          </ul>
        )}
        <form className="ops-form-grid" onSubmit={(e) => void add(e)}>
          <input placeholder="Name (e.g. Nightly audit)" value={name} onChange={(e) => setName(e.target.value)} />
          <input className="mono" placeholder="nightly | hourly | 0 3 * * *" value={schedule} onChange={(e) => setSchedule(e.target.value)} />
          <input className="mono" type="number" step="0.5" min="0.1" placeholder="Cap USD" value={cap} onChange={(e) => setCap(e.target.value)} />
          <input className="ops-span-full" placeholder="Prompt for the scheduled run" value={prompt} onChange={(e) => setPrompt(e.target.value)} />
          <div>
            <button className="small" disabled={busy || !name.trim() || !prompt.trim()}>
              {busy ? "Adding…" : "Add schedule"}
            </button>
          </div>
        </form>
      </div>
      <Problem error={loaded.error} onRetry={() => void loaded.reload()} />
    </OpsSection>
  );
}

export function UserMcpSection() {
  const loaded = useLoad(api.mcpServers, []);
  const [name, setName] = useState("");
  const [command, setCommand] = useState("");
  const [argsText, setArgsText] = useState("");
  const [rolesText, setRolesText] = useState("worker");
  const [secretKey, setSecretKey] = useState("");
  const [secretVal, setSecretVal] = useState("");
  const [requireApproval, setRequireApproval] = useState(true);

  const add = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!name.trim() || !command.trim()) return;
    try {
      const secret_env: Record<string, string> = {};
      if (secretKey.trim() && secretVal) secret_env[secretKey.trim()] = secretVal;
      await api.upsertMcp({
        name: name.trim(),
        command: command.trim(),
        args: argsText.trim() ? argsText.trim().split(/\s+/) : [],
        roles: rolesText.split(",").map((r) => r.trim()).filter(Boolean),
        secret_env,
        require_approval: requireApproval,
      });
      setName("");
      setCommand("");
      setArgsText("");
      setSecretKey("");
      setSecretVal("");
      await loaded.reload();
      toast("MCP server saved");
    } catch (err) {
      toast("Could not save MCP server", err instanceof Error ? err.message : String(err));
    }
  };

  return (
    <OpsSection
      id="mcp-servers"
      title="User MCP servers"
      note="Attach custom stdio MCP servers per role. Secret environment values are stored in the OS keychain and never written to config JSON."
    >
      <div className="ops-stack" data-testid="ops-mcp">
        {(loaded.data ?? []).length === 0 ? (
          <div className="muted small">No custom MCP servers configured.</div>
        ) : (
          <ul className="plain-list">
            {(loaded.data ?? []).map((srv) => (
              <li key={srv.name} className="ops-row">
                <div className="grow">
                  <div className="row gap wrap">
                    <strong className="mono">{srv.name}</strong>
                    <span className="mono small muted">{srv.command} {srv.args.join(" ")}</span>
                  </div>
                  <div className="muted small">
                    Roles: {srv.roles.length ? srv.roles.join(", ") : "all"}
                    {srv.secret_env_keys.length > 0 && ` · vaulted env: ${srv.secret_env_keys.join(", ")}`}
                    {srv.require_approval ? " · requires approval" : " · auto-allowed"}
                  </div>
                </div>
                <button type="button" className="icon" onClick={() => void api.removeMcp(srv.name).then(() => loaded.reload())}>
                  remove
                </button>
              </li>
            ))}
          </ul>
        )}
        <form className="ops-form-grid" onSubmit={(e) => void add(e)}>
          <input className="mono" placeholder="Server name (e.g. postgres)" value={name} onChange={(e) => setName(e.target.value)} />
          <input className="mono" placeholder="Command (e.g. npx)" value={command} onChange={(e) => setCommand(e.target.value)} />
          <input className="mono" placeholder="Args (space-separated)" value={argsText} onChange={(e) => setArgsText(e.target.value)} />
          <input className="mono" placeholder="Roles (worker,mid)" value={rolesText} onChange={(e) => setRolesText(e.target.value)} />
          <input className="mono" placeholder="Secret env key (optional)" value={secretKey} onChange={(e) => setSecretKey(e.target.value)} />
          <input type="password" placeholder="Secret env value (vaulted)" value={secretVal} onChange={(e) => setSecretVal(e.target.value)} />
          <div className="ops-span-full row between gap wrap">
            <label className="check small">
              <input type="checkbox" checked={requireApproval} onChange={(e) => setRequireApproval(e.target.checked)} />
              Require approval before tool calls
            </label>
            <button className="small" disabled={!name.trim() || !command.trim()}>Add MCP server</button>
          </div>
        </form>
      </div>
      <Problem error={loaded.error} onRetry={() => void loaded.reload()} />
    </OpsSection>
  );
}

export function RemoteMeshSection() {
  const runners = useLoad(api.runners, []);
  const envs = useLoad(api.environments, []);
  const [kind, setKind] = useState<"runner" | "environment">("runner");
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [token, setToken] = useState("");
  const [probes, setProbes] = useState<Record<string, NodeHealth>>({});

  const add = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!name.trim() || !url.trim()) return;
    try {
      const body = { name: name.trim(), url: url.trim(), token: token.trim() || undefined };
      if (kind === "runner") {
        await api.upsertRunner(body);
        await runners.reload();
      } else {
        await api.upsertEnvironment(body);
        await envs.reload();
      }
      setName("");
      setUrl("");
      setToken("");
      toast(`${kind === "runner" ? "Runner" : "Environment"} saved`);
    } catch (err) {
      toast("Could not save node", err instanceof Error ? err.message : String(err));
    }
  };

  const probe = async (id: string) => {
    try {
      const h = await api.probeRunner(id);
      setProbes((prev) => ({ ...prev, [id]: h }));
    } catch (err) {
      toast("Probe failed", err instanceof Error ? err.message : String(err));
    }
  };

  return (
    <OpsSection
      id="remote-mesh"
      title="Remote runners & environments"
      note="Connect remote tm-daemon hosts over Tailscale or LAN. Bearer tokens are vaulted in the OS keychain and never exported."
    >
      <div className="ops-stack" data-testid="ops-mesh">
        <div className="muted small strong">Runners ({(runners.data ?? []).length})</div>
        {(runners.data ?? []).length === 0 ? (
          <div className="muted small">No remote runners linked.</div>
        ) : (
          <ul className="plain-list">
            {(runners.data ?? []).map((n) => {
              const p = probes[n.id];
              return (
                <li key={n.id} className="ops-row">
                  <div className="grow">
                    <div className="row gap wrap">
                      <strong>{n.name}</strong>
                      <span className="mono small muted">{n.url}</span>
                      {n.has_token && <span className="mono small muted">· vaulted token</span>}
                    </div>
                    {p && (
                      <div className="muted small mono">
                        {p.reachable ? `reachable · ${p.latency_ms}ms${p.version ? ` · v${p.version}` : ""}` : `unreachable · ${p.error ?? "error"}`}
                      </div>
                    )}
                  </div>
                  <button type="button" className="ghost small" onClick={() => void probe(n.id)}>Probe</button>
                  <button type="button" className="icon" onClick={() => void api.removeRunner(n.id).then(() => runners.reload())}>remove</button>
                </li>
              );
            })}
          </ul>
        )}
        <div className="muted small strong">Environments ({(envs.data ?? []).length})</div>
        {(envs.data ?? []).length === 0 ? (
          <div className="muted small">No remote environments linked.</div>
        ) : (
          <ul className="plain-list">
            {(envs.data ?? []).map((n) => (
              <li key={n.id} className="ops-row">
                <div className="grow">
                  <div className="row gap wrap">
                    <strong>{n.name}</strong>
                    <span className="mono small muted">{n.url}</span>
                    {n.has_token && <span className="mono small muted">· vaulted token</span>}
                  </div>
                </div>
                <button type="button" className="icon" onClick={() => void api.removeEnvironment(n.id).then(() => envs.reload())}>remove</button>
              </li>
            ))}
          </ul>
        )}
        <form className="ops-form-grid" onSubmit={(e) => void add(e)}>
          <select value={kind} onChange={(e) => setKind(e.target.value as "runner" | "environment")} aria-label="Node kind">
            <option value="runner">Remote runner</option>
            <option value="environment">Remote environment</option>
          </select>
          <input placeholder="Name (e.g. build-box)" value={name} onChange={(e) => setName(e.target.value)} />
          <input className="mono" placeholder="http://100.x.y.z:8737" value={url} onChange={(e) => setUrl(e.target.value)} />
          <input type="password" placeholder="Bearer token (optional)" value={token} onChange={(e) => setToken(e.target.value)} />
          <div className="ops-span-full">
            <button className="small" disabled={!name.trim() || !url.trim()}>Add {kind}</button>
          </div>
        </form>
      </div>
    </OpsSection>
  );
}

export function OperationsSection() {
  const spend = useLoad(() => api.monthlySpend(), []);
  const egress = useLoad(() => api.egressLog(30), []);
  const stratified = useLoad(() => api.stratifiedScorecard(), []);
  const evalSuite = useLoad(api.evalSuite, []);
  const [gcResult, setGcResult] = useState<GcReport | null>(null);
  const [runningGc, setRunningGc] = useState(false);

  const triggerGc = async () => {
    setRunningGc(true);
    try {
      const report = await api.runGc({ max_runs: 50, max_age_days: 30, prune_worktrees: true });
      setGcResult(report);
      toast("Retention GC complete", `Removed ${report.removed_runs.length} runs, freed ${Math.round(report.bytes_freed / 1024)} KB`);
    } catch (err) {
      toast("GC failed", err instanceof Error ? err.message : String(err));
    } finally {
      setRunningGc(false);
    }
  };

  return (
    <OpsSection
      id="operations"
      title="Operations, egress & auto-routing"
      note="Monthly spend alert status, retention garbage collection, daemon egress audit log, stratified auto-routing, and the benchmark eval suite. Prometheus metrics are exposed at /metrics."
    >
      <div className="ops-stack" data-testid="ops-overview">
        <div className="ops-cards">
          <div className="ops-card">
            <div className="muted small">Monthly spend ({spend.data?.current_month ?? "—"})</div>
            <div className="mono strong ops-metric">
              ${(spend.data?.current_month_usd ?? 0).toFixed(2)}
              {spend.data?.alert_triggered && <span className="ops-alert-tag">alert</span>}
            </div>
            <div className="muted small">
              {spend.data?.total_runs ?? 0} runs
              {spend.data?.alert_threshold_usd != null ? ` · cap $${spend.data.alert_threshold_usd.toFixed(2)}` : ""}
            </div>
          </div>

          <div className="ops-card">
            <div className="muted small">Auto-route pick</div>
            <div className="mono strong ops-metric">{stratified.data?.auto_route?.key ?? "need history"}</div>
            <div className="muted small">{stratified.data?.slices.length ?? 0} stratified slices</div>
          </div>

          <div className="ops-card">
            <div className="muted small">Eval suite</div>
            <div className="mono strong ops-metric">{evalSuite.data?.length ?? 0} tasks</div>
            <div className="muted small">Run via `tm-daemon eval --suite bench/suite`</div>
          </div>
        </div>

        <div className="row gap wrap">
          <button type="button" className="small" disabled={runningGc} onClick={() => void triggerGc()}>
            {runningGc ? "Running GC…" : "Run retention GC"}
          </button>
          <button type="button" className="ghost small" onClick={() => { void spend.reload(); void egress.reload(); void stratified.reload(); }}>
            Reload stats
          </button>
          {gcResult && (
            <span className="muted small mono">
              GC: removed {gcResult.removed_runs.length} runs, kept {gcResult.kept_runs}
              {gcResult.worktrees_pruned ? ", worktrees pruned" : ""}
            </span>
          )}
        </div>

        <div className="ops-subblock">
          <div className="muted small strong">Recent egress audit ({(egress.data ?? []).length})</div>
          {(egress.data ?? []).length === 0 ? (
            <div className="muted small">No outbound network requests logged yet.</div>
          ) : (
            <table className="table small">
              <thead>
                <tr>
                  <th>Host</th>
                  <th>Status</th>
                  <th>Reason</th>
                </tr>
              </thead>
              <tbody>
                {(egress.data ?? []).slice(0, 10).map((e, i) => (
                  <tr key={`${e.at}-${i}`}>
                    <td className="mono">{e.host}</td>
                    <td className="mono">{e.allowed ? "allowed" : "blocked"}</td>
                    <td className="muted">{e.reason ?? "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>
    </OpsSection>
  );
}
