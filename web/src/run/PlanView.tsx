// The locked plan: pages, routes, owned globs, citations and the estimate. When it arrives
// while the page is open it locks into place.

import { useLayoutEffect, useRef } from "react";
import gsap from "gsap";
import { planAgents, type Plan } from "../api";
import { KnownPath } from "../paths";
import { motionOn } from "../prefs";
import { dur, ease } from "../motion";
import { tokens, usd } from "../format";

export function PlanView({ plan, arrived, compact, unpriced }: { plan: Plan; arrived?: boolean; compact?: boolean; unpriced?: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  const agents = planAgents(plan);
  useLayoutEffect(() => {
    if (!arrived || !ref.current || !motionOn()) return;
    const tl = gsap.timeline();
    tl.from(ref.current, { opacity: 0, y: 8, duration: dur.base, ease: ease.out })
      .from(ref.current.querySelectorAll("tbody tr"), { opacity: 0, x: -6, stagger: 0.03, duration: dur.fast, ease: ease.out }, "-=0.08")
      .fromTo(ref.current.querySelector(".lock-rule"), { scaleX: 0 }, { scaleX: 1, transformOrigin: "left", duration: dur.slow, ease: ease.out }, "-=0.08")
      .from(ref.current.querySelectorAll("p.small, .file-tag"), { opacity: 0, y: 4, stagger: 0.02, duration: dur.fast, ease: ease.out }, "-=0.1");
    return () => {
      tl.kill();
    };
  }, [arrived]);
  return (
    <div className={`plan ${compact ? "compact" : ""}`} ref={ref} data-testid="locked-plan" data-arrived={arrived ? "true" : undefined}>
      <div className="plan-head">
        <span className="strong">Locked plan</span>
        <span className="muted small">
          {plan.pages.length} page{plan.pages.length === 1 ? "" : "s"} · {agents.length} agent{agents.length === 1 ? "" : "s"} · about {tokens(plan.estimate.tokens)} tokens{unpriced ? "" : ` · ${usd(plan.estimate.usd)}`}
        </span>
      </div>
      <div className="lock-rule" />
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>page</th>
              <th>route</th>
              <th>owns</th>
              <th>cites</th>
            </tr>
          </thead>
          <tbody>
            {plan.pages.map((page) => (
              <tr key={page.id}>
                <td className="strong">
                  {page.id}
                  {(page.units?.length ?? 0) > 1 && <span className="muted"> ({page.units!.length} workers)</span>}
                </td>
                <td>{page.route}</td>
                <td>{page.owned_glob}</td>
                <td>{page.cites.length ? page.cites.join(", ") : "none"}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <div className="table-wrap" data-testid="plan-agents">
        <table className="table">
          <thead>
            <tr>
              <th>agent</th>
              <th>owns</th>
              <th>task</th>
              <th>done when</th>
            </tr>
          </thead>
          <tbody>
            {agents.map((agent) => (
              <tr key={agent.id}>
                <td className="strong">{agent.id}</td>
                <td>{agent.owned_glob}</td>
                <td>{compact ? agent.task.split("\n")[0] : agent.task}</td>
                <td>{agent.done}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {!compact &&
        plan.pages.map((page) => (
          <p key={page.id} className="small">
            <span className="strong">{page.id}</span> {page.brief}
          </p>
        ))}
      {plan.contracts.length > 0 && (
        <div className="small">
          <span className="muted">contracts </span>
          {plan.contracts.map((c) => (
            <span key={c.path} className="file-tag">
              <KnownPath path={c.path} />
            </span>
          ))}
        </div>
      )}
    </div>
  );
}
