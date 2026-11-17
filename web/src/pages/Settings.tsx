import { useEffect, useState } from "react";
import { api, server, type ApprovalMode, setServer, setToken, DEFAULT_ROLES, type RoleName, type Roles, type Models } from "../api";
import { loadEditors, useEditors } from "../paths";
import { setPrefs, usePrefs } from "../prefs";
import { ROLES, RolePicker } from "../roles";
import { Problem, useLoad } from "../ui";
import { toast } from "../overlay";

const KEYS: [string, string][] = [
  ["⌘K / Ctrl-K", "Command palette: runs, pages, commands. Start with > for commands only"],
  ["/", "Commands and skills, outside a text field or at the start of the composer"],
  ["⌘1–⌘9 / Ctrl-1–9", "Jump to the first nine runs, when no text field is focused"],
  ["⌘Z / Ctrl-Z", "Undo the last pin, settle or archive while its Undo shows, even from the composer"],
  ["⌘W / Ctrl-W", "Close the focused tab of a run's right panel (where the browser lets a page have it)"],
  ["⌘⇧T / Ctrl-Shift-T", "Reopen the last closed tab"],
  ["Enter / Shift+Enter", "Send / break the line, in the composer"],
  ["↑ / ↓", "Recall earlier prompts in an empty composer"],
  ["⌘-click / Ctrl-click", "Preview a path word in the right panel"],
  ["Right-click or long-press", "Open, Open with, Reveal in folder, Copy path, Copy path with line"],
];

function Section({ id, title, children, note }: { id?: string; title: string; children: React.ReactNode; note?: string }) {
  return (
    <section className="settings-section" id={id}>
      <h2 className="section-title">{title}</h2>
      {children}
      {note && <p className="muted small">{note}</p>}
    </section>
  );
}

export function Settings() {
  const p = usePrefs();
  const health = useLoad(api.health, []);
  const daemon = useLoad(api.settings, []);
  const providers = useLoad(() => api.providers(), []);
  const gh = useLoad(api.gh, []);
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
      () => toast("Tool approvals saved", `${next.mode}, ${secs}s`),
      (err) => toast("Tool approvals were not saved", err instanceof Error ? err.message : String(err)),
    );
  };

  useEffect(() => {
    if (window.location.hash) document.querySelector(window.location.hash)?.scrollIntoView();
  }, []);

  // Role defaults live in the daemon's settings file; this browser keeps no copy of its own.
  const [saved, setSaved] = useState<{ roles: Roles; models: Models } | null>(null);
  useEffect(() => {
    if (daemon.data && !saved) setSaved({ roles: { ...DEFAULT_ROLES, ...daemon.data.roles }, models: daemon.data.models ?? {} });
  }, [daemon.data, saved]);
  const roleDefault = (role: RoleName) => ({
    harness: saved?.roles[role] ?? daemon.data?.roles[role] ?? DEFAULT_ROLES[role],
    model: saved?.models[role] ?? null,
  });
  const saveRole = (role: RoleName, harness: string, model: string | null) => {
    const next = { roles: { ...(saved?.roles ?? DEFAULT_ROLES), [role]: harness }, models: { ...(saved?.models ?? {}), [role]: model } };
    setSaved(next);
    api.setRoleDefaults(next).then(
      () => toast("Role defaults saved", `${role}: ${harness}${model ? ` · ${model}` : ""}`),
      (err) => toast("Role defaults were not saved", err instanceof Error ? err.message : String(err)),
    );
  };

  return (
    <div className="page narrow settings">
      <header className="page-head">
        <h1>Settings</h1>
      </header>

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
          className="row gap wrap"
          onSubmit={(e) => {
            e.preventDefault();
            const url = other.url.trim().replace(/\/+$/, "");
            if (!url) return;
            setPrefs({ daemons: [...p.daemons.filter((d) => d.url !== url), { url, token: other.token.trim() || null, pinned: false }] });
            setOther({ url: "", token: "" });
          }}
        >
          <input className="grow" placeholder="Another daemon, http://100.x.y.z:8737" value={other.url} onChange={(e) => setOther({ ...other, url: e.target.value })} />
          <input type="password" placeholder="its token (optional)" value={other.token} onChange={(e) => setOther({ ...other, token: e.target.value })} />
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

      <Section title="Default harness and model" note="Kept by the daemon and used by every New Run, in any browser. Only harnesses and models the providers list returned can be chosen.">
        <Problem error={providers.error} onRetry={providers.reload} />
        <div className="roles">
          {ROLES.map((role) => {
            const d = roleDefault(role.id);
            return (
              <RolePicker
                key={role.id}
                role={role}
                providers={providers.data ?? []}
                harness={d.harness}
                model={d.model}
                onChange={(harness, model) => saveRole(role.id, harness, model)}
              />
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

      {policy && (
        <Section
          title="Tool approvals"
          note="Risky tool calls (a shell command, a write outside the worktree) wait for you on the run page; no answer in time is a denial. Always asks before every call; never keeps the automatic gate only."
        >
          <div className="row gap wrap" data-testid="approval-policy">
            <div className="seg" role="radiogroup" aria-label="Approval mode">
              {(["always", "risky", "never"] as const).map((m) => (
                <button key={m} role="radio" aria-checked={policy.mode === m} className={policy.mode === m ? "on" : ""} onClick={() => savePolicy({ ...policy, mode: m })}>
                  {m}
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

      <Section title="Source control" note="Draft pull request uses gh on the project machine, as this account.">
        <div className="row between gap wrap">
          <div data-testid="gh">
            {gh.loading && !gh.data
              ? "Asking gh…"
              : gh.data?.account
                ? `GitHub: ${gh.data.account}`
                : gh.data?.installed
                  ? "gh is installed but not signed in"
                  : "gh is not installed on the project machine"}
          </div>
          <button className="ghost small" onClick={() => void gh.reload()}>
            Rescan
          </button>
        </div>
      </Section>

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

      <Section title="Theme" note="Dark is the default. Blue marks focus, links and the active state.">
        <div className="seg" role="radiogroup" aria-label="Theme">
          {(["dark", "light"] as const).map((t) => (
            <button key={t} role="radio" aria-checked={p.theme === t} className={p.theme === t ? "on" : ""} onClick={() => setPrefs({ theme: t })}>
              {t}
            </button>
          ))}
        </div>
      </Section>

      <Section title="Keybindings">
        <table className="table small">
          <tbody>
            {KEYS.map(([k, what]) => (
              <tr key={k}>
                <td className="strong nowrap">{k}</td>
                <td>{what}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </Section>
    </div>
  );
}
