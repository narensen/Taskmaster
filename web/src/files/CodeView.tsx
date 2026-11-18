// A read-only file: only the lines on screen are drawn, coloured by Shiki once its
// grammar has loaded (plain text first, so nothing waits). Matches of the find bar are
// marked; the current one and a jumped-to line are kept in view.

import { useVirtualizer } from "@tanstack/react-virtual";
import { memo, useEffect, useMemo, useRef, useState } from "react";
import { tokenize, type ThemedToken } from "./highlight";

export interface Match {
  line: number;
  start: number;
  end: number;
}

export function findMatches(lines: string[], query: string): Match[] {
  const q = query.toLowerCase();
  if (!q) return [];
  const out: Match[] = [];
  lines.forEach((text, line) => {
    const lower = text.toLowerCase();
    let at = lower.indexOf(q);
    while (at >= 0 && out.length < 5000) {
      out.push({ line, start: at, end: at + q.length });
      at = lower.indexOf(q, at + Math.max(1, q.length));
    }
  });
  return out;
}

function styleOf(t: ThemedToken): React.CSSProperties | undefined {
  if (!t.color && !t.fontStyle) return undefined;
  return {
    color: t.color,
    fontStyle: t.fontStyle && t.fontStyle & 1 ? "italic" : undefined,
    fontWeight: t.fontStyle && t.fontStyle & 2 ? 600 : undefined,
  };
}

/** One line: its tokens, cut where a find match starts and ends. */
const Line = memo(function Line({ text, tokens, marks, current }: { text: string; tokens: ThemedToken[] | undefined; marks: Match[]; current: number }) {
  const parts = tokens ?? [{ content: text, offset: 0 } as ThemedToken];
  if (!marks.length) {
    return (
      <>
        {parts.map((t, i) => (
          <span key={i} style={styleOf(t)}>
            {t.content}
          </span>
        ))}
        {"\n"}
      </>
    );
  }
  const out: React.ReactNode[] = [];
  let pos = 0;
  parts.forEach((t, i) => {
    const start = pos;
    const end = pos + t.content.length;
    let cut = start;
    for (const [mi, m] of marks.entries()) {
      if (m.end <= start || m.start >= end) continue;
      const a = Math.max(m.start, start);
      const b = Math.min(m.end, end);
      if (a > cut) out.push(<span key={`${i}-${cut}`} style={styleOf(t)}>{t.content.slice(cut - start, a - start)}</span>);
      out.push(
        <mark key={`${i}-m${a}`} className={mi === current ? "find-hit cur" : "find-hit"} style={styleOf(t)}>
          {t.content.slice(a - start, b - start)}
        </mark>,
      );
      cut = b;
    }
    if (cut < end) out.push(<span key={`${i}-${cut}`} style={styleOf(t)}>{t.content.slice(cut - start)}</span>);
    pos = end;
  });
  return (
    <>
      {out}
      {"\n"}
    </>
  );
});

export function CodeView({
  path,
  content,
  line,
  query,
  current,
}: {
  path: string;
  content: string;
  /** A line to show and mark, counted from 1. */
  line: { n: number; at: number } | null;
  query: string;
  /** Which match of `query` is the current one. */
  current: number;
}) {
  const lines = useMemo(() => content.split("\n"), [content]);
  const [tokens, setTokens] = useState<ThemedToken[][] | null>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const matches = useMemo(() => findMatches(lines, query), [lines, query]);
  const byLine = useMemo(() => {
    const map = new Map<number, { list: Match[]; first: number }>();
    matches.forEach((m, i) => {
      const entry = map.get(m.line);
      if (entry) entry.list.push(m);
      else map.set(m.line, { list: [m], first: i });
    });
    return map;
  }, [matches]);

  useEffect(() => {
    let stale = false;
    setTokens(null);
    void tokenize(content, path)
      .then((t) => !stale && setTokens(t))
      .catch(() => undefined);
    return () => {
      stale = true;
    };
  }, [content, path]);

  const virtual = useVirtualizer({ count: lines.length, getScrollElement: () => scroller.current, estimateSize: () => 20, overscan: 24 });

  useEffect(() => {
    if (line && line.n >= 1) virtual.scrollToIndex(Math.min(lines.length - 1, line.n - 1), { align: "center" });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [line?.at, line?.n]);
  useEffect(() => {
    const m = matches[current];
    if (m) virtual.scrollToIndex(m.line, { align: "center" });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current, matches]);

  const gutter = String(lines.length).length;

  return (
    <div ref={scroller} className="codeview" data-testid="preview" style={{ "--gutter": `${gutter + 1}ch` } as React.CSSProperties}>
      <div className="codeview-inner" style={{ height: virtual.getTotalSize() }}>
        {virtual.getVirtualItems().map((item) => {
          const i = item.index;
          const hit = byLine.get(i);
          return (
            <div key={i} className={`cv-row ${line?.n === i + 1 ? "hl" : ""}`} style={{ transform: `translateY(${item.start}px)` }}>
              <span className="cv-num" aria-hidden>
                {i + 1}
              </span>
              <span className="cv-code">
                <Line text={lines[i]!} tokens={tokens?.[i]} marks={hit?.list ?? []} current={hit ? current - hit.first : -1} />
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
