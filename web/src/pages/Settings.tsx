import { useEffect, useState } from "react";
import { api, server, setServer, setToken, DEFAULT_ROLES, type RoleName } from "../api";
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

  useEffect(() => {
    if (window.location.hash) document.querySelector(window.location.hash)?.scrollIntoView();
  }, []);

  const roleDefault = (role: RoleName) => p.roleDefaults[role] ?? { harness: daemon.data?.roles[role] ?? DEFAULT_ROLES[role], model: null };

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

      <Section title="Default harness and model" note="Used by New Run in this browser. Only harnesses and models the providers list returned can be chosen.">
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
                onChange={(harness, model) => setPrefs({ roleDefaults: { ...p.roleDefaults, [role.id]: { harness, model } } })}
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

      <Section id="search" title="Search endpoint" note="The Web search tab sends your query here. Use {q} for the query, or it is added as ?q=. Kept in this browser.">
        <form
          className="row gap"
          onSubmit={(e) => {
            e.preventDefault();
            setPrefs({ searchEndpoint: endpoint.trim() });
            toast(endpoint.trim() ? "Search endpoint saved" : "Search endpoint removed");
          }}
        >
          <input className="grow" placeholder="https://searx.example.org/search?format=json&q={q}" value={endpoint} onChange={(e) => setEndpoint(e.target.value)} />
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

      <Section title="Theme" note="Black and white. Blue is kept for links: path words, pull requests and addresses.">
        <span />
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
