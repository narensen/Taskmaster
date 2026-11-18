import { readFileSync, rmSync } from "node:fs";
import { STATE } from "./setup";

export default async function teardown() {
  const state = JSON.parse(readFileSync(STATE, "utf8")) as { pid: number; base: string; fixture: { pid: number } | null };
  for (const pid of [state.pid, state.fixture?.pid]) {
    try {
      if (pid) process.kill(pid);
    } catch {
      // Already gone.
    }
  }
  rmSync(state.base, { recursive: true, force: true });
  rmSync(STATE, { force: true });
}
