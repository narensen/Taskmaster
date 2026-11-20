// What a run's agents read and wrote, split four ways. Output is the number a harness can
// shrink by asking for less, so it leads; cache reads are cheap and come last.

import type { Spend, Totals } from "../api";
import { seconds, tokens, usd } from "../format";

const PARTS = [
  { key: "output", label: "Output", note: "written by the models" },
  { key: "input", label: "Input", note: "read for the first time" },
  { key: "cache_write", label: "Cache write", note: "stored for later calls" },
  { key: "cache_read", label: "Cache read", note: "served from the cache" },
] as const;

type Part = (typeof PARTS)[number]["key"];

function agentName(key: string): string {
  if (key === "master") return "Master (plan)";
  if (key.startsWith("page:")) return `Page ${key.slice(5)}`;
  if (key.startsWith("unit:")) return `Unit ${key.slice(5)}`;
  const [role, page] = key.split(" page:");
  const name = role![0]!.toUpperCase() + role!.slice(1);
  return page ? `${name} · ${page}` : name;
}

/** The four parts as one bar, in shades of the ink, never in status colours. */
export function SplitBar({ split, label }: { split: Record<Part, number>; label: string }) {
  const total = PARTS.reduce((n, p) => n + split[p.key], 0);
  if (!total) return null;
  return (
    <div className="split-bar" role="img" aria-label={`${label}: ${PARTS.map((p) => `${p.label.toLowerCase()} ${tokens(split[p.key])}`).join(", ")}`}>
      {PARTS.map((p) =>
        split[p.key] ? <span key={p.key} className={`split-seg seg-${p.key}`} style={{ flexGrow: split[p.key] }} title={`${p.label}: ${split[p.key].toLocaleString()}`} /> : null,
      )}
    </div>
  );
}

export function TokenLedger({ totals, open }: { totals: Totals; open: boolean }) {
  if (!totals.calls) return null;
  const agents = Object.entries(totals.by_agent ?? {}).sort((a, b) => b[1].output - a[1].output);
  const priced = totals.unpriced_calls < totals.calls;
  return (
    <details className="token-ledger card" open={open} data-testid="token-ledger">
      <summary>
        <span className="strong">Tokens</span>
        <span className="muted small">
          {tokens(totals.output)} out · {tokens(totals.input)} in · {tokens(totals.cache_write)} cache write · {tokens(totals.cache_read)} cache read
        </span>
      </summary>
      <div className="token-body">
        <SplitBar split={totals} label="Whole run" />
        <dl className="token-totals">
          {PARTS.map((p) => (
            <div key={p.key} className="token-total">
              <dt>
                <span className={`split-key seg-${p.key}`} aria-hidden />
                {p.label}
              </dt>
              <dd className="mono">{totals[p.key].toLocaleString()}</dd>
              <dd className="muted small">{p.note}</dd>
            </div>
          ))}
        </dl>
        <div className="token-table-wrap">
          <table className="token-table">
            <caption className="sr-only">Tokens by agent</caption>
            <thead>
              <tr>
                <th scope="col">Agent</th>
                <th scope="col">Calls</th>
                <th scope="col">Output</th>
                <th scope="col">Input</th>
                <th scope="col">Cache write</th>
                <th scope="col">Cache read</th>
                <th scope="col">Time</th>
                {priced && <th scope="col">Cost</th>}
              </tr>
            </thead>
            <tbody>
              {agents.map(([key, s]: [string, Spend]) => (
                <tr key={key}>
                  <th scope="row">{agentName(key)}</th>
                  <td>{s.calls}</td>
                  <td className="strong">{(s.output ?? 0).toLocaleString()}</td>
                  <td>{(s.input ?? 0).toLocaleString()}</td>
                  <td>{(s.cache_write ?? 0).toLocaleString()}</td>
                  <td>{s.cache_read.toLocaleString()}</td>
                  <td>{seconds(s.seconds ?? 0)}</td>
                  {priced && <td>{s.unpriced_calls === s.calls ? "—" : usd(s.cost_usd)}</td>}
                </tr>
              ))}
            </tbody>
            <tfoot>
              <tr>
                <th scope="row">Run</th>
                <td>{totals.calls}</td>
                <td className="strong">{totals.output.toLocaleString()}</td>
                <td>{totals.input.toLocaleString()}</td>
                <td>{totals.cache_write.toLocaleString()}</td>
                <td>{totals.cache_read.toLocaleString()}</td>
                <td>{seconds(totals.seconds)}</td>
                {priced && <td>{usd(totals.cost_usd)}</td>}
              </tr>
            </tfoot>
          </table>
        </div>
      </div>
    </details>
  );
}
