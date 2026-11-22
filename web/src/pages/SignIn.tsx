import { useEffect, useState } from "react";
import { api, deviceLabel, server, setServer, setToken, signInUrl, type Health } from "../api";

/** The one-time code a pairing link carries after `#code=`, if this page was opened from one. */
function linkedCode(): string | null {
  const code = new URLSearchParams(window.location.hash.slice(1)).get("code") ?? new URLSearchParams(window.location.search).get("code");
  return code?.trim() || null;
}
import { Field, Problem, Spinner, useAction } from "../ui";
import { navigate } from "../router";

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
    // Opened from a pairing link: pair at once, with nothing to type, and take the code out
    // of the address so it is not left in history or shared by accident.
    const linked = linkedCode();
    if (!linked) return;
    // The code leaves the address now. The route changes once signed in: by then the
    // router is listening (this effect runs before the router's own on the first render).
    window.history.replaceState(null, "", window.location.pathname + window.location.search);
    setCode(linked);
    void action.run("pair", async () => {
      const me = deviceLabel();
      const { token } = await api.pair(linked.toUpperCase(), me.label, me.kind);
      setToken(token);
      if (window.location.pathname === "/pair") navigate("/", true);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const pair = (event: React.FormEvent) => {
    event.preventDefault();
    void action.run("pair", async () => {
      const me = deviceLabel();
      const { token } = await api.pair(code.trim().toUpperCase(), me.label, me.kind);
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
            <Field label="Pairing code" hint="The daemon printed it, and a link, when it started. Or open a pairing link from Connect on a device that is already signed in.">
              <input
                className="code-input"
                value={code}
                onChange={(e) => setCode(e.target.value)}
                autoComplete="one-time-code"
                autoCapitalize="characters"
                spellCheck={false}
                maxLength={16}
                placeholder="XXXXXXXX"
                autoFocus
              />
            </Field>
            <button className="primary" disabled={code.trim().length < 8 || action.busy !== null} data-testid="pair-submit">
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
