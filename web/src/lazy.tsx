// Parts that are heavy and not needed for the first screen load when first shown: the
// files panel (Shiki and the diff renderer), terminals (xterm.js) and the Lottie player.

import { lazy, Suspense, type ComponentProps } from "react";

const FilesPanelLazy = lazy(() => import("./files/FilesPanel").then((m) => ({ default: m.FilesPanel })));
const QuickTerminalLazy = lazy(() => import("./terminal/QuickTerminal").then((m) => ({ default: m.QuickTerminal })));
const TranscriptLazy = lazy(() => import("./terminal/Xterm").then((m) => ({ default: m.Transcript })));
export { EmptyRuns } from "./art/EmptyRuns";

function Lines({ n = 6 }: { n?: number }) {
  return (
    <div className="code-skel" role="status" aria-label="Loading">
      {Array.from({ length: n }, (_, i) => (
        <span key={i} className="skeleton" style={{ width: `${40 + ((i * 29) % 45)}%` }} />
      ))}
    </div>
  );
}

export function FilesPanel(props: ComponentProps<typeof FilesPanelLazy>) {
  return (
    <Suspense fallback={<Lines n={10} />}>
      <FilesPanelLazy {...props} />
    </Suspense>
  );
}

export function QuickTerminal(props: ComponentProps<typeof QuickTerminalLazy>) {
  return (
    <Suspense fallback={null}>
      <QuickTerminalLazy {...props} />
    </Suspense>
  );
}

export function Transcript(props: ComponentProps<typeof TranscriptLazy>) {
  return (
    <Suspense fallback={<Lines />}>
      <TranscriptLazy {...props} />
    </Suspense>
  );
}

/** Start loading the files panel early, e.g. when the pointer nears its tab. */
export function preloadFiles() {
  void import("./files/FilesPanel");
}
