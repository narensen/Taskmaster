import type { ChangeKind, Phase, RunEvent, PageState } from "./api";

export function usd(value: number): string {
  if (value === 0) return "$0";
  if (value < 0.01) return `$${value.toFixed(4)}`;
  return `$${value.toFixed(2)}`;
}

export function tokens(value: number): string {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(value >= 10_000_000 ? 0 : 1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(value >= 10_000 ? 0 : 1)}k`;
  return String(value);
}

export function seconds(value: number): string {
  if (value < 60) return `${value.toFixed(value < 10 ? 1 : 0)}s`;
  const minutes = Math.floor(value / 60);
  if (minutes < 60) return `${minutes}m ${Math.round(value % 60)}s`;
  return `${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}

/** Compact relative time: 12s, 5m, 3h, 6d. */
export function shortSpan(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.round(m / 60);
  if (h < 48) return `${h}h`;
  return `${Math.round(h / 24)}d`;
}

export function ago(at: string): string {
  const then = Date.parse(at);
  if (Number.isNaN(then)) return at;
  const diff = (Date.now() - then) / 1000;
  if (diff < 45) return "just now";
  if (diff < 3600) return `${Math.round(diff / 60)} min ago`;
  if (diff < 86_400) return `${Math.round(diff / 3600)} h ago`;
  return new Date(then).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export const PHASE: Record<Phase, string> = {
  planning: "Planning",
  awaiting_approval: "Waiting for approval",
  building: "Building",
  reviewing: "Reviewing",
  verifying: "Verifying",
  done: "Done",
  failed: "Failed",
};

/** What a run's badge says. */
export function phaseLabel(run: { phase: Phase; cancelled?: boolean }): string {
  return run.cancelled ? "Cancelled" : PHASE[run.phase];
}

export function phaseTone(phase: Phase): Tone {
  switch (phase) {
    case "done":
      return "good";
    case "failed":
      return "bad";
    case "awaiting_approval":
      return "ask";
    default:
      return "busy";
  }
}

export type Tone = "good" | "bad" | "ask" | "busy" | "quiet";

export function pageTone(state: PageState): Tone {
  switch (state) {
    case "passed":
      return "good";
    case "failed":
      return "bad";
    case "flagged":
      return "ask";
    case "building":
      return "busy";
    default:
      return "quiet";
  }
}

export const CHANGE: Record<ChangeKind, string> = {
  contracts_locked: "Contracts locked",
  shared_repaired: "Shared files repaired",
  page_built: "Page built",
  page_repaired: "Page repaired",
  entry_amended: "Dictionary entry amended",
  review_finding: "Review finding",
  verify_failed: "Verification failed",
};

export interface Described {
  title: string;
  detail?: string;
  tone: Tone;
}

/** One line for an entry in a run's log. */
export function describe(event: RunEvent): Described {
  if (event === "plan_approved") return { title: "Plan approved", tone: "good" };
  if (event === "plan_rejected") return { title: "Plan rejected", tone: "ask" };
  if (event === "cancelled") return { title: "Cancelled", tone: "bad" };
  if ("plan_ready" in event) {
    const plan = event.plan_ready;
    return {
      title: `Plan ready: ${plan.pages.length} page${plan.pages.length === 1 ? "" : "s"}`,
      detail: `${plan.contracts.length} contract file${plan.contracts.length === 1 ? "" : "s"}, about ${tokens(plan.estimate.tokens)} tokens`,
      tone: "ask",
    };
  }
  if ("worker_finished" in event) {
    const { page, outcome } = event.worker_finished;
    if (outcome === "passed") return { title: `${page} passed its checks`, tone: "good" };
    if ("check_failed" in outcome) return { title: `${page} failed its checks`, detail: outcome.check_failed.errors, tone: "bad" };
    return { title: `${page} objected to ${outcome.flagged.key}`, detail: outcome.flagged.issue, tone: "ask" };
  }
  if ("flag_resolved" in event) {
    const { page, key, resolution } = event.flag_resolved;
    return resolution === "amended"
      ? { title: `${key} amended for ${page}`, tone: "busy" }
      : { title: `${page} told to conform to ${key}`, tone: "busy" };
  }
  if ("review_finished" in event) {
    const n = event.review_finished.findings.length;
    return {
      title: n ? `Review found ${n} problem${n === 1 ? "" : "s"}` : "Review found nothing",
      detail: event.review_finished.findings.map((f) => `${f.file}:${f.line} ${f.failure}`).join("\n") || undefined,
      tone: n ? "ask" : "good",
    };
  }
  if ("findings_verified" in event) {
    const n = event.findings_verified.confirmed.length;
    return {
      title: n ? `${n} finding${n === 1 ? "" : "s"} confirmed, sent for repair` : "No finding held up",
      detail: event.findings_verified.confirmed.map((f) => `${f.file}:${f.line} ${f.failure}`).join("\n") || undefined,
      tone: n ? "ask" : "good",
    };
  }
  if ("verify_finished" in event) {
    const failing = Object.entries(event.verify_finished.failing);
    return event.verify_finished.ok
      ? { title: "The whole project checks out", tone: "good" }
      : {
          title: `Verification failed on ${failing.length} page${failing.length === 1 ? "" : "s"}`,
          detail: failing.map(([page, errors]) => `${page}: ${errors}`).join("\n"),
          tone: "bad",
        };
  }
  if ("page_fresh" in event) return { title: `${event.page_fresh.page} unchanged, not rebuilt`, tone: "quiet" };
  return { title: "Unknown event", detail: JSON.stringify(event), tone: "quiet" };
}
