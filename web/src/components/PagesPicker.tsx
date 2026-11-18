// Pages at once: one click picks how many pages build side by side. Max is what this
// machine can run; workers always run here.

import { motion } from "motion/react";
import { useId } from "react";
import { bezier, dur } from "../motion";
import { rove } from "./roving";

export const STEPS = [1, 2, 3, 4, 6, 8] as const;
export type Pages = number | "max";

export function resolvePages(value: Pages, max: number): number {
  return value === "max" ? Math.max(1, max) : Math.min(value, Math.max(1, max));
}

export function PagesPicker({ value, onChange, max }: { value: Pages; onChange: (v: Pages) => void; max: number | null }) {
  const id = useId();
  const options: { v: Pages; label: string; off: boolean }[] = [
    ...STEPS.map((n) => ({ v: n as Pages, label: String(n), off: max !== null && n > max })),
    { v: "max", label: "Max", off: false },
  ];
  return (
    <div className="pages-picker">
      <span className="pages-label" id={`${id}-l`}>
        Pages at once
      </span>
      <div className="seg" role="radiogroup" aria-labelledby={`${id}-l`} onKeyDown={(e) => rove(e, "x")}>
        {options.map((o) => {
          const on = value === o.v;
          return (
            <button
              key={String(o.v)}
              type="button"
              role="radio"
              aria-checked={on}
              data-rove
              tabIndex={on ? 0 : -1}
              disabled={o.off}
              title={o.v === "max" ? (max ? `What this machine can run: ${max}` : "What this machine can run") : o.off ? `This machine runs at most ${max}` : undefined}
              className={`seg-item ${on ? "on" : ""}`}
              onClick={() => onChange(o.v)}
              onFocus={() => !on && !o.off && onChange(o.v)}
            >
              {on && <motion.span layoutId={`${id}-thumb`} className="seg-thumb" transition={{ duration: dur.base, ease: bezier.out }} />}
              <span className="seg-text">{o.label}</span>
            </button>
          );
        })}
      </div>
      {value === "max" && max !== null && <span className="pages-note">{max} on this machine</span>}
    </div>
  );
}
