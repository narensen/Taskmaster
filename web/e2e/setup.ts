// Start tm-daemon on a fresh git project and pair once; tests read the address and token.
import { spawn, execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

export const STATE = join(tmpdir(), "taskmaster-e2e.json");

export default async function setup() {
  const root = resolve(here, "../..");
  const daemon = process.env.TM_DAEMON ?? join(root, "target/debug/tm-daemon");
  if (!existsSync(daemon)) throw new Error(`build the daemon first: cargo build -p tm-daemon (looked for ${daemon})`);
  const dist = resolve(here, "../dist");
  if (!existsSync(join(dist, "index.html"))) throw new Error("build the app first: npm run build");
  const base = mkdtempSync(join(tmpdir(), "tm-e2e-"));
  const project = join(base, "project");
  execFileSync("sh", ["-c", `mkdir -p "${project}" && cd "${project}" && git init -q && git config user.email e2e@example.invalid && git config user.name e2e && echo '# e2e' > README.md && git add . && git commit -qm init`]);
  const child = spawn(daemon, ["serve", project, "--bind", "127.0.0.1:0", "--web", dist, "--commons", join(base, "commons"), "--dictionary", join(base, "dictionary")], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, XDG_DATA_HOME: join(base, "data") },
  });
  const line = await new Promise<string>((ok, fail) => {
    let said = "";
    const timer = setTimeout(() => fail(new Error(`tm-daemon did not start: ${said}`)), 20_000);
    const read = (chunk: Buffer) => {
      said += chunk.toString();
      const found = said.split("\n").find((l) => l.includes(" code "));
      if (found) {
        clearTimeout(timer);
        ok(found);
      }
    };
    child.stdout.on("data", read);
    child.stderr.on("data", read);
    child.on("exit", (code) => fail(new Error(`tm-daemon exited with ${code}: ${said}`)));
  });
  const url = /api (\S+)/.exec(line)![1]!.replace(/\/$/, "");
  const code = /code (\S+)/.exec(line)![1]!;
  const paired = await fetch(`${url}/api/pair`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ code }) });
  const { token } = (await paired.json()) as { token: string };
  writeFileSync(STATE, JSON.stringify({ url, token, pid: child.pid, base, project }));
  child.unref();
}
