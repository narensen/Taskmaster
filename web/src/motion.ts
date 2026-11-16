// Motion tokens shared by GSAP and Framer Motion. They mirror --t-* and --ease-* in
// styles.css, so a number changed here should be changed there. Seconds, not milliseconds.

export const dur = { fast: 0.14, base: 0.22, slow: 0.38 } as const;

/** GSAP ease names and the equivalent cubic-bezier for Framer. */
export const ease = {
  out: "power3.out",
  inOut: "power3.inOut",
  spring: "back.out(1.4)",
} as const;

export const bezier = {
  out: [0.22, 1, 0.36, 1],
  inOut: [0.65, 0, 0.35, 1],
} as const;

export const springy = { type: "spring", stiffness: 420, damping: 34, mass: 0.8 } as const;
