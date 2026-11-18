// Each provider's own mark, drawn in the theme's ink. The SVGs in ./logos are the
// providers' published marks as single-colour paths (see ./logos/README.md); nothing here
// adds a tile or a brand background. A brand colour, where the provider has one, shows
// only while the pointer is over the mark.

import type { CSSProperties } from "react";
import claude from "./logos/claude.svg?raw";
import grok from "./logos/grok.svg?raw";
import codex from "./logos/codex.svg?raw";
import opencode from "./logos/opencode.svg?raw";
import agy from "./logos/agy.svg?raw";

interface Mark {
  svg: string;
  /** Shown on hover only. Omitted where the brand is black and white. */
  brand?: string;
  name: string;
}

function inner(svg: string): string {
  // Keep the paths, drop the <svg> wrapper and the <title>: the caller labels the mark.
  return svg.replace(/^[\s\S]*?<svg[^>]*>/, "").replace(/<\/svg>\s*$/, "").replace(/<title>[\s\S]*?<\/title>/, "");
}

const MARKS: Record<string, Mark> = {
  claude: { svg: inner(claude), brand: "#d97757", name: "Claude Code" },
  grok: { svg: inner(grok), name: "Grok Build" },
  codex: { svg: inner(codex), name: "Codex" },
  opencode: { svg: inner(opencode), name: "OpenCode" },
  agy: { svg: inner(agy), brand: "#3186ff", name: "Antigravity" },
};

export function providerName(id: string, fallback?: string): string {
  return fallback ?? MARKS[id]?.name ?? id;
}

/** A provider's mark at `size` pixels. An unknown id gets a plain ring with its initial. */
export function ProviderLogo({ id, size = 14, title, className = "" }: { id: string | null | undefined; size?: number; title?: string; className?: string }) {
  const mark = id ? MARKS[id] : undefined;
  const style = { width: size, height: size, ...(mark?.brand ? { "--brand": mark.brand } : {}) } as CSSProperties;
  const label = title ? { role: "img", "aria-label": title } : { "aria-hidden": true };
  if (!mark) {
    const letter = (id ?? "").trim().charAt(0).toUpperCase();
    return (
      <span className={`prov-logo prov-unknown ${className}`} style={style} {...label}>
        <svg viewBox="0 0 24 24" width={size} height={size} fill="none" stroke="currentColor" strokeWidth={1.75}>
          <circle cx="12" cy="12" r="9.25" strokeDasharray={id ? undefined : "3 3"} />
          {letter && (
            <text x="12" y="16.2" textAnchor="middle" fontSize="11" fontWeight="600" fill="currentColor" stroke="none" fontFamily="var(--sans)">
              {letter}
            </text>
          )}
        </svg>
      </span>
    );
  }
  return (
    <span className={`prov-logo ${mark.brand ? "has-brand" : ""} ${className}`} style={style} {...label}>
      <svg viewBox="0 0 24 24" width={size} height={size} fill="currentColor" fillRule="evenodd" dangerouslySetInnerHTML={{ __html: mark.svg }} />
    </span>
  );
}
