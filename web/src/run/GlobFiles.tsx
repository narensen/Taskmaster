// The files a unit owns, one chip each. A planner writes them as one brace glob
// (`{index.html,styles.css,tests/**}`); shown as that string it reads like a single stream.

import { memo, useMemo } from "react";
import { Check, FileCode2, FolderTree } from "lucide-react";
import { emit } from "../overlay";
import { expandGlob, globMatch } from "./model";

function isPattern(entry: string): boolean {
  return /[*?[\]]/.test(entry);
}

function label(entry: string): string {
  if (entry === "**" || entry === "**/*") return "every file";
  return entry;
}

export const GlobFiles = memo(function GlobFiles({
  glob,
  max = 0,
  edited = [],
  interactive = true,
  className = "",
}: {
  glob: string;
  /** Show at most this many, then `+N`. 0 shows all. */
  max?: number;
  /** Files the unit's agent wrote, so their chips are marked. */
  edited?: readonly string[];
  /** Chips open the file. Off inside another button. */
  interactive?: boolean;
  className?: string;
}) {
  const entries = useMemo(() => expandGlob(glob), [glob]);
  if (!entries.length) return null;
  const shown = max > 0 ? entries.slice(0, max) : entries;
  const rest = entries.length - shown.length;
  const touched = (entry: string) =>
    edited.some((file) => file === entry || file.endsWith(`/${entry}`) || (isPattern(entry) && globMatch(entry, file)));
  return (
    <span className={`glob-files ${className}`} data-testid="glob-files" title={entries.join("\n")}>
      {shown.map((entry) => {
        const pattern = isPattern(entry);
        const done = touched(entry);
        const body = (
          <>
            {done ? <Check size={10} strokeWidth={2.4} aria-hidden /> : pattern ? <FolderTree size={10} aria-hidden /> : <FileCode2 size={10} aria-hidden />}
            <span className="clip">{label(entry)}</span>
          </>
        );
        const cls = `glob-file mono ${pattern ? "pattern" : ""} ${done ? "edited" : ""}`;
        return interactive && !pattern ? (
          <button key={entry} type="button" className={cls} onClick={() => emit("preview", { path: entry })} title={`Open ${entry}`}>
            {body}
          </button>
        ) : (
          <span key={entry} className={cls}>
            {body}
          </span>
        );
      })}
      {rest > 0 && <span className="glob-file more mono">+{rest}</span>}
    </span>
  );
});
