import { api, server, setToken } from "../api";
import { Badge, Code, Problem, Spinner, useLoad } from "../ui";

export function Settings() {
  const health = useLoad(api.health, []);
  const providers = useLoad(api.providers, []);

  return (
    <div className="page narrow">
      <header className="page-head">
        <h1>Settings</h1>
      </header>

      <section className="card stack">
        <div className="row between gap">
          <div>
            <strong>Daemon</strong>
            <p className="muted small">
              {server() || window.location.origin}
              {health.data && ` · v${health.data.version} · sign in by ${health.data.auth.methods.join(" or ")}`}
            </p>
          </div>
          <button className="ghost danger" onClick={() => setToken(null)}>
            Sign out
          </button>
        </div>
        <p className="muted small">
          Storage, accounts and the shared Commons are set in <span className="mono">.taskmaster/config.json</span> in the project
          (see <span className="mono">config.example.json</span>). Restart the daemon after changing it.
        </p>
      </section>

      <section>
        <h2 className="section-title">Harnesses</h2>
        <Problem error={providers.error} onRetry={providers.reload} />
        {providers.loading && <Spinner label="Asking each harness for its models" />}
        <div className="list">
          {(providers.data ?? []).map((provider) => (
            <details key={provider.id} className="item">
              <summary>
                <div className="row between gap">
                  <span>
                    <strong>{provider.name}</strong> <span className="mono muted">{provider.id}</span>
                  </span>
                  <Badge tone={provider.installed ? "good" : "quiet"}>{provider.installed ? "installed" : "not installed"}</Badge>
                </div>
              </summary>
              <div className="stack small">
                {!provider.installed && (
                  <>
                    <span className="muted">Install</span>
                    <Code>{provider.install}</Code>
                  </>
                )}
                <span className="muted">Sign in</span>
                <Code>{provider.login}</Code>
                {provider.error && <p className="warning">{provider.error}</p>}
                {provider.models.length > 0 && (
                  <>
                    <span className="muted">Models</span>
                    <div className="row wrap gap">
                      {provider.models.map((model) => (
                        <span key={model.id} className={`chip ${model.default ? "on" : ""}`} title={model.id}>
                          {model.name}
                        </span>
                      ))}
                    </div>
                  </>
                )}
              </div>
            </details>
          ))}
        </div>
      </section>
    </div>
  );
}
