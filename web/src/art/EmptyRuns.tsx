// The empty gallery: the hand-made arch animation once, then still. With reduced motion,
// the finished drawing as plain SVG and no player at all.

import { useMemo } from "react";
import { DotLottieReact, setWasmUrl } from "@lottiefiles/dotlottie-react";
import wasm from "dotlottie-player.wasm?url";
import { useMotion, usePrefs } from "../prefs";
import { emptyRunsLottie } from "./emptyRuns";

setWasmUrl(wasm);

function Still() {
  return (
    <svg viewBox="0 0 160 120" width="160" height="120" fill="none" stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d="M30 102h100" strokeWidth="2" />
      <path d="M46 96V58M80 96V50M114 96V58" strokeWidth="3" />
      <path d="M46 58c0-16 12-24 26-24M114 58c0-16-12-24-26-24" strokeWidth="2.5" opacity="0.6" />
      <path d="M72 22h16l-4 12h-8z" fill="currentColor" stroke="none" />
    </svg>
  );
}

export function EmptyRuns() {
  const on = useMotion();
  const { theme } = usePrefs();
  const data = useMemo(() => emptyRunsLottie(theme === "dark" ? "#ededf0" : "#1c1b18"), [theme]);
  return (
    <div className="empty-art" aria-hidden>
      {on ? <DotLottieReact key={theme} data={data} autoplay loop={false} style={{ width: 160, height: 120 }} renderConfig={{ autoResize: true }} /> : <Still />}
    </div>
  );
}
