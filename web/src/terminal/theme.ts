// xterm colours from the app's own tokens. Dark is graphite with a monochrome ANSI
// palette: the eight colours are steps of grey, so a coloured prompt still reads as ink.
// Light is paper and ink, with verdigris for the cursor and the selection.

import type { ITheme } from "@xterm/xterm";

function css(name: string, fallback: string): string {
  const root = document.querySelector(".shell") ?? document.documentElement;
  return getComputedStyle(root).getPropertyValue(name).trim() || fallback;
}

export function terminalTheme(theme: "dark" | "light"): ITheme {
  const bg = css("--term-bg", theme === "dark" ? "#09090a" : "#f6f3ec");
  const ink = css("--ink", theme === "dark" ? "#ededf0" : "#1c1b18");
  if (theme === "dark") {
    return {
      background: bg,
      foreground: ink,
      cursor: "#ededf0",
      cursorAccent: bg,
      selectionBackground: "#ffffff2e",
      selectionInactiveBackground: "#ffffff18",
      black: "#3a3a40",
      red: "#d4d4da",
      green: "#bdbdc4",
      yellow: "#e4e4e8",
      blue: "#a6a6ae",
      magenta: "#cbcbd1",
      cyan: "#b4b4bb",
      white: "#dcdce2",
      brightBlack: "#6a6a72",
      brightRed: "#f2f2f5",
      brightGreen: "#d8d8de",
      brightYellow: "#ffffff",
      brightBlue: "#c4c4cb",
      brightMagenta: "#e6e6ea",
      brightCyan: "#d0d0d6",
      brightWhite: "#ffffff",
    };
  }
  return {
    background: bg,
    foreground: ink,
    cursor: css("--accent", "#0b6b62"),
    cursorAccent: bg,
    selectionBackground: "#0b6b6233",
    selectionInactiveBackground: "#0b6b621c",
    black: "#1c1b18",
    red: "#8a2a25",
    green: "#2f5d3a",
    yellow: "#6b5313",
    blue: "#0b6b62",
    magenta: "#5a4a5e",
    cyan: "#24605a",
    white: "#8a8478",
    brightBlack: "#605b51",
    brightRed: "#b3302b",
    brightGreen: "#1e6c34",
    brightYellow: "#7a590a",
    brightBlue: "#0e7d72",
    brightMagenta: "#6e5a72",
    brightCyan: "#2f7a72",
    brightWhite: "#3d3a34",
  };
}
