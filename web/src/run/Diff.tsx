// The diff tab: this attempt, this page, or the whole run against where it started. Find
// in the open diff, mark a file viewed until it changes again, and click a changed line's
// gutter to queue a comment to that page's worker as fix text for its next repair.

import { useEffect, useMemo, useRef, useState } from "react";
import { api, type Attempt, type RunDetail } from "../api";
import { KnownPath, openPathMenu, resolvedPath } from "../paths";
import { toast } from "../overlay";
import { prefs, setPrefs, usePrefs } from "../prefs";
import { globMatch } from "./model";

export type Scope = "attempt" | "page" | "run";

interface Line {
  kind: "add" | "del" | "ctx" | "hunk";
  text: string;
  old?: number;
  new?: number;
}

interface FileDiff {
  path: string;
  lines: Line[];
  hash: string;
  added: number;
  removed: number;
}

function hash(text: string): string {
  let h = 5381;
  for (let i = 0; i < text.length; i++) h = ((h << 5) + h + text.charCodeAt(i)) | 0;
  return (h >>> 0).toString(36);
}

export function parseDiff(text: string): FileDiff[] {
  const files: FileDiff[] = [];
  let cur: FileDiff | null = null;
  let raw: string[] = [];
  let oldN = 0;
  let newN = 0;
  const close = () => {
    if (cur) {
      cur.hash = hash(raw.join("\n"));
      files.push(cur);
    }
  };
  for (const line of text.split("\n")) {
    const head = /^diff --git a\/(.+?) b\/(.+)$/.exec(line);
    if (head) {
      close();
      cur = { path: head[2]!, lines: [], hash: "", added: 0, removed: 0 };
      raw = [];
      continue;
    }
    if (!cur) {
      // An attempt's diff may start at the --- line.
      const plus = /^\+\+\+ b\/(.+)$/.exec(line);
      if (plus) {
        cur = { path: plus[1]!, lines: [], hash: "", added: 0, removed: 0 };
        raw = [];
      }
      continue;
    }
    raw.push(line);
    if (/^(index |--- |\+\+\+ |new file|deleted file|similarity|rename |old mode|new mode)/.test(line)) {
      const plus = /^\+\+\+ b\/(.+)$/.exec(line);
      if (plus && cur.lines.length > 0) {
        close();
        cur = { path: plus[1]!, lines: [], hash: "", added: 0, removed: 0 };
        raw = [];
      }
      continue;
    }
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$/.exec(line);
    if (hunk) {
      oldN = Number(hunk[1]);
      newN = Number(hunk[2]);
      cur.lines.push({ kind: "hunk", text: line });
    } else if (line.startsWith("+")) {
      cur.lines.push({ kind: "add", text: line.slice(1), new: newN++ });
      cur.added++;
    } else if (line.startsWith("-")) {
      cur.lines.push({ kind: "del", text: line.slice(1), old: oldN++ });
      cur.removed++;
    } else if (line.startsWith(" ")) {
      cur.lines.push({ kind: "ctx", text: line.slice(1), old: oldN++, new: newN++ });
    }
  }
  close();
  return files;
}

function Highlighted({ text, find }: { text: string; find: string }) {
  if (!find) return <>{text || " "}</>;
  const parts = text.split(new RegExp(`(${find.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")})`, "gi"));
  return (
    <>
      {parts.map((p, i) => (i % 2 ? <mark key={i}>{p}</mark> : p))}
    </>
  );
}

export function DiffTab({
  run,
  detail,
  request,
}: {
  run: string;
  detail: RunDetail | null;
  request: { scope: Scope; page?: string; attempt?: number; n: number } | null;
}) {
  const pages = detail?.plan?.pages ?? [];
  const [scope, setScope] = useState<Scope>(request?.scope ?? "run");
  const [page, setPage] = useState<string>(request?.page ?? pages[0]?.id ?? "");
  const [attempt, setAttempt] = useState<number>(request?.attempt ?? 0);
  const [attempts, setAttempts] = useState<Attempt[]>([]);
  const [text, setText] = useState<string>("");
  const [error, setError] = useState<string | null>(null);
  const [find, setFind] = useState("");
  const [hit, setHit] = useState(0);
  const [comment, setComment] = useState<{ file: string; line: number; text: string } | null>(null);
  const [queued, setQueued] = useState<{ file: string; line: number; text: string }[]>([]);
  const body = useRef<HTMLDivElement>(null);
  const p = usePrefs();

  useEffect(() => {
    if (!request) return;
    setScope(request.scope);
    if (request.page) setPage(request.page);
    if (request.attempt !== undefined) setAttempt(request.attempt);
  }, [request]);

  useEffect(() => {
    if (!page && pages[0]) setPage(pages[0].id);
  }, [pages, page]);

  useEffect(() => {
    if (!page) return;
    api.attempts(run, page).then(setAttempts).catch(() => setAttempts([]));
  }, [run, page]);

  useEffect(() => {
    setError(null);
    if (scope === "attempt") {
      const a = attempts[attempt];
      setText(a?.diff ?? "");
      if (!a) setError(attempts.length ? "Pick an attempt." : "This page has no stored attempt.");
      else if (!a.diff) setError("This attempt kept no diff.");
      return;
    }
    api
      .diff(run, scope, scope === "page" ? page : undefined)
      .then((d) => setText(d.diff))
      .catch((err) => {
        setText("");
        setError(err instanceof Error ? err.message : String(err));
      });
  }, [run, scope, page, attempt, attempts, detail?.phase]);

  const files = useMemo(() => parseDiff(text), [text]);
  const hits = useMemo(() => {
    if (!find) return 0;
    const re = new RegExp(find.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"), "gi");
    return files.reduce((n, f) => n + f.lines.reduce((m, l) => m + (l.text.match(re)?.length ?? 0), 0), 0);
  }, [files, find]);

  useEffect(() => {
    const marks = body.current?.querySelectorAll("mark");
    if (!marks?.length) return;
    marks.forEach((m) => m.classList.remove("current"));
    const m = marks[hit % marks.length];
    m?.classList.add("current");
    m?.scrollIntoView({ block: "center" });
  }, [hit, find, files]);

  const owner = (file: string): string | null => {
    if (scope !== "run") return page || null;
    return pages.find((pg) => globMatch(pg.owned_glob, file))?.id ?? null;
  };

  const viewedKey = (file: string) => `${run}:${file}`;
  const isViewed = (f: FileDiff) => p.viewed[viewedKey(f.path)] === f.hash;

  const submit = async () => {
    if (!comment || !comment.text.trim()) return;
    const to = owner(comment.file);
    if (!to) {
      toast("No page owns this file, so there is no worker to send the comment to", comment.file);
      return;
    }
    try {
      await api.addNote(run, `page:${to}`, `Comment on ${comment.file}:${comment.line}: ${comment.text.trim()}`);
      setQueued((q) => [...q, comment]);
      toast(`Queued to the ${to} worker`, "It is fix text on that page's next repair.");
      setComment(null);
    } catch (err) {
      toast(err instanceof Error ? err.message : String(err));
    }
  };

  return (
    <div className="diff-tab">
      <div className="toolbar">
        <div className="seg" role="radiogroup" aria-label="Diff scope">
          {(["attempt", "page", "run"] as Scope[]).map((s) => (
            <button key={s} role="radio" aria-checked={scope === s} className={scope === s ? "on" : ""} onClick={() => setScope(s)}>
              {s === "attempt" ? "this attempt" : s === "page" ? "this page" : "whole run"}
            </button>
          ))}
        </div>
        {scope !== "run" && (
          <select value={page} onChange={(e) => setPage(e.target.value)} aria-label="Page">
            {pages.map((pg) => (
              <option key={pg.id} value={pg.id}>
                {pg.id}
              </option>
            ))}
          </select>
        )}
        {scope === "attempt" && (
          <select value={attempt} onChange={(e) => setAttempt(Number(e.target.value))} aria-label="Attempt">
            {attempts.map((a, i) => (
              <option key={i} value={i}>
                #{i + 1} {a.outcome}
              </option>
            ))}
          </select>
        )}
        <input
          className="find"
          placeholder="Find in diff"
          value={find}
          onChange={(e) => {
            setFind(e.target.value);
            setHit(0);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") setHit((h) => (e.shiftKey ? h - 1 + hits : h + 1) % Math.max(1, hits));
          }}
        />
        {find && <span className="muted small">{hits ? `${(hit % hits) + 1}/${hits}` : "0/0"}</span>}
      </div>
      {error && <div className="tab-empty">{error}</div>}
      {!error && files.length === 0 && <div className="tab-empty">No changes in this scope.</div>}
      <div className="diff-body" ref={body}>
        {files.map((f) => {
          const viewed = isViewed(f);
          return (
            <section key={f.path} className={`diff-file ${viewed ? "viewed" : ""}`}>
              <header
                className="diff-file-head"
                onContextMenu={(e) => {
                  const full = resolvedPath(f.path);
                  if (!full) return;
                  e.preventDefault();
                  openPathMenu(e.clientX, e.clientY, full, { raw: f.path });
                }}
              >
                <KnownPath path={f.path} />
                <span className="small">
                  <span className="diff-add">+{f.added}</span> <span className="diff-del">−{f.removed}</span>
                </span>
                <label className="check small">
                  <input
                    type="checkbox"
                    checked={viewed}
                    onChange={(e) =>
                      setPrefs({ viewed: { ...prefs().viewed, [viewedKey(f.path)]: e.target.checked ? f.hash : "" } })
                    }
                  />
                  viewed
                </label>
              </header>
              {!viewed && (
                <table className="diff-lines">
                  <tbody>
                    {f.lines.map((l, i) => {
                      const target = l.new ?? l.old ?? 0;
                      const canComment = l.kind === "add" || l.kind === "ctx";
                      const open = comment && comment.file === f.path && comment.line === target && canComment;
                      const done = queued.filter((q) => q.file === f.path && q.line === target);
                      return (
                        <FragmentRow key={i}>
                          <tr className={`dl ${l.kind}`}>
                            {l.kind === "hunk" ? (
                              <td colSpan={3} className="hunk">
                                {l.text}
                              </td>
                            ) : (
                              <>
                                <td
                                  className={`gutter ${canComment ? "commentable" : ""}`}
                                  title={canComment ? "Comment on this line" : undefined}
                                  onClick={() => canComment && setComment({ file: f.path, line: target, text: "" })}
                                >
                                  {l.old ?? ""}
                                </td>
                                <td
                                  className={`gutter ${canComment ? "commentable" : ""}`}
                                  onClick={() => canComment && setComment({ file: f.path, line: target, text: "" })}
                                >
                                  {l.new ?? ""}
                                </td>
                                <td className="code-cell">
                                  <span className="sign">{l.kind === "add" ? "+" : l.kind === "del" ? "−" : " "}</span>
                                  <Highlighted text={l.text} find={find} />
                                </td>
                              </>
                            )}
                          </tr>
                          {done.map((q, j) => (
                            <tr key={`q${j}`} className="dl queued">
                              <td colSpan={3}>
                                <div className="draft small">
                                  queued to {owner(q.file) ?? "?"}: {q.text}
                                </div>
                              </td>
                            </tr>
                          ))}
                          {open && (
                            <tr className="dl commenting">
                              <td colSpan={3}>
                                <div className="comment-box">
                                  <textarea
                                    autoFocus
                                    rows={2}
                                    placeholder={`Fix text for the ${owner(f.path) ?? "?"} worker, about line ${target}`}
                                    value={comment.text}
                                    onChange={(e) => setComment({ ...comment, text: e.target.value })}
                                    onKeyDown={(e) => {
                                      if (e.key === "Enter" && !e.shiftKey) {
                                        e.preventDefault();
                                        void submit();
                                      }
                                      if (e.key === "Escape") setComment(null);
                                    }}
                                  />
                                  <div className="row gap">
                                    <button className="primary small" onClick={() => void submit()}>
                                      Queue to worker
                                    </button>
                                    <button className="ghost small" onClick={() => setComment(null)}>
                                      Cancel
                                    </button>
                                  </div>
                                </div>
                              </td>
                            </tr>
                          )}
                        </FragmentRow>
                      );
                    })}
                  </tbody>
                </table>
              )}
            </section>
          );
        })}
      </div>
    </div>
  );
}

function FragmentRow({ children }: { children: React.ReactNode }) {
  return <>{children}</>;
}
