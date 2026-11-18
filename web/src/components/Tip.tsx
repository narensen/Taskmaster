// A tooltip on hover and keyboard focus. Short labels and a shortcut, nothing more.

import * as Tooltip from "@radix-ui/react-tooltip";
import type { ReactNode } from "react";

export function TipProvider({ children }: { children: ReactNode }) {
  return (
    <Tooltip.Provider delayDuration={350} skipDelayDuration={150}>
      {children}
    </Tooltip.Provider>
  );
}

export function Tip({ label, keys, children, side = "bottom" }: { label: string; keys?: string; children: ReactNode; side?: "top" | "bottom" | "left" | "right" }) {
  return (
    <Tooltip.Root>
      <Tooltip.Trigger asChild>{children}</Tooltip.Trigger>
      <Tooltip.Portal>
        <Tooltip.Content className="tip" side={side} sideOffset={6} collisionPadding={8}>
          <span>{label}</span>
          {keys && <kbd>{keys}</kbd>}
        </Tooltip.Content>
      </Tooltip.Portal>
    </Tooltip.Root>
  );
}
