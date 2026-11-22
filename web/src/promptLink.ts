// Prompt links: `/runs/new?prompt=…&mode=agent` opens New Run with the prompt in the
// composer. A link never runs anything: the person reads it and presses Send. Characters a
// person cannot see (zero-width, bidi controls, tag characters) can hide instructions for
// an agent, so they are removed and the person is told.

import type { RunMode } from "./api";

const MAX = 8000;
// Zero-width and joiners, bidi embeddings and isolates, word joiner and invisible operators,
// the byte-order mark, and the Unicode tag block.
const HIDDEN = /[​-‏‪-‮⁠-⁤⁦-⁩﻿]|\uDB40[\uDC00-\uDC7F]/g;

export function stripHidden(text: string): { text: string; removed: number } {
  let removed = 0;
  const clean = text.replace(HIDDEN, () => {
    removed += 1;
    return "";
  });
  return { text: clean, removed };
}

export interface PromptLink {
  prompt: string;
  mode: RunMode | null;
  /** How many hidden characters were taken out. */
  removed: number;
  /** A leading `/goal` line the link carried, taken out: a goal's check runs on this machine. */
  goal: string | null;
}

/** Take a leading `/goal …` line off a link's prompt. The daemon reads that line as a goal,
 * and a goal can name a command it runs here, so a link may not set one. */
export function dropGoalLine(text: string): { text: string; goal: string | null } {
  const lines = text.replace(/^\s+/, "").split("\n");
  const first = lines[0] ?? "";
  const m = /^\/goal[ \t]+(.+)$/.exec(first.trim());
  if (!m) return { text, goal: null };
  return { text: lines.slice(1).join("\n").trim(), goal: m[1]!.trim() };
}

/** The prompt a link carries, from a URL's query. `null` when it carries none. */
export function readPromptLink(search: string): PromptLink | null {
  const params = new URLSearchParams(search);
  const raw = params.get("prompt");
  if (!raw || !raw.trim()) return null;
  const { text: visible, removed } = stripHidden(raw.slice(0, MAX));
  const { text, goal } = dropGoalLine(visible);
  const mode = params.get("mode");
  return { prompt: text, mode: mode === "auto" || mode === "code" || mode === "agent" ? mode : null, removed, goal };
}

/** A link that opens New Run on this daemon with `prompt` ready to send. */
export function promptLink(prompt: string, mode?: RunMode, origin = window.location.origin): string {
  const params = new URLSearchParams({ prompt: prompt.slice(0, MAX) });
  if (mode && mode !== "auto") params.set("mode", mode);
  return `${origin}/runs/new?${params.toString()}`;
}
