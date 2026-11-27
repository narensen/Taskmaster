// Source control: sign in to a git host, then create a repository or set a remote.
// A token is checked before it is kept, and this page never sees it again.

import { useState } from "react";
import { api, type GitState, type HostConnection } from "../api";
import { Problem, useAction, useLoad } from "../ui";
import { toast } from "../overlay";
import { SelectMenu } from "../components/SelectMenu";

const HINT: Record<string, string> = {
  github: "Personal access token with repo scope, or sign in with gh. For GitHub Enterprise, set its address.",
  gitlab: "Personal access token with api and write_repository. For a self-managed GitLab, set its address.",
  gitea: "A token from the site, and its address. Forgejo uses this too.",
  bitbucket: "Your username and an app password with repository write.",
};

function viaLabel(host: HostConnection): string {
  if (host.via === "cli" && host.kind === "github") return "signed in with gh";
  if (host.via === "cli" && host.kind === "gitlab") return "signed in with glab";
  if (host.via === "token") return "token on this machine";
  return "";
}

export function Hosts() {
  const hosts = useLoad(api.hosts, []);
  const git = useLoad(api.git, []);
  const action = useAction();
  const [draft, setDraft] = useState<Record<string, { token: string; base: string; user: string }>>({});
  const [repoName, setRepoName] = useState("");
  const [repoPrivate, setRepoPrivate] = useState(true);
  const [repoHost, setRepoHost] = useState("");
  const [remoteName, setRemoteName] = useState("origin");
  const [remoteUrl, setRemoteUrl] = useState("");

  const list = hosts.data ?? [];
  const connected = list.filter((host) => host.connected);
  const chosen = connected.find((host) => host.id === repoHost) ?? connected[0];
  const field = (id: string) => draft[id] ?? { token: "", base: "", user: "" };
  const setField = (id: string, patch: Partial<{ token: string; base: string; user: string }>) =>
    setDraft((prev) => {
      const current = prev[id] ?? { token: "", base: "", user: "" };
      return { ...prev, [id]: { ...current, ...patch } };
    });

  const connect = (host: HostConnection) => {
    const values = field(host.id);
    void action.run(`connect:${host.id}`, async () => {
      if (!values.token.trim()) throw new Error("Paste a token first.");
      await api.connectHost({
        kind: host.kind,
        token: values.token.trim(),
        base: values.base.trim() || undefined,
        user: values.user.trim() || undefined,
      });
      setField(host.id, { token: "" });
      await hosts.reload();
      toast(`${host.name} connected`);
    });
  };

  const forget = (host: HostConnection) => {
    void action.run(`forget:${host.id}`, async () => {
      await api.forgetHost(host.id);
      await hosts.reload();
      toast(`${host.name} token removed`);
    });
  };

  const createRepo = () => {
    if (!chosen) return;
    void action.run("create", async () => {
      const name = repoName.trim();
      if (!name) throw new Error("Name the repository first.");
      const made = await api.createHostRepo(chosen.id, { name, private: repoPrivate, push: true });
      setRepoName("");
      await git.reload();
      toast(made.pushed ? `Pushed ${name}` : `Created ${name}`, made.html_url || made.clone_url || "");
    });
  };

  const saveRemote = () => {
    void action.run("remote", async () => {
      await api.setRemote({ name: remoteName.trim() || "origin", url: remoteUrl.trim() });
      setRemoteUrl("");
      await git.reload();
      toast("Remote saved");
    });
  };

  const push = () => {
    void action.run("push", async () => {
      const made = await api.pushGit({ remote: remoteName.trim() || "origin" });
      await git.reload();
      toast(`Pushed ${made.branch}`, made.remote);
    });
  };

  return (
    <div data-testid="hosts">
      <GitLine state={git.data} />
      {hosts.loading && !hosts.data && <p className="muted">Asking the hosts…</p>}
      <Problem error={hosts.error} />
      {list.map((host) => (
        <div className="host-block" key={host.id} data-testid={host.kind === "github" ? "gh" : `host-${host.kind}`}>
          <div className="row between gap wrap">
            <div>
              <span className="strong">{host.name}</span>
              {host.account && <span className="muted"> · {host.account}</span>}
              {host.connected && viaLabel(host) && <span className="muted"> · {viaLabel(host)}</span>}
            </div>
            {host.via === "token" && (
              <button type="button" className="ghost small" onClick={() => forget(host)} disabled={action.busy !== null}>
                Forget token
              </button>
            )}
          </div>
          {host.via === "cli" && <p className="hint">This account comes from the CLI. Signing out of it on this machine disconnects the host.</p>}
          {host.error && <p className="error-line small">{host.error}</p>}
          {!host.connected && (
            <form
              className="host-fields"
              onSubmit={(e) => {
                e.preventDefault();
                connect(host);
              }}
            >
              {host.kind !== "bitbucket" && (
                <input
                  aria-label={`${host.name} address`}
                  placeholder={host.base}
                  value={field(host.id).base}
                  spellCheck={false}
                  autoComplete="off"
                  onChange={(e) => setField(host.id, { base: e.target.value })}
                />
              )}
              {host.kind === "bitbucket" && (
                <input
                  aria-label="Bitbucket username"
                  placeholder="username"
                  value={field(host.id).user}
                  spellCheck={false}
                  autoComplete="username"
                  onChange={(e) => setField(host.id, { user: e.target.value })}
                />
              )}
              <input
                type="password"
                aria-label={`${host.name} token`}
                placeholder="token"
                value={field(host.id).token}
                autoComplete="off"
                onChange={(e) => setField(host.id, { token: e.target.value })}
              />
              <button type="submit" className="small" disabled={action.busy !== null}>
                Connect
              </button>
            </form>
          )}
          {!host.connected && <p className="hint">{HINT[host.kind]}</p>}
        </div>
      ))}

      {git.data?.repo && (
        <div className="host-block">
          <p className="strong">This project</p>
          {connected.length > 0 && (
            <form
              className="host-fields"
              onSubmit={(e) => {
                e.preventDefault();
                createRepo();
              }}
            >
              {connected.length > 1 && (
                <SelectMenu
                  ariaLabel="Host for the new repository"
                  value={chosen?.id ?? ""}
                  options={connected.map((host) => ({
                    value: host.id,
                    label: host.name,
                    note: host.account ?? undefined,
                  }))}
                  onChange={setRepoHost}
                />
              )}
              <input
                aria-label="Repository name"
                placeholder="repository name"
                value={repoName}
                spellCheck={false}
                autoComplete="off"
                onChange={(e) => setRepoName(e.target.value)}
              />
              <label className="check">
                <input type="checkbox" checked={repoPrivate} onChange={(e) => setRepoPrivate(e.target.checked)} />
                Private
              </label>
              <button type="submit" className="small" disabled={action.busy !== null}>
                Create and push
              </button>
            </form>
          )}
          <form
            className="host-fields"
            onSubmit={(e) => {
              e.preventDefault();
              saveRemote();
            }}
          >
            <input aria-label="Remote name" value={remoteName} spellCheck={false} onChange={(e) => setRemoteName(e.target.value)} />
            <input
              className="grow"
              aria-label="Remote address"
              placeholder="https://… or git@…"
              value={remoteUrl}
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => setRemoteUrl(e.target.value)}
            />
            <button type="submit" className="small" disabled={action.busy !== null}>
              Set remote
            </button>
            <button type="button" className="ghost small" onClick={push} disabled={action.busy !== null}>
              Push
            </button>
          </form>
          <p className="hint">Or set origin to any git address. A saved token is used for that host. Otherwise git uses the sign-in already on this machine.</p>
          {git.data.remotes.length > 0 && (
            <ul className="host-remotes">
              {git.data.remotes.map((remote) => (
                <li key={remote.name}>
                  <span className="mono">{remote.name}</span> <span className="muted">{remote.url}</span>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
      <Problem error={action.error} />
    </div>
  );
}

function GitLine({ state }: { state: GitState | null }) {
  if (!state) return <p className="muted">Reading the repository…</p>;
  if (!state.repo) return <p className="muted">This folder is not a git repository.</p>;
  const where = state.detached || !state.branch ? "detached" : state.branch;
  return (
    <p>
      <span className="mono">{where}</span>
      {state.dirty && <span className="muted"> · dirty</span>}
    </p>
  );
}
