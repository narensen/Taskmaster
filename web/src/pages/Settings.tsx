import { useEffect, useRef, useState } from "react";
import { APPROVAL_MODES, approvalLabel } from "../format";
import { api, server, type ApprovalMode, setServer, setToken, type RoleName } from "../api";
import { loadEditors, useEditors } from "../paths";
import { setPrefs, usePrefs } from "../prefs";
import { ROLES } from "../roles";
import { RoleMark } from "../marks";
import { ProviderPicker } from "../components/ProviderPicker";
import { ALL_ROLES, rememberChoice, savedChoice } from "../rolechoice";
import { useProviders, useSettings, queryClient, keys as qk } from "../query";
import { allowNotifications, tick } from "../notify";
import { SHORTCUTS } from "../shell/Shortcuts";
import type { RoleDefault } from "../prefs";
import { Hosts } from "../settings/Hosts";
import {
  OperationsSection,
  RemoteMeshSection,
  SchedulesSection,
  UserMcpSection,
  WebhookNotificationsSection,
} from "../settings/OpsPanels";
import { WebAccessSection } from "../settings/WebAccessSection";
import { Problem, Switch, useLoad } from "../ui";
import { toast } from "../overlay";
import { appVersion, checkForUpdate, installUpdate, isDesktop, type AvailableUpdate } from "../desktop";

const KEYS: [string, string][] = [
  ...SHORTCUTS.flatMap((g) => g.items.map(([label, combo]) => [combo.join(" "), label] as [string, string])),
  ["Enter / Shift+Enter", "Send, or break the line, in the composer"],
  ["↑ / ↓", "Recall earlier prompts in an empty composer"],
  ["⌘-click / Ctrl-click", "Preview a path word in the side panel"],
  ["Right-click or long-press", "Open, Open with, Reveal in folder, Copy path, Copy path with line"],
];

const slug = (title: string) => title.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "");

function Updates() {
  const [version, setVersion] = useState<string | null>(null);
  const [state, setState] = useState<"idle" | "checking" | "latest" | "available" | "installing" | "failed">("idle");
  const [update, setUpdate] = useState<AvailableUpdate | null>(null);
  const [error, setError] = useState<string | null>(null);
  const check = async () => {
    setState("checking");
    setError(null);
    try {
      const found = await checkForUpdate();
      setUpdate(found);
      setState(found ? "available" : "latest");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setState("failed");
    }
  };
  useEffect(() => {
    void appVersion().then(setVersion);
    void check();
  }, []);
  const install = async () => {
    setState("installing");
    try {
      await installUpdate();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setState("failed");
    }
  };
  return (
    <Section id="updates" title="Updates" note="New versions come from the project's GitHub releases. Each installer is checked against the signing key built into this app before it is installed.">
      <div className="update-box" data-testid="updates" aria-live="polite">
        <div className="row gap wrap">
          <span>
            This is Taskmaster <span className="mono">{version ?? "…"}</span>.
          </span>
          {state === "checking" && (
            <span className="muted">
              <span className="spin" aria-hidden /> Checking
            </span>
          )}
          {state === "latest" && <span className="muted">It is the newest release.</span>}
          {state === "available" && update && <span className="strong">Update available: {update.version}</span>}
          <span className="grow" />
          <button className="small" onClick={() => void check()} disabled={state === "checking" || state === "installing"}>
            Check for updates
          </button>
        </div>
        {state === "available" && update && (
          <div className="update-notes">
            {update.date && <div className="muted small">Released {update.date.slice(0, 10)}</div>}
            {update.notes && <pre className="update-notes-text">{update.notes}</pre>}
            <button className="primary small" onClick={() => void install()}>
              Install and restart
            </button>
          </div>
        )}
        {state === "installing" && (
          <div className="muted small">
            <span className="spin" aria-hidden /> Downloading and checking the signature; Taskmaster restarts when it is installed.
          </div>
        )}
        {error && <div className="error-line small">{error}</div>}
      </div>
    </Section>
  );
}

function Logs() {
  const logs = useLoad(api.logs, []);
  return (
    <Section id="logs" title="Logs" note="The daemon writes here. A terminal you started it from shows the same lines.">
      <div className="row gap wrap">
        <span className="muted small break">{logs.data?.path ?? "No log file yet."}</span>
        <button type="button" className="ghost small" onClick={() => void logs.reload()}>
          Refresh
        </button>
      </div>
      <pre className="log-view" data-testid="daemon-log">
        {logs.data?.lines.length ? logs.data.lines.join("\n") : "Nothing written yet."}
      </pre>
      <Problem error={logs.error} onRetry={() => void logs.reload()} />
    </Section>
  );
}

function SettingsFile({ onImported }: { onImported: (next: Awaited<ReturnType<typeof api.settings>>) => void }) {
  const file = useRef<HTMLInputElement>(null);
  const [armed, setArmed] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const download = async () => {
    setError(null);
    try {
      const exported = await api.exportSettings();
      const blob = new Blob([JSON.stringify(exported.settings, null, 2) + "\n"], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      link.href = url;
      link.download = "taskmaster-settings.json";
      link.click();
      URL.revokeObjectURL(url);
      toast("Settings exported", exported.note);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };
  const read = async (list: FileList | null) => {
    const picked = list?.[0];
    if (!picked) return;
    setError(null);
    try {
      const text = await picked.text();
      const parsed = JSON.parse(text) as unknown;
      const body = parsed && typeof parsed === "object" && "settings" in parsed ? (parsed as { settings: unknown }).settings : parsed;
      onImported(await api.importSettings(body));
      toast("Settings imported", "Tokens in the file were not required. Host connections stay in their own file.");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };
  const reset = async () => {
    if (!armed) {
      setArmed(true);
      return;
    }
    setError(null);
    try {
      onImported(await api.resetSettings());
      setArmed(false);
      toast("Choices reset", "The git project is still here.");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };
  return (
    <Section
      id="settings-file"
      title="Settings file"
      note="Export leaves tokens out. Host connections are kept in their own file and are not in this one. Reset forgets role, editor, approval, checker, code mode, and telemetry choices. It does not delete the git project."
    >
      <div className="settings-transfer" data-testid="settings-transfer">
        <button type="button" className="small" onClick={() => void download()}>
          Export
        </button>
        <button type="button" className="small" onClick={() => file.current?.click()}>
          Import
        </button>
        <input
          ref={file}
          type="file"
          accept="application/json,.json"
          className="sr-only"
          aria-label="Settings file to import"
          onChange={(e) => {
            void read(e.target.files);
            e.target.value = "";
          }}
        />
        <button type="button" className="ghost small" onClick={() => void reset()}>
          {armed ? "Click again to reset" : "Reset choices"}
        </button>
      </div>
      {error && <div className="error-line small">{error}</div>}
    </Section>
  );
}

function Section({ id, title, children, note }: { id?: string; title: string; children: React.ReactNode; note?: string }) {
  return (
    <section className="settings-section" id={id ?? slug(title)} aria-labelledby={`h-${id ?? slug(title)}`}>
      <div className="settings-head">
        <h2 className="section-title" id={`h-${id ?? slug(title)}`}>
          {title}
        </h2>
        {note && <p className="muted small">{note}</p>}
      </div>
      <div className="settings-body">{children}</div>
    </section>
  );
}

const pct = (n: number) => `${Math.round(n * 100)}%`;

/** What each harness and model has cost and delivered across recent runs, from their ledgers. */
function ModelScorecardSection() {
  const card = useLoad(api.scorecard, []);
  const models = card.data?.models ?? [];
  const struggling = new Set(card.data?.struggling ?? []);
  return (
    <Section
      id="scorecard"
      title="Model scorecard"
      note="From the cost ledgers of the last 50 runs here. First pass is the share of units a model built without a repair. The recommended worker passes at least 85% first time, over 3 units or more, for the least cost and time. A model under 50% needs a repair more often than not."
    >
      <div data-testid="model-scorecard">
        {card.error ? (
          <p className="muted small">The scorecard is not available from this daemon.</p>
        ) : card.loading && !card.data ? (
          <p className="muted small">Loading…</p>
        ) : models.length === 0 ? (
          <p className="muted small">No runs yet. The scorecard fills in as runs finish.</p>
        ) : (
          <>
            {card.data?.recommended_worker && (
              <p className="small" data-testid="scorecard-recommended">
                Recommended worker: <span className="badge good">{card.data.recommended_worker}</span>
              </p>
            )}
            <table className="table small scorecard-table">
              <thead>
                <tr>
                  <th scope="col">Model</th>
                  <th scope="col" className="num">Calls</th>
                  <th scope="col" className="num">First pass</th>
                  <th scope="col" className="num">Cache hits</th>
                  <th scope="col" className="num">Avg cost</th>
                  <th scope="col" className="num">Avg time</th>
                </tr>
              </thead>
              <tbody>
                {models.map((m) => (
                  <tr key={m.key}>
                    <td>
                      {m.key}
                      {struggling.has(m.key) && (
                        <>
                          {" "}
                          <span className="badge bad" data-testid="scorecard-struggling">
                            Needs repairs
                          </span>
                        </>
                      )}
                    </td>
                    <td className="num">{m.calls}</td>
                    <td className="num">{m.units_built > 0 ? pct(m.first_pass_rate) : "–"}</td>
                    <td className="num">{pct(m.cache_hit_rate)}</td>
                    <td className="num">{m.priced_calls > 0 ? `$${m.avg_usd_per_call.toFixed(3)}` : "–"}</td>
                    <td className="num">{`${m.avg_seconds.toFixed(1)}s`}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            <p className="muted small">{`${card.data?.runs_sampled ?? 0} runs sampled`}</p>
          </>
        )}
      </div>
    </Section>
  );
}

/** The agent time limits offered, with the one in the settings file if it is another. */
function limitChoices(current: number): number[] {
  const offered = [0, 600, 1200, 1800, 3600];
  return offered.includes(current) ? offered : [0, ...[...offered.slice(1), current].sort((a, b) => a - b)];
}

function limitLabel(secs: number): string {
  if (secs === 0) return "No limit";
  if (secs % 3600 === 0) return secs === 3600 ? "1 hour" : `${secs / 3600} hours`;
  if (secs % 60 === 0) return `${secs / 60} minutes`;
  return `${secs} seconds`;
}

const TOC: { group: string; items: [string, string][] }[] = [
  { group: "Connection", items: [["account", "Account"], ["daemon-address", "Daemon address"], ["project-path", "Project path"], ["commons", "Commons"]] },
  {
    group: "Agents",
    items: [
      ["providers-for-each-role", "Providers for each role"],
      ["time-limit", "Agent time limit"],
      ["checker", "Mid-tier checker"],
      ["code-mode", "Code Mode"],
      ["scorecard", "Model scorecard"],
      ["sandbox", "Worker sandbox"],
      ["web-access", "Web access"],
      ["telemetry", "Telemetry"],
      ["tool-approvals", "Tool approvals"],
    ],
  },
  { group: "Tools", items: [["editor", "Editor"], ["search", "Search endpoint"], ["source-control", "Source control"], ["mcp-servers", "User MCP servers"], ["logs", "Logs"], ["settings-file", "Settings file"]] },
  { group: "Operations", items: [["schedules", "Scheduled runs"], ["remote-mesh", "Remote mesh"], ["operations", "Spend, GC & egress"]] },
  { group: "Appearance", items: [["motion", "Motion"], ["density", "Density"], ["theme", "Theme"]] },
  { group: "Alerts", items: [["notifications", "Notifications"], ["daemon-alerts", "Daemon webhooks"], ["sound", "Sound"]] },
  { group: "Help", items: [["keybindings", "Keybindings"]] },
];

function Keys({ text }: { text: string }) {
  const parts = text.split(" / ");
  return (
    <span className="keys">
      {parts.map((p, i) => (
        <span key={i} className="keys-alt">
          {i > 0 && <span className="muted"> or </span>}
          <kbd>{p}</kbd>
        </span>
      ))}
    </span>
  );
}

export function Settings() {
  const p = usePrefs();
  const health = useLoad(api.health, []);
  const daemon = useLoad(api.settings, []);
  const { list: editorList, fallback } = useEditors();
  const [address, setAddress] = useState(server());
  const [other, setOther] = useState({ url: "", token: "" });
  const [endpoint, setEndpoint] = useState(p.searchEndpoint);
  const [policy, setPolicy] = useState<{ mode: ApprovalMode; timeout: string } | null>(null);
  useEffect(() => {
    const a = daemon.data?.approvals;
    if (a && !policy) setPolicy({ mode: a.mode, timeout: String(a.timeout_secs) });
  }, [daemon.data, policy]);
  const savePolicy = (next: { mode: ApprovalMode; timeout: string }) => {
    setPolicy(next);
    const secs = Number(next.timeout);
    if (!Number.isInteger(secs) || secs < 1 || secs > 86400) return;
    api.setApprovals({ approvals: next.mode, approval_timeout_secs: secs }).then(
      () => toast("Tool approvals saved", `${approvalLabel(next.mode)}, denied after ${secs}s`),
      (err) => toast("Tool approvals were not saved", err instanceof Error ? err.message : String(err)),
    );
  };

  useEffect(() => {
    if (window.location.hash) document.querySelector(window.location.hash)?.scrollIntoView();
  }, []);

  // Role choices live in the daemon's settings file. A change shows at once and is saved behind it.
  const roleSettings = useSettings();
  const roleProviders = useProviders();
  const saved = savedChoice(roleSettings.data);
  const saveRole = (role: RoleName, pick: RoleDefault | null) => {
    const before = roleSettings.data;
    const next = { ...saved };
    if (pick) next[role] = pick;
    else delete next[role];
    if (before) {
      queryClient.setQueryData(qk.settings, {
        ...before,
        roles: { ...before.roles, [role]: pick?.harness },
        models: { ...(before.models ?? {}), [role]: pick?.model ?? null },
      });
    }
    const request = pick ? rememberChoice(next) : api.setRoleDefaults({ roles: { [role]: "" } });
    request.then(
      (settings) => {
        queryClient.setQueryData(qk.settings, settings);
        toast(pick ? "Saved for this project" : "Choice forgotten", `${ROLES.find((r) => r.id === role)?.label}: ${pick ? pick.harness + (pick.model ? ` · ${pick.model}` : "") : "no provider"}`, 2500);
      },
      (err) => {
        if (before) queryClient.setQueryData(qk.settings, before);
        toast("The choice was not saved", err instanceof Error ? err.message : String(err));
      },
    );
  };

  return (
    <div className="page settings">
      <header className="page-head">
        <div className="grow">
          <h1>Settings</h1>
          <p className="page-lede">This daemon, its agents and tools, and how this browser looks.</p>
        </div>
      </header>
      <div className="settings-layout">
      <nav className="settings-toc" aria-label="Settings sections">
        {TOC.map((g) => (
          <div key={g.group} className="toc-group">
            <div className="toc-label">{g.group}</div>
            {g.items.map(([id, title]) => (
              <a
                key={id}
                href={`#${id}`}
                onClick={(e) => {
                  e.preventDefault();
                  document.getElementById(id)?.scrollIntoView({ behavior: "smooth", block: "start" });
                }}
              >
                {title}
              </a>
            ))}
          </div>
        ))}
      </nav>
      <div className="settings-main">

      {isDesktop() && <Updates />}

      <Section title="Account">
        <div className="row between gap wrap">
          <div>
            <div>Signed in on this device{health.data ? ` · by ${health.data.auth.methods.join(" or ")}` : ""}.</div>
            <div className="muted small">The pairing token is kept in this browser and never shown again.</div>
          </div>
          <button className="ghost" onClick={() => setToken(null)}>
            Sign out
          </button>
        </div>
      </Section>

      <Section title="Daemon address" note="Other daemons are kept in this browser and appear as cards in the gallery.">
        <form
          className="row gap"
          onSubmit={(e) => {
            e.preventDefault();
            setServer(address);
            toast("Daemon address saved", address || window.location.origin);
          }}
        >
          <input className="grow" value={address} onChange={(e) => setAddress(e.target.value)} placeholder={`${window.location.origin} (this page's own server)`} />
          <button className="small">Save</button>
        </form>
        <div className="muted small">
          Now: {server() || window.location.origin}
          {health.data && ` · v${health.data.version}`}
        </div>
        <ul className="plain-list">
          {p.daemons.map((d) => (
            <li key={d.url} className="row between gap">
              <span className="clip">{d.url}</span>
              <span className="row gap">
                <button
                  className="icon"
                  onClick={() => setPrefs({ daemons: p.daemons.map((x) => (x.url === d.url ? { ...x, pinned: !x.pinned } : x)) })}
                >
                  {d.pinned ? "unpin" : "pin"}
                </button>
                <button className="icon" onClick={() => setPrefs({ daemons: p.daemons.filter((x) => x.url !== d.url) })}>
                  remove
                </button>
              </span>
            </li>
          ))}
        </ul>
        <form
          className="row gap wrap daemon-add"
          onSubmit={(e) => {
            e.preventDefault();
            const url = other.url.trim().replace(/\/+$/, "");
            if (!url) return;
            setPrefs({ daemons: [...p.daemons.filter((d) => d.url !== url), { url, token: other.token.trim() || null, pinned: false }] });
            setOther({ url: "", token: "" });
          }}
        >
          <input className="grow" aria-label="Another daemon's address" placeholder="Another daemon, http://100.x.y.z:8737" value={other.url} onChange={(e) => setOther({ ...other, url: e.target.value })} />
          <input type="password" aria-label="Its token" placeholder="its token (optional)" value={other.token} onChange={(e) => setOther({ ...other, token: e.target.value })} />
          <button className="small">Add</button>
        </form>
      </Section>

      <Section title="Project path" note="To serve another project, start the daemon there: tm-daemon serve <project>. That is a restart.">
        <div className="break">{daemon.data?.project ?? "…"}</div>
      </Section>

      <Section title="Commons" note={`Set in ${daemon.data?.config ?? daemon.data?.config_default ?? ".taskmaster/config.json"}; a change needs a config edit and a restart.`}>
        <div className="break">{daemon.data?.commons ?? "…"}</div>
        <div className="muted small">
          {daemon.data ? (daemon.data.commons_remote ? "Shared with a remote Commons." : "Not shared with a remote Commons.") : ""} Records are kept as{" "}
          {daemon.data?.store ?? "…"}.
        </div>
      </Section>

      <Section title="Providers for each role" note="Kept for this project by the daemon. New Run starts from these, and you can change any of them there for one run. Nothing is chosen for you.">
        <Problem error={roleProviders.error} onRetry={() => void roleProviders.refetch()} />
        <div className="role-table">
          {ALL_ROLES.map((role) => {
            const meta = ROLES.find((r) => r.id === role)!;
            return (
              <div key={role} className="role-line">
                <span className="role-line-name">
                  <RoleMark role={role} size={13} />
                  {meta.label}
                </span>
                <span className="role-line-hint">{meta.hint}</span>
                <ProviderPicker label={`${meta.label} provider`} providers={roleProviders.data ?? []} loading={roleProviders.isLoading} value={saved[role] ?? null} onChange={(pick) => saveRole(role, pick)} />
                <button type="button" className="ghost small" disabled={!saved[role]} onClick={() => saveRole(role, null)}>
                  Forget
                </button>
              </div>
            );
          })}
        </div>
      </Section>

      <Section title="Editor" note="A path word opens in this editor, on the machine that holds the project.">
        <div className="row gap wrap">
          <select value={p.editor ?? ""} onChange={(e) => setPrefs({ editor: e.target.value || null })} aria-label="Default editor">
            <option value="">automatic ({editorList.find((e) => e.id === fallback)?.name ?? fallback})</option>
            {editorList
              .filter((e) => e.installed)
              .map((e) => (
                <option key={e.id} value={e.id}>
                  {e.name}
                </option>
              ))}
          </select>
          <button className="ghost small" onClick={() => void loadEditors(true)}>
            Rescan
          </button>
        </div>
        <ul className="editor-list small" data-testid="editors">
          {editorList.map((e) => (
            <li key={e.id} className={e.installed ? "strong" : "muted"}>
              {e.name} <span className="muted">{e.command}</span> {e.installed ? "· installed" : "· not found"}
            </li>
          ))}
        </ul>
      </Section>

      {daemon.data && (
        <Section
          id="time-limit"
          title="Agent time limit"
          note="Optional cap on how long a single agent call may run. Off by default (No limit) so workers and planners can finish without arbitrary cutoffs."
        >
          <label className="row gap small" data-testid="time-limit">
            <span className="muted">Stop an agent after</span>
            <select
              aria-label="Agent time limit"
              value={String(daemon.data.agents?.timeout_secs ?? 0)}
              onChange={(e) => {
                const secs = Number(e.target.value);
                void api.setAgentTimeout(secs).then(
                  (next) => {
                    daemon.setData(next);
                    toast("Agent time limit saved", secs === 0 ? "No limit for new runs" : `${limitLabel(secs)} for new runs`);
                  },
                  (err) => toast("Not saved", err instanceof Error ? err.message : String(err)),
                );
              }}
            >
              {limitChoices(daemon.data.agents?.timeout_secs ?? 0).map((secs) => (
                <option key={secs} value={secs}>
                  {limitLabel(secs)}
                </option>
              ))}
            </select>
          </label>
        </Section>
      )}

      {daemon.data && (
        <Section
          id="checker"
          title="Mid-tier checker"
          note="After a page passes its own checks, the mid model reads its diff, its screenshot, its brief and the worker's last words, and answers pass or a short fix list. It runs beside the build, only on pages that change UI files or large ones, and waits at most 20 seconds at the end. Each page is checked once, so it adds at most one repair. Uses the mid role's harness and model."
        >
          <label className="row gap" data-testid="checker-setting">
            <Switch
              on={daemon.data.checker?.enabled ?? false}
              label="Check finished pages with the mid model"
              onChange={(on) =>
                void api.setChecker(on).then(
                  (next) => {
                    daemon.setData(next);
                    toast(on ? "The checker is on for new runs" : "The checker is off");
                  },
                  (err) => toast("Not saved", err instanceof Error ? err.message : String(err)),
                )
              }
            />
            <span>{daemon.data.checker?.enabled ? "On: new runs get a second look per page" : "Off"}</span>
          </label>
        </Section>
      )}

      {daemon.data && (
        <Section
          id="code-mode"
          title="Code Mode"
          note="Every agent gets run_code: one call runs a short JavaScript program that reads, edits, patches, searches and checks files, instead of a tool call per step. Builders get the full set; the planner, reviewers and verifiers get a read-only one. It runs in Node with no file, process or network access of its own, under the same permissions as the agent's tools. Claude Code workers use Claude Code's own code mode. On by default for every harness."
        >
          <label className="row gap" data-testid="code-mode-setting">
            <Switch
              on={daemon.data.code_mode?.enabled ?? true}
              label="Use Code Mode for workers"
              onChange={(on) =>
                void api.setCodeMode(on).then(
                  (next) => {
                    daemon.setData(next);
                    toast(on ? "Code Mode is on for new runs" : "Code Mode is off");
                  },
                  (err) => toast("Not saved", err instanceof Error ? err.message : String(err)),
                )
              }
            />
            <span>{daemon.data.code_mode?.enabled ? "On for new runs" : "Off"}</span>
          </label>
        </Section>
      )}

      <ModelScorecardSection />

      {daemon.data && (
        <Section
          id="sandbox"
          title="Worker sandbox"
          note="A builder whose provider runs inside the sandbox (bubblewrap for Antigravity, Grok and OpenCode; Codex's own) can run commands: its tests, an install, a lockfile update. It can write only its worktree, its own scratch folder and the run's package caches; the rest of this machine is read-only to it. Claude Code workers have no shell either way."
        >
          {daemon.data.sandbox && !daemon.data.sandbox.available && (
            <p className="muted small" data-testid="sandbox-missing">
              Bubblewrap cannot run on this machine, so no worker gets a shell whatever is set here.
            </p>
          )}
          <label className="row gap" data-testid="sandbox-shell-setting">
            <Switch
              on={daemon.data.sandbox?.shell ?? true}
              label="Let builders run commands in the sandbox"
              onChange={(on) =>
                void api.setSandbox({ shell: on }).then(
                  (next) => {
                    daemon.setData(next);
                    toast(on ? "Builders can run commands in new runs" : "Builders get no shell in new runs");
                  },
                  (err) => toast("Not saved", err instanceof Error ? err.message : String(err)),
                )
              }
            />
            <span>{daemon.data.sandbox?.shell ?? true ? "On: tests and installs run in the sandbox" : "Off: no commands"}</span>
          </label>
          <label className="row gap" data-testid="sandbox-indexes-setting">
            <Switch
              on={daemon.data.sandbox?.package_indexes ?? true}
              label="Let fetch tools reach package indexes"
              onChange={(on) =>
                void api.setSandbox({ package_indexes: on }).then(
                  (next) => {
                    daemon.setData(next);
                    toast(on ? "Fetches may reach PyPI, npm and crates.io" : "Fetches are refused");
                  },
                  (err) => toast("Not saved", err instanceof Error ? err.message : String(err)),
                )
              }
            />
            <span>{daemon.data.sandbox?.package_indexes ?? true ? "On: PyPI, npm and crates.io only" : "Off"}</span>
          </label>
        </Section>
      )}

      <WebAccessSection />

      {daemon.data && (
        <Section
          id="telemetry"
          title="Telemetry"
          note="Off until you turn it on. When it is on, the daemon records a count in its log. It sends that count only if TASKMASTER_TELEMETRY_URL is an https address you set. Prompts, paths, and tokens are never included. With no address, nothing leaves this machine."
        >
          <label className="row gap" data-testid="telemetry-setting">
            <Switch
              on={daemon.data.telemetry?.enabled ?? false}
              label="Send anonymous counts"
              onChange={(on) =>
                void api.setTelemetry(on).then(
                  (next) => {
                    daemon.setData(next);
                    toast(on ? "Anonymous counts are on" : "Anonymous counts are off");
                  },
                  (err) => toast("Not saved", err instanceof Error ? err.message : String(err)),
                )
              }
            />
            <span>{daemon.data.telemetry?.enabled ? "On. Nothing is sent until an https collector is configured." : "Off"}</span>
          </label>
        </Section>
      )}

      <Logs />
      <SettingsFile onImported={(next) => daemon.setData(next)} />

      {policy && (
        <Section
          title="Tool approvals"
          note="Risky tool calls (a shell command, a write outside the worktree) wait for you on the run page; no answer in time is a denial. Ask on every call stops before each one; Approvals off keeps only the automatic gate."
        >
          <div className="row gap wrap" data-testid="approval-policy">
            <div className="seg" role="radiogroup" aria-label="Approval mode">
              {APPROVAL_MODES.map((m) => (
                <button key={m} role="radio" aria-checked={policy.mode === m} className={policy.mode === m ? "on" : ""} onClick={() => savePolicy({ ...policy, mode: m })}>
                  {approvalLabel(m)}
                </button>
              ))}
            </div>
            <label className="row gap small">
              <span className="muted">Deny after</span>
              <input
                type="number"
                min={1}
                max={86400}
                style={{ width: 90 }}
                aria-label="Approval timeout in seconds"
                value={policy.timeout}
                onChange={(e) => setPolicy({ ...policy, timeout: e.target.value })}
                onBlur={() => savePolicy(policy)}
              />
              <span className="muted">seconds</span>
            </label>
          </div>
        </Section>
      )}

      <Section id="search" title="Search endpoint" note="The Web search tab asks the daemon to search here. Empty = the daemon default (local SearXNG at http://127.0.0.1:8888/search?format=json&q={q}). Use {q} for the query, or it is added as ?q=. Kept in this browser.">
        <form
          className="row gap"
          onSubmit={(e) => {
            e.preventDefault();
            setPrefs({ searchEndpoint: endpoint.trim() });
            toast(endpoint.trim() ? "Search endpoint saved" : "Search endpoint removed");
          }}
        >
          <input className="grow" placeholder="empty = http://127.0.0.1:8888/search?format=json&q={q}" value={endpoint} onChange={(e) => setEndpoint(e.target.value)} />
          <button className="small">Save</button>
        </form>
      </Section>

      <Section
        id="source-control"
        title="Source control"
        note="Connect a git host to create a repository and push this project. A signed-in gh or glab is used on its own. Otherwise paste a token. Tokens stay on this machine. Draft pull request still uses gh."
      >
        <Hosts />
      </Section>

      <UserMcpSection />
      <SchedulesSection />
      <RemoteMeshSection />
      <OperationsSection />

      <Section title="Motion" note="The system's reduced-motion setting always wins.">
        <label className="check">
          <input type="checkbox" checked={p.motion} onChange={(e) => setPrefs({ motion: e.target.checked })} />
          Motion on
        </label>
      </Section>

      <Section title="Density">
        <div className="seg" role="radiogroup" aria-label="Density">
          {(["comfortable", "compact"] as const).map((d) => (
            <button key={d} role="radio" aria-checked={p.density === d} className={p.density === d ? "on" : ""} onClick={() => setPrefs({ density: d })}>
              {d}
            </button>
          ))}
        </div>
      </Section>

      <Section title="Theme" note="Dark is the default. Dark uses ink and graphite only — no colour except status. Light keeps verdigris.">
        <div className="seg" role="radiogroup" aria-label="Theme">
          {(["dark", "light"] as const).map((t) => (
            <button key={t} role="radio" aria-checked={p.theme === t} className={p.theme === t ? "on" : ""} onClick={() => setPrefs({ theme: t })}>
              {t}
            </button>
          ))}
        </div>
      </Section>

      <Section title="Notifications" note="When a run finishes, fails, or waits on you, and the window is not in front. Clicking one opens the run.">
        <div className="row between gap">
          <span>Desktop notifications</span>
          <Switch
            on={p.notify}
            label="Desktop notifications"
            onChange={(on) => {
              setPrefs({ notify: on });
              if (on) void allowNotifications().then((ok) => !ok && toast("Notifications are blocked", "Allow them for this app in your system settings."));
            }}
          />
        </div>
      </Section>

      <WebhookNotificationsSection />

      <Section title="Sound" note="A very soft tick when a run finishes. Off unless you turn it on.">
        <div className="row between gap">
          <span>Completion tick</span>
          <span className="row gap">
            <button type="button" className="ghost small" onClick={tick}>
              Play it
            </button>
            <Switch on={p.sound} label="Completion tick" onChange={(on) => (setPrefs({ sound: on }), on && tick())} />
          </span>
        </div>
      </Section>

      <Section title="Keybindings">
        <table className="table small">
          <tbody>
            {KEYS.map(([k, what]) => (
              <tr key={k}>
                <td className="nowrap">
                  <Keys text={k} />
                </td>
                <td>{what}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </Section>
      </div>
      </div>
    </div>
  );
}
