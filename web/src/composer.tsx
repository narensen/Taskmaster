// The composer: a text field with chips. Enter sends, Shift+Enter breaks the line, Arrow
// Up in an empty field recalls what this browser sent last. @ inserts a file, page, agent
// or Dictionary key; # and digits a pull request gh can see; / at the start a command or a
// skill. A big paste or a dropped file becomes an attachment.

import { forwardRef, useEffect, useImperativeHandle, useMemo, useRef, useState, type ReactNode } from "react";
import { navigate } from "./router";
import { api, type Skill } from "./api";
import { commands, type Command } from "./commands";
import { MOD } from "./desktop";
import { emit, toast, useOn, type Chip } from "./overlay";
import { prefs, rememberPrompt } from "./prefs";

export const BIG_PASTE = 32 * 1024;

export interface Message {
  text: string;
  chips: Chip[];
  attachments: Chip[];
}

/** How a chip reads in the message that is sent, and in copied Markdown. */
export function chipText(chip: Chip): string {
  switch (chip.kind) {
    case "file":
      return chip.value + (chip.line ? `:${chip.line}` : "");
    case "page":
      return `page \`${chip.value}\``;
    case "agent":
      return `the ${chip.value}`;
    case "key":
      return `Dictionary key \`${chip.value}\``;
    case "pr":
      return `#${chip.value}${chip.url ? ` (${chip.url})` : ""}`;
    case "skill":
      return `skill \`${chip.value}\``;
    case "cite":
      return `\n> ${chip.value.split("\n").join("\n> ")}\n${chip.comment ? `${chip.comment}\n` : ""}`;
    case "search":
      return `[${chip.label}](${chip.url ?? chip.value})`;
    case "attachment":
      return chip.label;
  }
}

/** The whole message as text: chips written out, attachments after it. */
export function messageText(message: Message): string {
  let out = message.text;
  for (const a of message.attachments) {
    out += a.value ? `\n\n--- attached: ${a.label} ---\n${a.value}\n--- end of ${a.label} ---` : `\n\n(attached image: ${a.label})`;
  }
  return out.trim();
}

let fileCache: Promise<string[]> | null = null;
let keyCache: Promise<string[]> | null = null;
let skillCache: Promise<Skill[]> | null = null;

function files() {
  return (fileCache ??= api.files().catch(() => ((fileCache = null), [])));
}
function keys() {
  return (keyCache ??= api
    .dictionary("")
    .then((e) => e.map((x) => x.key))
    .catch(() => ((keyCache = null), [])));
}
export function skills() {
  return (skillCache ??= api.skills().catch(() => ((skillCache = null), [])));
}
/** Read the skills again next time: one was installed, removed or switched. */
export function forgetSkills() {
  skillCache = null;
}

interface Option {
  id: string;
  label: string;
  group: string;
  pick: () => void;
}

export interface ComposerHandle {
  focus: () => void;
  insert: (chip: Chip) => void;
  clear: () => void;
  isEmpty: () => boolean;
  read: () => Message;
  attach: (chips: Chip[]) => void;
  addFiles: (files: FileList | File[]) => Promise<void>;
  /** Replace the text with `text` and put the caret at its end. */
  setText: (text: string) => void;
}

export interface ComposerProps {
  placeholder?: string;
  onSend: (message: Message, opts?: { alt?: boolean }) => void;
  onAltUp?: () => void;
  pages?: string[];
  agents?: string[];
  autoFocus?: boolean;
  disabled?: boolean;
  /** Listen for chips handed over from elsewhere (search results, cites, skills). */
  bus?: boolean;
  onChipClick?: (chip: Chip) => void;
  className?: string;
  onChange?: (message: Message) => void;
  /** Controls under the field: a model pill, a send button. */
  footer?: ReactNode;
}

function chipNode(chip: Chip): HTMLSpanElement {
  const span = document.createElement("span");
  span.contentEditable = "false";
  span.className = `chip chip-${chip.kind}${chip.url ? " linked" : ""}`;
  span.dataset.chip = JSON.stringify(chip);
  span.title = chip.kind === "file" ? chip.value : chip.url ?? chip.value;
  span.textContent = chip.label;
  return span;
}

function serialize(root: HTMLElement): { text: string; chips: Chip[] } {
  const chips: Chip[] = [];
  let text = "";
  const walk = (node: Node) => {
    if (node.nodeType === Node.TEXT_NODE) text += node.textContent ?? "";
    else if (node instanceof HTMLElement) {
      if (node.dataset.chip) {
        const chip = JSON.parse(node.dataset.chip) as Chip;
        chips.push(chip);
        text += chipText(chip);
      } else if (node.tagName === "BR") text += "\n";
      else {
        const block = node.tagName === "DIV" || node.tagName === "P";
        if (block && text && !text.endsWith("\n")) text += "\n";
        node.childNodes.forEach(walk);
      }
    }
  };
  root.childNodes.forEach(walk);
  return { text: text.replace(/\u00a0/g, " "), chips };
}

/** A6: a stashed draft, its text and chips in order, and its attachments. */
type Part = { t: string } | { chip: Chip };
interface Stash {
  parts: Part[];
  attachments: Chip[];
}

const STASH_KEY = "taskmaster.stash";
let stashInTab: Stash | null = null;

function partsOf(root: HTMLElement): Part[] {
  const parts: Part[] = [];
  const walk = (node: Node) => {
    if (node.nodeType === Node.TEXT_NODE) parts.push({ t: node.textContent ?? "" });
    else if (node instanceof HTMLElement) {
      if (node.dataset.chip) parts.push({ chip: JSON.parse(node.dataset.chip) as Chip });
      else if (node.tagName === "BR") parts.push({ t: "\n" });
      else {
        if ((node.tagName === "DIV" || node.tagName === "P") && parts.length) parts.push({ t: "\n" });
        node.childNodes.forEach(walk);
      }
    }
  };
  root.childNodes.forEach(walk);
  return parts;
}

/** Keep the stash in this browser; a stash too big for storage stays for this tab. */
function saveStash(stash: Stash): "kept" | "tab" {
  stashInTab = stash;
  try {
    localStorage.setItem(STASH_KEY, JSON.stringify(stash));
    return "kept";
  } catch {
    localStorage.removeItem(STASH_KEY);
    return "tab";
  }
}

function takeStash(): Stash | null {
  let stash = stashInTab;
  try {
    const raw = localStorage.getItem(STASH_KEY);
    if (raw) stash = JSON.parse(raw) as Stash;
  } catch {
    // A damaged stash is no stash.
  }
  stashInTab = null;
  localStorage.removeItem(STASH_KEY);
  return stash;
}

export function readFile(file: File): Promise<Chip> {
  return new Promise((resolve) => {
    const reader = new FileReader();
    if (file.type.startsWith("image/")) {
      reader.onload = () => resolve({ kind: "attachment", label: file.name, value: "", thumb: String(reader.result), bytes: file.size });
      reader.readAsDataURL(file);
    } else {
      reader.onload = () => resolve({ kind: "attachment", label: file.name, value: String(reader.result), bytes: file.size });
      reader.readAsText(file.slice(0, 1024 * 1024));
    }
  });
}

export const Composer = forwardRef<ComposerHandle, ComposerProps>(function Composer(props, handle) {
  const ref = useRef<HTMLDivElement>(null);
  const [attachments, setAttachments] = useState<Chip[]>([]);
  const [empty, setEmpty] = useState(true);
  const [popup, setPopup] = useState<{ mode: "@" | "#" | "/" | "$"; query: string } | null>(null);
  const [options, setOptions] = useState<Option[]>([]);
  const [active, setActive] = useState(0);
  const [drag, setDrag] = useState(false);
  const [focused, setFocused] = useState(false);
  const historyAt = useRef<number | null>(null);
  const saved = useRef<Range | null>(null);

  const pages = useMemo(() => props.pages ?? [], [props.pages]);
  const agents = useMemo(() => props.agents ?? ["master", "worker", "mid", "reviewer", "verifier"], [props.agents]);

  const sync = () => {
    const el = ref.current;
    if (!el) return;
    const blank = !el.textContent?.trim() && !el.querySelector("[data-chip]");
    setEmpty(blank);
    props.onChange?.(read());
  };

  const caretRange = (): Range | null => {
    const sel = window.getSelection();
    if (!sel || !sel.rangeCount || !ref.current) return null;
    const range = sel.getRangeAt(0);
    return ref.current.contains(range.startContainer) ? range : null;
  };

  const insert = (chip: Chip, trigger = 0) => {
    const el = ref.current;
    if (!el) return;
    el.focus();
    let range = caretRange() ?? saved.current;
    if (!range || !el.contains(range.startContainer)) {
      range = document.createRange();
      range.selectNodeContents(el);
      range.collapse(false);
    }
    if (trigger > 0 && range.startContainer.nodeType === Node.TEXT_NODE) {
      const start = Math.max(0, range.startOffset - trigger);
      range.setStart(range.startContainer, start);
      range.deleteContents();
    }
    const node = chipNode(chip);
    const space = document.createTextNode("\u00a0");
    range.insertNode(space);
    range.insertNode(node);
    const after = document.createRange();
    after.setStartAfter(space);
    after.collapse(true);
    const sel = window.getSelection();
    sel?.removeAllRanges();
    sel?.addRange(after);
    setPopup(null);
    sync();
  };

  const clear = () => {
    if (ref.current) ref.current.innerHTML = "";
    setAttachments([]);
    setPopup(null);
    historyAt.current = null;
    sync();
  };

  const read = (): Message => {
    const { text, chips } = ref.current ? serialize(ref.current) : { text: "", chips: [] };
    return { text: text.trim(), chips, attachments };
  };

  useImperativeHandle(handle, () => ({
    focus: () => ref.current?.focus(),
    insert: (chip) => insert(chip),
    clear,
    isEmpty: () => empty && attachments.length === 0,
    read,
    attach: (chips) => setAttachments((a) => [...a, ...chips]),
    addFiles: async (files) => {
      const chips = await Promise.all([...files].map(readFile));
      setAttachments((a) => [...a, ...chips]);
    },
    setText: (text) => {
      if (!ref.current) return;
      ref.current.textContent = text;
      sync();
      ref.current.focus();
      const range = document.createRange();
      range.selectNodeContents(ref.current);
      range.collapse(false);
      const selection = window.getSelection();
      selection?.removeAllRanges();
      selection?.addRange(range);
    },
  }));

  useOn<Chip>("chip", (chip) => {
    if (props.bus) insert(chip);
  });

  useEffect(() => {
    if (props.autoFocus) ref.current?.focus();
  }, [props.autoFocus]);

  // The options for an open popup.
  useEffect(() => {
    if (!popup) return;
    let stale = false;
    const q = popup.query.toLowerCase();
    const len = popup.query.length + 1;
    void (async () => {
      const list: Option[] = [];
      if (popup.mode === "@") {
        for (const role of agents) {
          if (role.toLowerCase().includes(q))
            list.push({ id: `a:${role}`, label: role, group: "Agent", pick: () => insert({ kind: "agent", label: `@${role}`, value: role }, len) });
        }
        for (const page of pages) {
          if (page.toLowerCase().includes(q))
            list.push({ id: `p:${page}`, label: page, group: "Page", pick: () => insert({ kind: "page", label: `@${page}`, value: page }, len) });
        }
        const [fs, ks] = await Promise.all([files(), keys()]);
        for (const f of fs.filter((f) => f.toLowerCase().includes(q)).slice(0, 8)) {
          list.push({
            id: `f:${f}`,
            label: f,
            group: "File",
            pick: () => insert({ kind: "file", label: `@${f.split("/").pop()}`, value: f }, len),
          });
        }
        for (const k of ks.filter((k) => k.toLowerCase().includes(q)).slice(0, 6)) {
          list.push({ id: `k:${k}`, label: k, group: "Dictionary", pick: () => insert({ kind: "key", label: `@${k}`, value: k }, len) });
        }
      } else if (popup.mode === "$") {
        const all = (await skills()).filter((s) => s.place === "store" && s.enabled);
        for (const s of all.filter((s) => s.name.toLowerCase().includes(q) || s.description.toLowerCase().includes(q))) {
          list.push({
            id: `$:${s.name}`,
            label: s.name,
            group: s.description ? s.description.slice(0, 48) + (s.description.length > 48 ? "…" : "") : "Skill",
            pick: () => insert({ kind: "skill", label: `$${s.name}`, value: s.name, to: "master" }, len),
          });
        }
        list.push({
          id: "$:find",
          label: popup.query ? `Find skills for “${popup.query}”` : all.length ? "Find more skills" : "No skills yet: find some",
          group: "Skills",
          pick: () => {
            setPopup(null);
            navigate(popup.query ? `/skills?q=${encodeURIComponent(popup.query)}` : "/skills");
          },
        });
      } else if (popup.mode === "#") {
        if (/^\d+$/.test(popup.query)) {
          const n = Number(popup.query);
          list.push({
            id: `pr:${n}`,
            label: `Attach pull request #${n}`,
            group: "Pull request",
            pick: () => {
              setPopup(null);
              api
                .ghPr(n)
                .then((pr) => insert({ kind: "pr", label: `#${pr.number}`, value: String(pr.number), url: pr.url }, len))
                .catch((err) => toast(`gh cannot see pull request #${n}`, err instanceof Error ? err.message : String(err)));
            },
          });
        }
      } else {
        const cmds: Command[] = commands();
        for (const c of cmds.filter((c) => c.label.toLowerCase().includes(q))) {
          list.push({
            id: `c:${c.id}`,
            label: c.label,
            group: c.group === "Go" ? "Command" : c.group,
            pick: () => {
              clear();
              c.run();
            },
          });
        }
        for (const s of (await skills()).filter((s) => s.name.toLowerCase().includes(q))) {
          list.push({
            id: `s:${s.name}`,
            label: s.name,
            group: "Skill",
            pick: () => insert({ kind: "skill", label: `/${s.name}`, value: s.name, to: "master" }, len),
          });
        }
      }
      if (!stale) {
        setOptions(list.slice(0, 24));
        setActive(0);
      }
    })();
    return () => {
      stale = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [popup?.mode, popup?.query, pages, agents]);

  const detect = () => {
    const range = caretRange();
    if (!range || !ref.current) return setPopup(null);
    saved.current = range.cloneRange();
    const node = range.startContainer;
    const before = node.nodeType === Node.TEXT_NODE ? (node.textContent ?? "").slice(0, range.startOffset) : "";
    const whole = serialize(ref.current).text;
    const slash = /^\/([\w-]*)$/.exec(whole.trimStart());
    if (slash && before.endsWith(`/${slash[1]}`)) return setPopup({ mode: "/", query: slash[1]! });
    const m = /(?:^|[\s\u00a0(])([@#$])([\w./:-]*)$/.exec(before);
    // `$5` is money, not a skill.
    if (m && !(m[1] === "$" && /^[^A-Za-z]/.test(m[2]!))) return setPopup({ mode: m[1] as "@" | "#" | "$", query: m[2]! });
    setPopup(null);
  };

  const send = (alt = false) => {
    const message = read();
    if (!message.text && !message.attachments.length) return;
    rememberPrompt(messageText(message));
    props.onSend(message, { alt });
  };

  const setText = (text: string) => {
    if (!ref.current) return;
    ref.current.textContent = text;
    const range = document.createRange();
    range.selectNodeContents(ref.current);
    range.collapse(false);
    const sel = window.getSelection();
    sel?.removeAllRanges();
    sel?.addRange(range);
    sync();
  };

  // A6: Ctrl/Cmd+S puts the draft aside; on an empty box it brings it back.
  const stashOrRestore = () => {
    const el = ref.current;
    if (!el) return;
    const blank = !el.textContent?.trim() && !el.querySelector("[data-chip]") && attachments.length === 0;
    if (!blank) {
      const where = saveStash({ parts: partsOf(el), attachments });
      clear();
      toast("Draft stashed", `${MOD}+S on an empty box brings it back${where === "tab" ? " (kept in this tab only: it is too big to store)" : ""}.`, 4000);
      return;
    }
    const stash = takeStash();
    if (!stash) {
      toast("Nothing stashed", `${MOD}+S puts a draft aside.`, 2500);
      return;
    }
    el.innerHTML = "";
    for (const part of stash.parts) {
      el.appendChild("chip" in part ? chipNode(part.chip) : document.createTextNode(part.t));
    }
    setAttachments(stash.attachments ?? []);
    sync();
    el.focus();
    const range = document.createRange();
    range.selectNodeContents(el);
    range.collapse(false);
    window.getSelection()?.removeAllRanges();
    window.getSelection()?.addRange(range);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key.toLowerCase() === "s") {
      e.preventDefault();
      return stashOrRestore();
    }
    if (popup && options.length) {
      if (e.key === "ArrowDown") {
        e.preventDefault();
        return setActive((a) => (a + 1) % options.length);
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        return setActive((a) => (a - 1 + options.length) % options.length);
      }
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault();
        return options[active]?.pick();
      }
    }
    if (e.key === "Escape" && popup) {
      e.preventDefault();
      return setPopup(null);
    }
    if (e.key === "ArrowUp" && e.altKey && props.onAltUp) {
      e.preventDefault();
      return props.onAltUp();
    }
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      return send(e.metaKey || e.ctrlKey);
    }
    if (e.key === "Enter" && e.shiftKey) {
      e.preventDefault();
      document.execCommand("insertLineBreak");
      return;
    }
    const history = prefs().history;
    if (e.key === "ArrowUp" && (empty || historyAt.current !== null) && history.length) {
      e.preventDefault();
      const at = historyAt.current === null ? history.length - 1 : Math.max(0, historyAt.current - 1);
      historyAt.current = at;
      return setText(history[at] ?? "");
    }
    if (e.key === "ArrowDown" && historyAt.current !== null) {
      e.preventDefault();
      const at = historyAt.current + 1;
      if (at >= history.length) {
        historyAt.current = null;
        return setText("");
      }
      historyAt.current = at;
      return setText(history[at] ?? "");
    }
  };

  const attach = async (list: FileList | File[]) => {
    const read = await Promise.all([...list].map(readFile));
    setAttachments((a) => [...a, ...read]);
  };

  const onPaste = (e: React.ClipboardEvent) => {
    const dropped = [...e.clipboardData.files];
    if (dropped.length) {
      e.preventDefault();
      void attach(dropped);
      return;
    }
    const text = e.clipboardData.getData("text/plain");
    e.preventDefault();
    if (text.length >= BIG_PASTE) {
      const n = attachments.filter((a) => a.label.startsWith("pasted")).length + 1;
      setAttachments((a) => [...a, { kind: "attachment", label: `pasted-${n}.txt`, value: text, bytes: text.length }]);
      return;
    }
    document.execCommand("insertText", false, text);
  };

  return (
    <div
      className={`composer ${drag ? "drag" : ""} ${props.className ?? ""}`}
      onFocus={() => setFocused(true)}
      onBlur={(e) => !e.currentTarget.contains(e.relatedTarget as Node | null) && setFocused(false)}
      onDragOver={(e) => {
        if (e.dataTransfer.types.includes("Files")) {
          e.preventDefault();
          setDrag(true);
        }
      }}
      onDragLeave={() => setDrag(false)}
      onDrop={(e) => {
        setDrag(false);
        if (e.dataTransfer.files.length) {
          e.preventDefault();
          void attach(e.dataTransfer.files);
        }
      }}
    >
      <span
        className={`composer-ring ${focused || drag ? "on" : ""}`}
        aria-hidden
      />
      {drag && <div className="composer-drop">Drop to attach</div>}
      {attachments.length > 0 && (
        <div className="attachments">
          {attachments.map((a, i) => (
            <span key={`${a.label}-${i}`} className="chip chip-attachment">
              {a.thumb && <img src={a.thumb} alt="" className="thumb" />}
              <span>{a.label}</span>
              {a.bytes !== undefined && <span className="muted">{Math.ceil(a.bytes / 1024)} KiB</span>}
              <button className="chip-x" aria-label={`Remove ${a.label}`} onClick={() => setAttachments((all) => all.filter((_, j) => j !== i))}>
                ×
              </button>
            </span>
          ))}
        </div>
      )}
      {popup && options.length > 0 && (
        <div className="popup" role="listbox">
          {options.map((o, i) => (
            <button
              key={o.id}
              role="option"
              aria-selected={i === active}
              className={`popup-item ${i === active ? "on" : ""}`}
              onMouseDown={(e) => {
                e.preventDefault();
                o.pick();
              }}
            >
              <span className="clip">{o.label}</span>
              <span className="muted small">{o.group}</span>
            </button>
          ))}
        </div>
      )}
      <div
        ref={ref}
        className="composer-input"
        contentEditable={!props.disabled}
        suppressContentEditableWarning
        role="textbox"
        aria-multiline="true"
        aria-label={props.placeholder ?? "Message"}
        data-placeholder={props.placeholder}
        data-empty={empty ? "true" : "false"}
        spellCheck
        onInput={() => {
          sync();
          detect();
          historyAt.current = null;
        }}
        onKeyUp={(e) => {
          if (!["ArrowUp", "ArrowDown", "Enter", "Tab", "Escape"].includes(e.key)) detect();
        }}
        onClick={(e) => {
          const chipEl = (e.target as HTMLElement).closest("[data-chip]") as HTMLElement | null;
          if (chipEl?.dataset.chip) {
            const chip = JSON.parse(chipEl.dataset.chip) as Chip;
            if (chip.url) window.open(chip.url, "_blank", "noopener");
            else if (props.onChipClick) props.onChipClick(chip);
            else emit("chip-jump", chip);
          }
          detect();
        }}
        onKeyDown={onKeyDown}
        onPaste={onPaste}
        onBlur={() => window.setTimeout(() => setPopup(null), 150)}
      />
      {props.footer}
    </div>
  );
});
