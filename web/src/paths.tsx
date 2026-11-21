// Path words. Any path an agent, a tool call, a diff, an error, a terminal line or a plan
// writes becomes a linked word when the daemon says it is a real file. The word is the file
// name, with the shortest parent suffix that tells it apart from another file of the same
// name. A line, a line and column, or a range stays on the word. A click asks the daemon to
// open it in the editor chosen in Settings, on the machine that holds the project.

import { Fragment, memo, useEffect, useMemo, useRef, useSyncExternalStore, type ReactNode } from "react";
import { api, ApiError, type EditorInfo } from "./api";
import { emit, openMenu, toast, type MenuItem } from "./overlay";
import { prefs } from "./prefs";

export interface PathRef {
  raw: string;
  line?: number;
  column?: number;
  end?: number;
}

// Finding candidates.

const URL_RE = /\bhttps?:\/\/[^\s<>()"'`\]]+[^\s<>()"'`\].,;:!?]/g;
const SECRET_RES: RegExp[] = [
  /\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{16,}/g,
  /\bgh[pousr]_[A-Za-z0-9]{20,}/g,
  /\bgithub_pat_[A-Za-z0-9_]{20,}/g,
  /\bAKIA[0-9A-Z]{16}\b/g,
  /\bxox[abprs]-[A-Za-z0-9-]{10,}/g,
  /\bAIza[0-9A-Za-z_-]{30,}/g,
  /\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}/g,
  /(?<=\b(?:token|secret|password|passwd|api[_-]?key|bearer)\b["']?\s*[=:]\s*["']?)[^\s"'`,;]{12,}/gi,
];
// A path: optional ~/ ./ ../ or /, segments, and either a slash or an extension. Then an
// optional :line, :line:col, :line-end, #Lline or #Lline-Lend.
const PATH_RE =
  /(?<![\w@~./-])((?:~\/|\.{1,2}\/|\/)?(?:[\w@+-][\w@.+-]*\/)*[\w@+-][\w@.+-]*)(?::(\d+)(?::(\d+)|-(\d+))?|#L(\d+)(?:-L?(\d+))?|\((\d+),(\d+)\))?/g;

export function looksLikePath(token: string): boolean {
  if (token.length < 3 || token.length > 400) return false;
  if (/^\d+(\.\d+)*$/.test(token)) return false;
  const base = token.split("/").pop() ?? "";
  const hasExt = /\.[A-Za-z0-9]{1,10}$/.test(base) && !/^\.+$/.test(base);
  return token.includes("/") ? token.replace(/^\.{0,2}\//, "").length > 1 : hasExt;
}

export type Piece =
  | { t: "text"; s: string }
  | { t: "url"; s: string }
  | { t: "secret"; s: string }
  | { t: "path"; s: string; ref: PathRef };

function splitBy(pieces: Piece[], re: RegExp, make: (m: RegExpExecArray) => Piece | null): Piece[] {
  const out: Piece[] = [];
  for (const piece of pieces) {
    if (piece.t !== "text") {
      out.push(piece);
      continue;
    }
    let last = 0;
    re.lastIndex = 0;
    let m: RegExpExecArray | null;
    while ((m = re.exec(piece.s))) {
      const made = make(m);
      if (!made) continue;
      let text = m[0];
      // A sentence's full stop is not part of a path.
      if (made.t === "path") {
        const trimmed = made.ref.raw.replace(/[.]+$/, "");
        if (trimmed !== made.ref.raw && !made.ref.line) {
          made.ref.raw = trimmed;
          text = trimmed;
          made.s = trimmed;
        }
      }
      if (m.index > last) out.push({ t: "text", s: piece.s.slice(last, m.index) });
      out.push(made);
      last = m.index + text.length;
      re.lastIndex = last;
    }
    if (last < piece.s.length) out.push({ t: "text", s: piece.s.slice(last) });
  }
  return out;
}

const PIECES_CACHE_MAX = 512;
const piecesCache = new Map<string, Piece[]>();

export function pieces(text: string): Piece[] {
  if (text.length <= 8000) {
    const hit = piecesCache.get(text);
    if (hit) {
      piecesCache.delete(text);
      piecesCache.set(text, hit);
      return hit;
    }
  }
  let out: Piece[] = [{ t: "text", s: text }];
  out = splitBy(out, URL_RE, (m) => ({ t: "url", s: m[0] }));
  for (const re of SECRET_RES) out = splitBy(out, re, (m) => ({ t: "secret", s: m[0] }));
  out = splitBy(out, PATH_RE, (m) => {
    const raw = m[1]!;
    if (!looksLikePath(raw.replace(/[.]+$/, ""))) return null;
    const num = (i: number) => (m[i] ? Number(m[i]) : undefined);
    const ref: PathRef = {
      raw,
      line: num(2) ?? num(5) ?? num(7),
      column: num(3) ?? num(8),
      end: num(4) ?? num(6),
    };
    return { t: "path", s: m[0], ref };
  });
  if (text.length <= 8000) {
    if (piecesCache.size >= PIECES_CACHE_MAX) {
      const oldest = piecesCache.keys().next().value;
      if (oldest !== undefined) piecesCache.delete(oldest);
    }
    piecesCache.set(text, out);
  }
  return out;
}

export function mask(secret: string): string {
  return `${secret.slice(0, 4)}••••••`;
}

/** Text with every secret masked, for copying. */
export function masked(text: string): string {
  return pieces(text)
    .map((p) => (p.t === "secret" ? mask(p.s) : p.s))
    .join("");
}

// Resolving, in batches.

type Resolved = Record<string, string | null>;
let resolved: Resolved = {};
const pending = new Set<string>();
let timer: number | null = null;
const pathListeners = new Set<() => void>();
const editorListeners = new Set<() => void>();
let editorVersion = 0;
const labelCache = new Map<string, string>();
let byBasename = new Map<string, Set<string>>();

function rebuildIndex() {
  labelCache.clear();
  const map = new Map<string, Set<string>>();
  for (const full of Object.values(resolved)) {
    if (!full) continue;
    const base = full.split("/").pop() ?? full;
    let bucket = map.get(base);
    if (!bucket) {
      bucket = new Set();
      map.set(base, bucket);
    }
    bucket.add(full);
  }
  byBasename = map;
}

function notifyPaths() {
  rebuildIndex();
  pathListeners.forEach((l) => l());
}

function notifyEditors() {
  editorVersion++;
  editorListeners.forEach((l) => l());
}

function subscribePaths(l: () => void) {
  pathListeners.add(l);
  return () => pathListeners.delete(l);
}

function subscribeEditors(l: () => void) {
  editorListeners.add(l);
  return () => editorListeners.delete(l);
}

function flush() {
  timer = null;
  const batch = [...pending].slice(0, 400);
  batch.forEach((p) => pending.delete(p));
  if (!batch.length) return;
  api
    .paths(batch)
    .then((reply) => {
      let changed = false;
      for (const [k, v] of Object.entries(reply.paths)) {
        if (resolved[k] !== v) {
          resolved[k] = v;
          changed = true;
        }
      }
      if (changed) notifyPaths();
    })
    .catch(() => {
      // Unknown stays plain text; try again later.
      batch.forEach((p) => delete resolved[p]);
    });
  if (pending.size) timer = window.setTimeout(flush, 30);
}

function want(raw: string) {
  if (raw in resolved || pending.has(raw)) return;
  pending.add(raw);
  if (timer === null) timer = window.setTimeout(flush, 30);
}

/** Subscribe a PathWord only to its own resolved path + disambiguated label. */
function usePathSnapshot(raw: string): string {
  return useSyncExternalStore(subscribePaths, () => {
    const full = resolved[raw];
    return full ? `${full}\0${label(full)}` : "";
  });
}

export function resolvedPath(raw: string): string | null | undefined {
  return raw in resolved ? resolved[raw] : undefined;
}

/** The visible word: the file name, with the shortest parent suffix that is unique. */
export function label(full: string): string {
  const cached = labelCache.get(full);
  if (cached !== undefined) return cached;
  const parts = full.split("/").filter(Boolean);
  const base = parts[parts.length - 1] ?? full;
  const bucket = byBasename.get(base);
  if (!bucket || bucket.size <= 1) {
    labelCache.set(full, base);
    return base;
  }
  const sameName = [...bucket].filter((o) => o !== full);
  for (let n = 1; n <= parts.length; n++) {
    const suffix = parts.slice(-n).join("/");
    if (!sameName.some((o) => o === suffix || o.endsWith(`/${suffix}`))) {
      labelCache.set(full, suffix);
      return suffix;
    }
  }
  labelCache.set(full, full);
  return full;
}

export function lineSuffix(ref: PathRef): string {
  if (!ref.line) return "";
  if (ref.end) return `:${ref.line}-${ref.end}`;
  if (ref.column) return `:${ref.line}:${ref.column}`;
  return `:${ref.line}`;
}

// Editors.

let editorList: EditorInfo[] | null = null;
let editorDefault = "files";
let editorLoad: Promise<void> | null = null;

export function loadEditors(force = false): Promise<void> {
  if (!editorLoad || force) {
    editorLoad = api
      .editors()
      .then((r) => {
        editorList = r.editors;
        editorDefault = r.default;
        notifyEditors();
      })
      .catch(() => {
        editorLoad = null;
      });
  }
  return editorLoad;
}

export function editors(): { list: EditorInfo[]; fallback: string } {
  return { list: editorList ?? [], fallback: editorDefault };
}

export function useEditors() {
  useSyncExternalStore(subscribeEditors, () => editorVersion);
  useEffect(() => {
    void loadEditors();
  }, []);
  return editors();
}

// Actions.

export async function openFile(full: string, ref: PathRef, editor?: string, reveal = false) {
  try {
    await api.open({
      path: full,
      line: ref.line,
      column: ref.column,
      editor: editor ?? prefs().editor ?? undefined,
      reveal,
    });
  } catch (err) {
    if (err instanceof ApiError && err.status === 424) {
      const body = err.body as { command?: string; path?: string } | null;
      toast(`\`${body?.command ?? editor}\` is not installed on the project machine`, body?.path ?? full);
    } else {
      toast(err instanceof Error ? err.message : String(err), full);
    }
  }
}

function copy(text: string) {
  void navigator.clipboard?.writeText(text).then(
    () => toast("Copied", text, 2500),
    () => toast("Could not copy", text),
  );
}

/** Open the path menu once the editor list is known, so "Open with" is never stale. */
export function openPathMenu(x: number, y: number, full: string, ref: PathRef) {
  void loadEditors().finally(() => openMenu(x, y, pathMenu(full, ref)));
}

export function pathMenu(full: string, ref: PathRef): MenuItem[] {
  const { list } = editors();
  const installed = list.filter((e) => e.installed && e.id !== "files");
  return [
    { label: "Open", hint: "click", run: () => void openFile(full, ref) },
    {
      label: "Open with",
      items: installed.length
        ? installed.map((e) => ({ label: e.name, run: () => void openFile(full, ref, e.id) }))
        : [{ label: "No editor found on the project machine", disabled: true }],
    },
    { label: "Reveal in folder", run: () => void openFile(full, ref, "files", true) },
    { label: "Open preview", hint: "⌘/Ctrl-click", run: () => emit("preview", { path: full, line: ref.line }) },
    { label: "Copy path", run: () => copy(full) },
    { label: "Copy path with line", run: () => copy(full + lineSuffix(ref)), disabled: !ref.line },
  ];
}

export function useLongPress(onLong: (x: number, y: number) => void) {
  const t = useRef<number | null>(null);
  return {
    onTouchStart: (e: React.TouchEvent) => {
      const touch = e.touches[0];
      if (!touch) return;
      const { clientX, clientY } = touch;
      t.current = window.setTimeout(() => onLong(clientX, clientY), 500);
    },
    onTouchEnd: () => t.current && window.clearTimeout(t.current),
    onTouchMove: () => t.current && window.clearTimeout(t.current),
  };
}

export const PathWord = memo(function PathWord({ text, refd }: { text: string; refd: PathRef }) {
  const snap = usePathSnapshot(refd.raw);
  useEffect(() => {
    want(refd.raw);
  }, [refd.raw]);
  const sep = snap.indexOf("\0");
  const full = sep >= 0 ? snap.slice(0, sep) : "";
  const shortLabel = sep >= 0 ? snap.slice(sep + 1) : "";
  const long = useLongPress((x, y) => full && openPathMenu(x, y, full, refd));
  if (!full) return <>{text}</>;
  const suffix = lineSuffix(refd);
  const word = shortLabel + suffix;
  return (
    <a
      className="path"
      href="#"
      title={full + suffix}
      data-path={full}
      data-line={refd.line ?? ""}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        if (e.metaKey || e.ctrlKey) emit("preview", { path: full, line: refd.line });
        else void openFile(full, refd);
      }}
      onContextMenu={(e) => {
        e.preventDefault();
        e.stopPropagation();
        openPathMenu(e.clientX, e.clientY, full, refd);
      }}
      {...long}
    >
      {word}
    </a>
  );
});

export function ExternalLink({ href, children }: { href: string; children?: ReactNode }) {
  return (
    <a className="link-word" href={href} target="_blank" rel="noreferrer noopener">
      {children ?? href}
    </a>
  );
}

/** Text with path words, links and masked secrets. Parsed again only when the text changes. */
export const Rich = memo(function Rich({ text }: { text: string }) {
  const list = pieces(text);
  return (
    <>
      {list.map((p, i) => (
        <Fragment key={i}>
          {p.t === "text" && p.s}
          {p.t === "url" && <ExternalLink href={p.s} />}
          {p.t === "secret" && (
            <span className="secret" title="A token that looks like a secret is masked">
              {mask(p.s)}
            </span>
          )}
          {p.t === "path" && <PathWord text={p.s} refd={p.ref} />}
        </Fragment>
      ))}
    </>
  );
});

/** A known path (a diff file row, a tool target): a word when real, plain otherwise. */
export const KnownPath = memo(function KnownPath({ path, line }: { path: string; line?: number }) {
  const refd = useMemo(() => ({ raw: path, line }), [path, line]);
  return <PathWord text={path} refd={refd} />;
});
