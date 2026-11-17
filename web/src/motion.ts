// Motion tokens shared by GSAP and Motion. Short and ease-out: 120–220ms.
// Seconds, not milliseconds. A number changed here should stay in this range.

export const dur = { fast: 0.12, base: 0.18, slow: 0.22 } as const;

/** GSAP ease names and the equivalent cubic-bezier for Motion. */
export const ease = {
  out: "power2.out",
  inOut: "power2.inOut",
} as const;

export const bezier = {
  out: [0.22, 1, 0.36, 1] as [number, number, number, number],
  inOut: [0.4, 0, 0.2, 1] as [number, number, number, number],
};
