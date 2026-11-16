// The locked plan: pages, routes, owned globs, citations and the estimate. When it arrives
// while the page is open it locks into place.

import { useLayoutEffect, useRef } from "react";
import gsap from "gsap";
import type { Plan } from "../api";
import { KnownPath } from "../paths";
import { motionOn } from "../prefs";
import { tokens, usd } from "../format";

export function PlanView({ plan, arrived, compact }: { plan: Plan; arrived?: boolean; compact?: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    if (!arrived || !ref.current || !motionOn()) return;
    const tl = gsap.timeline();
    tl.from(ref.current, { opacity: 0, y: 10, duration: 0.3, ease: "power2.out" })
      .from(ref.current.querySelectorAll("tbody tr"), { opacity: 0, x: -8, stagger: 0.05, duration: 0.2 }, "-=0.1")
      .fromTo(ref.current.querySelector(".lock-rule"), { scaleX: 0 }, { scaleX: 1, transformOrigin: "left", duration: 0.4, ease: "power3.inOut" });
    return () => {
      tl.kill();
    };
  }, [arrived]);
  return (
    <div className={`plan ${compact ? "compact" : ""}`} ref={ref} data-testid="locked-plan">
      <div className="plan-head">
        <span className="strong">Locked plan</span>
        <span className="muted small">
          {plan.pages.length} page{plan.pages.length === 1 ? "" : "s"} · about {tokens(plan.estimate.tokens)} tokens · {usd(plan.estimate.usd)}
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
                <td className="strong">{page.id}</td>
                <td>{page.route}</td>
                <td>{page.owned_glob}</td>
                <td>{page.cites.length ? page.cites.join(", ") : "none"}</td>
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
