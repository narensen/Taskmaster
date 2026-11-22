// Lightweight monochrome arch illustration for the empty gallery — zero WASM/Lottie bundle cost.

export function EmptyRuns() {
  return (
    <div className="empty-art" aria-hidden>
      <svg
        viewBox="0 0 160 120"
        width="160"
        height="120"
        fill="none"
        stroke="currentColor"
        strokeLinecap="round"
        strokeLinejoin="round"
        aria-hidden
      >
        <path d="M30 102h100" strokeWidth="2" />
        <path d="M46 96V58M80 96V50M114 96V58" strokeWidth="3" />
        <path d="M46 58c0-16 12-24 26-24M114 58c0-16-12-24-26-24" strokeWidth="2.5" opacity="0.6" />
        <path d="M72 22h16l-4 12h-8z" fill="currentColor" stroke="none" />
      </svg>
    </div>
  );
}
