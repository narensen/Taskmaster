// The locked plan: pages, routes, owned globs, citations, the agents it starts with their
// tasks and done criteria, the design spec and the estimate. When it arrives while the page
// is open it locks into place.

import { useLayoutEffect, useRef } from "react";
import gsap from "gsap";
import { CircleCheck, FileCode2, Lock, Palette } from "lucide-react";
import { planAgents, type Plan } from "../api";
import { RoleMark } from "../marks";
import { KnownPath } from "../paths";
import { motionOn } from "../prefs";
import { dur, ease } from "../motion";
import { tokens, usd } from "../format";

export function PlanView({
  plan,
  arrived,
  compact,
  unpriced,
  open,
}: {
  plan: Plan;
  arrived?: boolean;
  compact?: boolean;
  unpriced?: boolean;
  /** Show the agents and design spec unfolded in the compact view. */
  open?: boolean;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const agents = planAgents(plan);
  useLayoutEffect(() => {
    if (!arrived || !ref.current || !motionOn()) return;
    const tl = gsap.timeline();
    tl.from(ref.current, { opacity: 0, y: 8, duration: dur.base, ease: ease.out })
      .from(ref.current.querySelectorAll(".plan-page"), { opacity: 0, x: -6, stagger: 0.03, duration: dur.fast, ease: ease.out }, "-=0.08")
      .fromTo(ref.current.querySelector(".lock-rule"), { scaleX: 0 }, { scaleX: 1, transformOrigin: "left", duration: dur.slow, ease: ease.out }, "-=0.08")
      .from(ref.current.querySelectorAll(".plan-agent, .file-tag"), { opacity: 0, y: 4, stagger: 0.02, duration: dur.fast, ease: ease.out }, "-=0.1");
    return () => {
      tl.kill();
    };
  }, [arrived]);
  const design = plan.design?.trim();
  const agentList = (
    <div className="plan-agents" data-testid="plan-agents">
      {agents.map((agent) => (
        <article key={agent.id} className="plan-agent">
          <div className="plan-agent-head">
            <RoleMark role="worker" size={14} />
            <span className="mono strong clip">{agent.id}</span>
          </div>
          <span className="mono muted clip plan-owns" title={agent.owned_glob}>
            owns {agent.owned_glob}
          </span>
          <p className={`plan-task ${compact ? "clamp" : ""}`}>{agent.task}</p>
          <p className="plan-done">
            <CircleCheck size={13} aria-hidden />
            <span>
              <span className="muted">Done when </span>
              {agent.done}
            </span>
          </p>
        </article>
      ))}
    </div>
  );
  const designBlock = design ? (
    <div className="plan-design" data-testid="plan-design">
      <div className="plan-label">
        <Palette size={13} aria-hidden /> Design spec
      </div>
      <div className="plan-design-text">{design}</div>
    </div>
  ) : null;
  return (
    <div className={`plan ${compact ? "compact" : ""}`} ref={ref} data-testid="locked-plan" data-arrived={arrived ? "true" : undefined}>
      <div className="plan-head">
        <span className="plan-title">
          <Lock size={14} aria-hidden />
          Locked plan
        </span>
        <span className="plan-stats">
          <span>
            <strong>{plan.pages.length}</strong> page{plan.pages.length === 1 ? "" : "s"}
          </span>
          <span>
            <strong>{agents.length}</strong> agent{agents.length === 1 ? "" : "s"}
          </span>
          <span>
            about <strong>{tokens(plan.estimate.tokens)}</strong> tokens{unpriced ? "" : ` · ${usd(plan.estimate.usd)}`}
          </span>
        </span>
      </div>
      <div className="lock-rule" />
      <ul className="plan-pages" aria-label="Pages">
        {plan.pages.map((page) => (
          <li key={page.id} className="plan-page">
            <span className="strong plan-page-id">
              {page.id}
              {(page.units?.length ?? 0) > 1 && <span className="muted"> ({page.units!.length} workers)</span>}
            </span>
            <span className="mono plan-route">{page.route}</span>
            <span className="mono muted clip plan-owns">{page.owned_glob}</span>
            <span className="plan-cites">
              {page.cites.length ? page.cites.map((c) => <span key={c} className="cite-tag">{c}</span>) : <span className="muted">no cites</span>}
              {page.skills?.map((s) => (
                <span key={`skill-${s}`} className="cite-tag skill-tag" title={`Skill: the agent reads ${s}/SKILL.md first`}>
                  ${s}
                </span>
              ))}
            </span>
          </li>
        ))}
      </ul>
      {compact ? (
        <details className="plan-more" open={open}>
          <summary>
            {agents.length} agent{agents.length === 1 ? "" : "s"}, their tasks and done criteria{design ? ", and the design spec" : ""}
          </summary>
          {agentList}
          {designBlock}
        </details>
      ) : (
        <>
          <div className="plan-label">
            <RoleMark role="worker" size={13} /> Agents
          </div>
          {agentList}
          {designBlock}
          <div className="plan-label">
            <FileCode2 size={13} aria-hidden /> Page briefs
          </div>
          {plan.pages.map((page) => (
            <p key={page.id} className="small plan-brief">
              <span className="strong">{page.id}</span> {page.brief}
            </p>
          ))}
        </>
      )}
      {plan.contracts.length > 0 && (
        <div className="small plan-contracts">
          <span className="muted">Contracts</span>
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
