import { useEffect, useState } from "react";
import { api, server, setServer, setToken, signInUrl, type Health } from "../api";
import { Field, Problem, Spinner, useAction } from "../ui";

export function SignIn() {
  const [health, setHealth] = useState<Health | null>(null);
  const [unreachable, setUnreachable] = useState<unknown>(null);
  const [code, setCode] = useState("");
  const [address, setAddress] = useState(server());
  const [editing, setEditing] = useState(false);
  const [pasted, setPasted] = useState("");
  const action = useAction();

  const probe = async () => {
    setUnreachable(null);
    setHealth(null);
    try {
      setHealth(await api.health());
    } catch (err) {
      setUnreachable(err);
      setEditing(true);
    }
  };

  useEffect(() => {
    void probe();
  }, []);

  const pair = (event: React.FormEvent) => {
    event.preventDefault();
    void action.run("pair", async () => {
      const { token } = await api.pair(code.trim().toUpperCase());
      setToken(token);
    });
  };

  const methods = health?.auth.methods ?? [];
  const account = methods.includes("account") && health?.auth.sign_in;

  return (
    <main className="signin">
      <div className="signin-card">
        <img className="mark" src="/icon.png" alt="" width={96} height={96} />
        <h1>Taskmaster</h1>
        <p className="muted">One prompt, a working prototype. Sign in to the daemon running your project.</p>

        {!health && !unreachable && <Spinner label="Finding the daemon" />}
        <Problem error={unreachable} onRetry={probe} />

        {health && methods.includes("pairing") && (
          <form onSubmit={pair} className="stack">
            <Field label="Pairing code" hint="The daemon printed it when it started: tm-daemon serve <project>">
              <input
                className="code-input"
                value={code}
                onChange={(e) => setCode(e.target.value)}
                autoComplete="one-time-code"
                autoCapitalize="characters"
                spellCheck={false}
                maxLength={8}
                placeholder="XXXXXXXX"
                autoFocus
              />
            </Field>
            <button className="primary" disabled={code.trim().length < 8 || action.busy !== null}>
              {action.busy ? "Pairing…" : "Pair this device"}
            </button>
          </form>
        )}

        {account && (
          <div className="stack">
            {methods.includes("pairing") && <div className="or">or</div>}
            <a className="button" href={signInUrl(account)}>
              Sign in with your account
            </a>
            <details>
              <summary>I have a token</summary>
              <div className="stack">
                <textarea rows={3} value={pasted} onChange={(e) => setPasted(e.target.value)} spellCheck={false} />
                <button className="ghost" disabled={!pasted.trim()} onClick={() => setToken(pasted.trim())}>
                  Use this token
                </button>
              </div>
            </details>
          </div>
        )}

        <Problem error={action.error} />

        <div className="server-line">
          {editing ? (
            <form
              className="row"
              onSubmit={(e) => {
                e.preventDefault();
                setServer(address);
                setEditing(false);
                void probe();
              }}
            >
              <input
                value={address}
                onChange={(e) => setAddress(e.target.value)}
                placeholder="http://100.x.y.z:8737 (empty: this page's own server)"
                inputMode="url"
                spellCheck={false}
              />
              <button className="ghost small">Connect</button>
            </form>
          ) : (
            <button className="link" onClick={() => setEditing(true)}>
              {server() ? `Daemon: ${server()}` : "Daemon: this server"}
              {health && ` · v${health.version}`} · change
            </button>
          )}
        </div>
      </div>
    </main>
  );
}
