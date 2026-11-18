// The five roles of a run, with a line on what each does. Choosing a provider for one is
// in components/ProviderPicker.tsx; what is chosen, in rolechoice.ts.

import type { RoleName } from "./api";

export const ROLES: { id: RoleName; label: string; hint: string }[] = [
  { id: "master", label: "Master", hint: "Plans, locks the contracts, repairs shared files." },
  { id: "worker", label: "Worker", hint: "Builds each page first." },
  { id: "mid", label: "Mid", hint: "Takes a page the worker could not finish." },
  { id: "reviewer", label: "Reviewer", hint: "Reads the result and reports problems." },
  { id: "verifier", label: "Verifier", hint: "Checks each reported problem." },
];
