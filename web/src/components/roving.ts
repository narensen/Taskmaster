import type { KeyboardEvent } from "react";

/** Arrow keys, Home and End move focus between the `[data-rove]` items of a list. */
export function rove(e: KeyboardEvent<HTMLElement>, axis: "x" | "y" = "y") {
  const next = axis === "y" ? "ArrowDown" : "ArrowRight";
  const prev = axis === "y" ? "ArrowUp" : "ArrowLeft";
  if (![next, prev, "Home", "End"].includes(e.key)) return;
  const items = [...e.currentTarget.querySelectorAll<HTMLElement>("[data-rove]:not([disabled])")];
  if (!items.length) return;
  const at = items.indexOf(document.activeElement as HTMLElement);
  let to = at;
  if (e.key === next) to = at < 0 ? 0 : Math.min(items.length - 1, at + 1);
  else if (e.key === prev) to = at < 0 ? items.length - 1 : Math.max(0, at - 1);
  else if (e.key === "Home") to = 0;
  else to = items.length - 1;
  e.preventDefault();
  items[to]?.focus();
}
