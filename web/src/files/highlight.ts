// Syntax colours from Shiki, with its JavaScript regex engine (no wasm) and a theme of CSS
// variables, so the colours are the app's own tokens: steps of graphite in dark, ink and
// verdigris in light (see `--syn-*` in styles.css). Grammars load the first time a file
// of that kind opens.

import { createCssVariablesTheme, createHighlighterCore, type HighlighterCore, type ThemedToken } from "shiki/core";
import { createJavaScriptRegexEngine } from "shiki/engine/javascript";

const theme = createCssVariablesTheme({ name: "taskmaster", variablePrefix: "--syn-", fontStyle: true });

const GRAMMARS: Record<string, () => Promise<unknown>> = {
  typescript: () => import("shiki/langs/typescript.mjs"),
  tsx: () => import("shiki/langs/tsx.mjs"),
  javascript: () => import("shiki/langs/javascript.mjs"),
  jsx: () => import("shiki/langs/jsx.mjs"),
  json: () => import("shiki/langs/json.mjs"),
  css: () => import("shiki/langs/css.mjs"),
  html: () => import("shiki/langs/html.mjs"),
  markdown: () => import("shiki/langs/markdown.mjs"),
  rust: () => import("shiki/langs/rust.mjs"),
  toml: () => import("shiki/langs/toml.mjs"),
  yaml: () => import("shiki/langs/yaml.mjs"),
  shellscript: () => import("shiki/langs/shellscript.mjs"),
  python: () => import("shiki/langs/python.mjs"),
  go: () => import("shiki/langs/go.mjs"),
};

const BY_EXT: Record<string, string> = {
  ts: "typescript", mts: "typescript", cts: "typescript", tsx: "tsx",
  js: "javascript", mjs: "javascript", cjs: "javascript", jsx: "jsx",
  json: "json", jsonc: "json", css: "css", html: "html", htm: "html",
  md: "markdown", markdown: "markdown", rs: "rust", toml: "toml",
  yml: "yaml", yaml: "yaml", sh: "shellscript", bash: "shellscript", zsh: "shellscript",
  py: "python", go: "go",
};

export function languageOf(path: string): string | null {
  const name = path.split("/").pop() ?? "";
  if (name === "Dockerfile" || name === "Makefile") return null;
  const ext = name.includes(".") ? name.split(".").pop()!.toLowerCase() : "";
  return BY_EXT[ext] ?? null;
}

let core: Promise<HighlighterCore> | null = null;
const loaded = new Set<string>();

function highlighter(): Promise<HighlighterCore> {
  core ??= createHighlighterCore({ themes: [theme], langs: [], engine: createJavaScriptRegexEngine({ forgiving: true }) });
  return core;
}

/** Big files are shown plain: colouring them would cost more than reading them. */
export const MAX_HIGHLIGHT = 400_000;

export async function tokenize(code: string, path: string): Promise<ThemedToken[][] | null> {
  const lang = languageOf(path);
  if (!lang || code.length > MAX_HIGHLIGHT) return null;
  const h = await highlighter();
  if (!loaded.has(lang)) {
    const mod = (await GRAMMARS[lang]!()) as { default: Parameters<HighlighterCore["loadLanguage"]>[0] };
    await h.loadLanguage(mod.default);
    loaded.add(lang);
  }
  return h.codeToTokensBase(code, { lang, theme: "taskmaster" });
}

export type { ThemedToken };
