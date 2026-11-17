// Role marks: one drawn family for the agents of a run, taken from the masonry of the bust in
// the Taskmaster logo. The master is the keystone, the wedge that locks an arch and carries
// the load the other stones share (it locks the contracts and repairs the shared files). A
// worker is a column, one per page; the mid is a column still going up (a half shaft under a
// rising chevron), picking up a page a worker left; the reviewer is a plumb bob that reads
// whether the work is true; the verifier is a try-square with a tick, checking each reported
// problem against a right angle.
// Drawn on a 16px grid with a 1.5 stroke so they stay crisp at 13 to 16px.

import type { ReactElement, SVGProps } from "react";

export type MarkRole = "master" | "worker" | "mid" | "reviewer" | "verifier";

const paths: Record<MarkRole, ReactElement> = {
  master: (
    <>
      <path d="M2 14.5V10a6 6 0 0 1 3.4-5.4M14 14.5V10a6 6 0 0 0-3.4-5.4" />
      <path d="M5.5 1.4h5l-1.9 4.75H7.4z" fill="currentColor" />
    </>
  ),
  worker: <path d="M2.5 2.5h11M4 4.75h8M5.25 4.75v7.75M10.75 4.75v7.75M8 6.5V11M2.5 14.25h11" />,
  mid: <path d="M5.25 8.5v4M10.75 8.5v4M8 9.5V12M2.5 14.25h11M5.25 5.5 8 2.25 10.75 5.5" />,
  reviewer: <path d="M4.5 1.75h7M8 1.75v4M8 14.5 4.75 9.6a3.25 3.25 0 1 1 6.5 0z" />,
  verifier: <path d="M2.25 1.75v12h12v-3h-9v-9zM8.25 5.25l1.5 1.5 3-3.25" />,
};

export function RoleMark({ role, size = 14, ...rest }: { role: MarkRole; size?: number } & SVGProps<SVGSVGElement>) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.5}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
      focusable="false"
      className="role-mark"
      data-role={role}
      {...rest}
    >
      {paths[role]}
    </svg>
  );
}

/** The mark for an agent surface or plan agent id: master, reviewer and verifier by name, any page agent a worker. */
export function markFor(id: string, tier?: string): MarkRole {
  if (id === "master" || id === "reviewer" || id === "verifier") return id;
  return tier === "mid" ? "mid" : "worker";
}
