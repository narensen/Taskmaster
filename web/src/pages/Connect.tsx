// Connect: everything that reaches this daemon, in one place.
//
// - Pair a device: a one-time link and QR code. Opening it on a phone or another computer
//   signs that device in, with nothing to type.
// - Devices: every paired device, named, with when it was last seen, rename and revoke.
// - Machines: other Taskmaster daemons, added by pasting their pairing link.
// - Agents: a token and copy-paste setup so Claude Code, Codex or any MCP client can start,
//   watch and steer runs here.

import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Bot, Check, Copy, ExternalLink, Laptop, Link2, Monitor, Pencil, Plug, RefreshCw, Server, Smartphone, Trash2, Wifi } from "lucide-react";
import {
  api,
  callAt,
  deviceLabel,
  pairAt,
  type ConnectRoute,
  type ConnectView,
  type Device,
  type DeviceKind,
  type Health,
  type PairingOffer,
  type RunSummary,
} from "../api";
import { QrCode } from "../components/QrCode";
import { ago } from "../format";
import { toast } from "../overlay";
import { setPrefs, usePrefs, type OtherDaemon } from "../prefs";
import { Problem, useLoad } from "../ui";

const ROUTE_NAME: Record<ConnectRoute["kind"], string> = {
  "this-computer": "This computer",
  "local-network": "Local network",
  tailscale: "Tailscale",
  network: "Network",
};

const KIND_NAME: Record<DeviceKind, string> = {
  phone: "Phone",
  browser: "Browser",
  desktop: "Desktop app",
  machine: "Another machine",
  agent: "Outside agent",
};

function KindIcon({ kind }: { kind: DeviceKind | string }) {
  const props = { size: 15, strokeWidth: 1.7, "aria-hidden": true } as const;
  if (kind === "phone") return <Smartphone {...props} />;
  if (kind === "desktop") return <Monitor {...props} />;
  if (kind === "machine") return <Server {...props} />;
  if (kind === "agent") return <Bot {...props} />;
  return <Laptop {...props} />;
}

function copy(text: string, what: string) {
  void navigator.clipboard?.writeText(text).then(
    () => toast(`${what} copied`, undefined, 2000),
    () => toast(`${what} was not copied`, "Select it and copy it by hand."),
  );
}

function CopyButton({ text, what, testId }: { text: string; what: string; testId?: string }) {
  const [done, setDone] = useState(false);
  return (
    <button
      type="button"
      className="ghost small connect-copy"
      data-testid={testId}
      onClick={() => {
        copy(text, what);
        setDone(true);
        window.setTimeout(() => setDone(false), 1500);
      }}
    >
      {done ? <Check size={13} aria-hidden /> : <Copy size={13} aria-hidden />}
      {done ? "Copied" : "Copy"}
    </button>
  );
}

function Card({ id, icon, title, note, children }: { id: string; icon: ReactNode; title: string; note?: ReactNode; children: ReactNode }) {
  return (
    <section className="settings-section connect-card" id={id} aria-labelledby={`h-${id}`}>
      <div className="settings-head">
        <h2 className="section-title connect-title" id={`h-${id}`}>
          {icon}
          {title}
        </h2>
        {note && <p className="muted small">{note}</p>}
      </div>
      <div className="settings-body">{children}</div>
    </section>
  );
}

/** Where this daemon can be reached, and how to open it to the network when it cannot. */
function Reach({ view }: { view: ConnectView | null }) {
  if (!view) return null;
  const project = view.project || "<project>";
  return (
    <div className="connect-reach" data-testid="connect-reach">
      <span className="connect-reach-label">
        <Wifi size={13} aria-hidden /> Reachable at
      </span>
      {view.routes.map((route) => (
        <span key={route.url} className={`connect-route kind-${route.kind}`} title={route.url}>
          <span className="connect-route-kind">{ROUTE_NAME[route.kind]}</span>
          <span className="mono">{route.address}</span>
        </span>
      ))}
      {view.only_this_computer && (
        <div className="connect-hint" data-testid="connect-only-here">
          <p>
            Only this computer can reach Taskmaster now. To pair a phone or another computer, start the daemon on your
            network, or on your tailnet only:
          </p>
          <div className="connect-cmd-row">
            <code className="mono clip">tm-daemon serve {project} --lan</code>
            <CopyButton text={`tm-daemon serve ${project} --lan`} what="Command" />
          </div>
          <div className="connect-cmd-row">
            <code className="mono clip">tm-daemon serve {project} --tailscale</code>
            <CopyButton text={`tm-daemon serve ${project} --tailscale`} what="Command" />
          </div>
        </div>
      )}
    </div>
  );
}

function useNow(active: boolean) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [active]);
  return now;
}

function PairCard({ view, devices, onChange }: { view: ConnectView | null; devices: Device[] | null; onChange: () => void }) {
  const [label, setLabel] = useState("");
  const [kind, setKind] = useState<DeviceKind>("phone");
  const [offer, setOffer] = useState<PairingOffer | null>(null);
  const [route, setRoute] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const seenAt = useRef<number | null>(null);
  const now = useNow(!!offer);
  const left = offer ? Math.max(0, Math.round((Date.parse(offer.expires_at) - now) / 1000)) : 0;
  const expired = !!offer && left === 0;

  // A device that pairs while the link is shown: say so, and put the link away.
  useEffect(() => {
    if (!devices) return;
    // Agent tokens are made on this page too; only a device that opened the link counts.
    const people = devices.filter((d) => d.kind !== "agent");
    if (!offer) {
      seenAt.current = people.length;
      return;
    }
    if (seenAt.current !== null && people.length > seenAt.current) {
      const newest = [...people].sort((a, b) => (b.created || "").localeCompare(a.created || ""))[0];
      toast("Paired", newest ? `${newest.label} is signed in.` : "A new device is signed in.", 4000);
      setOffer(null);
    }
    seenAt.current = people.length;
  }, [devices, offer]);

  const make = async () => {
    setBusy(true);
    setError(null);
    try {
      const made = await api.createPairing({ label: label.trim(), kind });
      setOffer(made);
      // The link a phone can open: the first route that is not this computer.
      const away = made.links.findIndex((l) => l.kind !== "this-computer");
      setRoute(away >= 0 ? away : 0);
      onChange();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };

  const link = offer?.links[route];
  return (
    <Card
      id="pair"
      icon={<Link2 size={15} aria-hidden />}
      title="Pair a device"
      note="A one-time link signs a phone, a tablet or another computer in: scan the code or open the link there. It works once, for ten minutes."
    >
      <div className="connect-form">
        <label className="connect-field">
          <span>Name</span>
          <input value={label} onChange={(e) => setLabel(e.target.value)} placeholder="Naren's phone" maxLength={60} data-testid="pair-label" />
        </label>
        <label className="connect-field">
          <span>Kind</span>
          <select value={kind} onChange={(e) => setKind(e.target.value as DeviceKind)} aria-label="Device kind">
            {(["phone", "browser", "desktop", "machine"] as const).map((k) => (
              <option key={k} value={k}>
                {KIND_NAME[k]}
              </option>
            ))}
          </select>
        </label>
        <button type="button" className="primary small" disabled={busy} onClick={() => void make()} data-testid="pair-make">
          {busy ? "Making…" : offer ? "Make another" : "Make pairing link"}
        </button>
      </div>
      <Problem error={error} />
      {offer && link && (
        <div className={`connect-offer ${expired ? "expired" : ""}`} data-testid="pair-offer">
          <QrCode value={link.url} label={`QR code for the pairing link to ${link.address}`} />
          <div className="connect-offer-side">
            {offer.links.length > 1 && (
              <div className="connect-routes" role="radiogroup" aria-label="Which address the device uses">
                {offer.links.map((l, i) => (
                  <button key={l.url} type="button" role="radio" aria-checked={i === route} className={`connect-route-pick ${i === route ? "on" : ""}`} onClick={() => setRoute(i)}>
                    {ROUTE_NAME[l.kind]}
                  </button>
                ))}
              </div>
            )}
            <div className="connect-cmd-row">
              <code className="mono clip" data-testid="pair-link">
                {link.url}
              </code>
              <CopyButton text={link.url} what="Pairing link" testId="pair-copy" />
            </div>
            <p className="muted small">
              {expired ? (
                "This link expired. Make another."
              ) : (
                <>
                  Good once · expires in {Math.floor(left / 60)}:{String(left % 60).padStart(2, "0")}
                  {offer.label ? <> · pairs as “{offer.label}”</> : null}
                </>
              )}
            </p>
            {link.kind === "this-computer" && (
              <p className="small connect-warn">Only a browser on this computer can open this address. {view?.only_this_computer ? "Start the daemon with --lan or --tailscale for other devices." : "Pick another address for a phone."}</p>
            )}
            <p className="muted small">Treat the link like a password: anyone who opens it first gets in.</p>
          </div>
        </div>
      )}
    </Card>
  );
}

function DeviceRow({ device, onChange }: { device: Device; onChange: () => void }) {
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState(device.label);
  const [arming, setArming] = useState(false);
  useEffect(() => {
    if (!arming) return;
    const timer = window.setTimeout(() => setArming(false), 5000);
    return () => window.clearTimeout(timer);
  }, [arming]);
  const save = async () => {
    setEditing(false);
    if (!name.trim() || name.trim() === device.label) return;
    try {
      await api.renameDevice(device.id, name.trim());
      onChange();
    } catch (err) {
      toast("Not renamed", err instanceof Error ? err.message : String(err));
    }
  };
  const revoke = async () => {
    if (!arming) {
      setArming(true);
      return;
    }
    try {
      await api.revokeDevice(device.id);
      toast("Revoked", `${device.label} is signed out.`, 3000);
      onChange();
    } catch (err) {
      toast("Not revoked", err instanceof Error ? err.message : String(err));
    }
  };
  return (
    <li className="connect-device" data-testid="device-row" data-kind={device.kind}>
      <span className="connect-device-icon">
        <KindIcon kind={device.kind} />
      </span>
      <span className="connect-device-main">
        {editing ? (
          <input
            autoFocus
            value={name}
            maxLength={60}
            aria-label="Device name"
            onChange={(e) => setName(e.target.value)}
            onBlur={() => void save()}
            onKeyDown={(e) => {
              if (e.key === "Enter") void save();
              if (e.key === "Escape") {
                setName(device.label);
                setEditing(false);
              }
            }}
          />
        ) : (
          <span className="connect-device-name">
            {device.label}
            {device.current && <span className="badge quiet">This device</span>}
          </span>
        )}
        <span className="muted small">
          {KIND_NAME[device.kind] ?? device.kind}
          {device.created ? ` · paired ${ago(device.created)}` : ""}
          {device.last_seen ? ` · seen ${ago(device.last_seen)}` : ""}
        </span>
      </span>
      <span className="connect-device-actions">
        <button type="button" className="icon" aria-label={`Rename ${device.label}`} title="Rename" onClick={() => setEditing(true)}>
          <Pencil size={13} />
        </button>
        <button type="button" className={`ghost small ${arming ? "danger" : ""}`} data-testid="device-revoke" onClick={() => void revoke()}>
          <Trash2 size={13} aria-hidden /> {arming ? (device.current ? "Sign this device out" : "Confirm revoke") : "Revoke"}
        </button>
      </span>
    </li>
  );
}

function DevicesCard({ devices, error, onChange }: { devices: Device[] | null; error: unknown; onChange: () => void }) {
  const people = (devices ?? []).filter((d) => d.kind !== "agent");
  return (
    <Card id="devices" icon={<Smartphone size={15} aria-hidden />} title="Devices" note="Everything signed in to this daemon. Revoking one signs it out at once.">
      <Problem error={error} onRetry={onChange} />
      {devices && people.length === 0 && <p className="muted small">No devices yet.</p>}
      <ul className="connect-devices" data-testid="devices">
        {people.map((d) => (
          <DeviceRow key={d.id} device={d} onChange={onChange} />
        ))}
      </ul>
    </Card>
  );
}

interface MachineState {
  health: Health;
  runs: RunSummary[];
}

function MachineRow({ daemon }: { daemon: OtherDaemon }) {
  const p = usePrefs();
  const state = useLoad<MachineState>(
    async () => {
      const [health, list] = await Promise.all([callAt<Health>(daemon.url, daemon.token, "/api/health"), callAt<{ runs: RunSummary[] }>(daemon.url, daemon.token, "/api/runs")]);
      return { health, runs: list.runs };
    },
    [daemon.url, daemon.token],
    15000,
  );
  const runs = state.data?.runs ?? [];
  const working = runs.filter((r) => r.active).length;
  const needs = runs.filter((r) => (r.pending_approvals ?? 0) > 0 || (r.phase === "awaiting_approval" && !r.active)).length;
  const online = !!state.data && !state.error;
  return (
    <li className="connect-device" data-testid="machine-row">
      <span className={`connect-dot ${online ? "on" : state.loading ? "" : "off"}`} aria-label={online ? "online" : "offline"} />
      <span className="connect-device-main">
        <span className="connect-device-name mono">{daemon.url.replace(/^https?:\/\//, "")}</span>
        <span className="muted small">
          {state.loading && !state.data
            ? "Checking…"
            : online
              ? `v${state.data!.health.version} · ${runs.length} run${runs.length === 1 ? "" : "s"}${working ? ` · ${working} working` : ""}${needs ? ` · ${needs} need${needs === 1 ? "s" : ""} you` : ""}`
              : "Not reachable now. It may be asleep, or on another network."}
        </span>
      </span>
      <span className="connect-device-actions">
        <button type="button" className="icon" aria-label="Check again" title="Check again" onClick={() => void state.reload()}>
          <RefreshCw size={13} />
        </button>
        <a className="button ghost small" href={daemon.url} target="_blank" rel="noreferrer">
          <ExternalLink size={13} aria-hidden /> Open
        </a>
        <button type="button" className="ghost small" onClick={() => setPrefs({ daemons: p.daemons.filter((d) => d.url !== daemon.url) })}>
          Remove
        </button>
      </span>
    </li>
  );
}

function MachinesCard() {
  const p = usePrefs();
  const [link, setLink] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      const paired = await pairAt(link, `${deviceLabel().label} · Taskmaster`);
      setPrefs({ daemons: [...p.daemons.filter((d) => d.url !== paired.url), { url: paired.url, token: paired.token, pinned: false }] });
      setLink("");
      toast("Machine added", paired.url, 3000);
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Card
      id="machines"
      icon={<Server size={15} aria-hidden />}
      title="Other machines"
      note={<>On the other machine, open Connect, make a pairing link of kind “Another machine”, and paste it here. Its runs show up beside this one's in the gallery.</>}
    >
      <form
        className="connect-form"
        onSubmit={(e) => {
          e.preventDefault();
          void add();
        }}
      >
        <input className="grow" value={link} onChange={(e) => setLink(e.target.value)} placeholder="http://100.x.y.z:8737/pair#code=…" aria-label="Pairing link from another machine" data-testid="machine-link" spellCheck={false} />
        <button type="submit" className="small" disabled={busy || !link.trim()} data-testid="machine-add">
          {busy ? "Adding…" : "Add machine"}
        </button>
      </form>
      <Problem error={error} />
      {p.daemons.length > 0 && (
        <ul className="connect-devices">
          {p.daemons.map((d) => (
            <MachineRow key={d.url} daemon={d} />
          ))}
        </ul>
      )}
    </Card>
  );
}

type Client = "claude" | "codex" | "json" | "curl";

function snippet(client: Client, url: string, token: string): string {
  switch (client) {
    case "claude":
      return `claude mcp add --transport http taskmaster ${url} --header "Authorization: Bearer ${token}"`;
    case "codex":
      return `# ~/.codex/config.toml\n[mcp_servers.taskmaster]\nurl = "${url}"\nbearer_token_env_var = "TASKMASTER_TOKEN"\n\n# then, in the shell that runs codex:\nexport TASKMASTER_TOKEN=${token}`;
    case "json":
      return JSON.stringify({ mcpServers: { taskmaster: { url, headers: { Authorization: `Bearer ${token}` } } } }, null, 2);
    case "curl":
      return `curl -s ${url} -H "Authorization: Bearer ${token}" -H "content-type: application/json" -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'`;
  }
}

const CLIENTS: { id: Client; label: string }[] = [
  { id: "claude", label: "Claude Code" },
  { id: "codex", label: "Codex" },
  { id: "json", label: "Cursor and others" },
  { id: "curl", label: "Test with curl" },
];

function AgentsCard({ view, devices, onChange }: { view: ConnectView | null; devices: Device[] | null; onChange: () => void }) {
  const [label, setLabel] = useState("Claude Code");
  const [minted, setMinted] = useState<{ token: string; id: string } | null>(null);
  const [client, setClient] = useState<Client>("claude");
  const [route, setRoute] = useState(0);
  const [reveal, setReveal] = useState(false);
  const [busy, setBusy] = useState(false);
  const [test, setTest] = useState<string | null>(null);
  const [error, setError] = useState<unknown>(null);
  const routes = view?.routes ?? [];
  const base = routes[route]?.url ?? (view ? view.mcp_url.replace(/mcp$/, "") : "");
  const url = `${base}mcp`;
  const agents = (devices ?? []).filter((d) => d.kind === "agent");
  const token = minted?.token ?? "<token>";
  const shown = useMemo(() => snippet(client, url, token), [client, url, token]);
  const masked = minted && !reveal ? shown.split(minted.token).join(`${minted.token.slice(0, 6)}…`) : shown;
  const skillCommand = `mkdir -p ~/.claude/skills/taskmaster && curl -fsS ${base}skill.md -o ~/.claude/skills/taskmaster/SKILL.md`;

  const mint = async () => {
    setBusy(true);
    setError(null);
    setTest(null);
    try {
      const made = await api.mintAgent(label.trim() || "Outside agent");
      setMinted({ token: made.token, id: made.id });
      onChange();
    } catch (err) {
      setError(err);
    } finally {
      setBusy(false);
    }
  };
  const check = async () => {
    if (!minted) return;
    setTest("Checking…");
    try {
      const response = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json", authorization: `Bearer ${minted.token}` },
        body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list" }),
      });
      const body = (await response.json().catch(() => null)) as { result?: { tools?: unknown[] } } | null;
      setTest(response.ok && body?.result?.tools ? `It works: ${body.result.tools.length} tools.` : `It answered ${response.status}.`);
    } catch {
      setTest(`Cannot reach ${url} from this browser.`);
    }
  };

  return (
    <Card
      id="agents"
      icon={<Plug size={15} aria-hidden />}
      title="Outside agents (MCP)"
      note="Let Claude Code, Codex, Cursor or any MCP client start runs here, wait for them, read what they did and steer them. Each agent gets its own token; revoke it to cut it off."
    >
      <div className="connect-form">
        <label className="connect-field grow">
          <span>Agent</span>
          <input value={label} onChange={(e) => setLabel(e.target.value)} maxLength={60} data-testid="agent-label" />
        </label>
        {routes.length > 1 && (
          <label className="connect-field">
            <span>It runs on</span>
            <select value={route} onChange={(e) => setRoute(Number(e.target.value))} aria-label="Where the agent runs">
              {routes.map((r, i) => (
                <option key={r.url} value={i}>
                  {r.kind === "this-computer" ? "This computer" : `Another machine (${ROUTE_NAME[r.kind].toLowerCase()})`}
                </option>
              ))}
            </select>
          </label>
        )}
        <button type="button" className="primary small" disabled={busy} onClick={() => void mint()} data-testid="agent-make">
          {busy ? "Making…" : minted ? "Make another token" : "Make a token"}
        </button>
      </div>
      <Problem error={error} />
      <div className="connect-mcp">
        <div className="tabs connect-clients" role="tablist" aria-label="Agent">
          {CLIENTS.map((c) => (
            <button key={c.id} type="button" role="tab" aria-selected={client === c.id} className={client === c.id ? "on" : ""} onClick={() => setClient(c.id)}>
              {c.label}
            </button>
          ))}
        </div>
        <pre className="connect-snippet mono" data-testid="agent-snippet">
          {masked}
        </pre>
        <div className="connect-cmd-row">
          {minted ? (
            <>
              <CopyButton text={shown} what="Setup" testId="agent-copy" />
              <button type="button" className="ghost small" onClick={() => setReveal((v) => !v)}>
                {reveal ? "Hide token" : "Show token"}
              </button>
              <button type="button" className="ghost small" onClick={() => void check()} data-testid="agent-test">
                Test it
              </button>
              {test && (
                <span className="small" data-testid="agent-test-result">
                  {test}
                </span>
              )}
            </>
          ) : (
            <span className="muted small">Make a token to fill this in. It is shown once.</span>
          )}
        </div>
        {minted && <p className="small connect-warn">Copy it now: the token is not shown again. Anyone with it can start runs here.</p>}
        <div className="connect-skill" data-testid="agent-skill">
          <div className="connect-subhead">Teach it the tools</div>
          <p className="muted small">
            A skill tells the agent when to use Taskmaster and to stop if its token is missing. Claude Code reads skills from{" "}
            <code>~/.claude/skills</code>; other agents take the{" "}
            <a href={`${base}skill.md`} target="_blank" rel="noreferrer">
              same file
            </a>{" "}
            wherever they read instructions.
          </p>
          <pre className="connect-snippet mono">{skillCommand}</pre>
          <div className="connect-cmd-row">
            <CopyButton text={skillCommand} what="Skill install" testId="agent-skill-copy" />
          </div>
        </div>
        {routes[route]?.kind === "this-computer" && routes.length === 1 && (
          <p className="muted small">The address is this computer's, so the agent must run here. For an agent on another machine, start the daemon with --lan or --tailscale.</p>
        )}
      </div>
      {agents.length > 0 && (
        <>
          <div className="connect-subhead">Connected agents</div>
          <ul className="connect-devices">
            {agents.map((d) => (
              <DeviceRow key={d.id} device={d} onChange={onChange} />
            ))}
          </ul>
        </>
      )}
    </Card>
  );
}

export function Connect() {
  const view = useLoad(api.connect, []);
  const devices = useLoad(api.devices, [], 5000);
  // `/connect#agents` (from the palette or a link) opens at that card.
  useEffect(() => {
    const id = window.location.hash.slice(1);
    if (id) window.setTimeout(() => document.getElementById(id)?.scrollIntoView({ block: "start" }), 50);
  }, [view.data]);
  const reload = () => {
    void devices.reload();
  };
  return (
    <div className="page connect-page">
      <header className="page-head">
        <div className="grow">
          <h1>Connect</h1>
          <p className="page-lede">Pair a phone or another computer with a link, add your other machines, and let outside agents drive Taskmaster.</p>
        </div>
      </header>
      <Problem error={view.error} onRetry={() => void view.reload()} />
      <Reach view={view.data} />
      <div className="connect-grid">
        <PairCard view={view.data} devices={devices.data} onChange={reload} />
        <DevicesCard devices={devices.data} error={devices.error} onChange={reload} />
        <AgentsCard view={view.data} devices={devices.data} onChange={reload} />
        <MachinesCard />
      </div>
    </div>
  );
}
