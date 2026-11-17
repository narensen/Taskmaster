import { readFileSync, rmSync } from "node:fs";
import { STATE } from "./setup";

export default async function teardown() {
  const state = JSON.parse(readFileSync(STATE, "utf8")) as { pid: number; base: string };
  try {
    process.kill(state.pid);
  } catch {
    // Already gone.
  }
  rmSync(state.base, { recursive: true, force: true });
  rmSync(STATE, { force: true });
}
